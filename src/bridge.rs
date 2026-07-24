//! Runtime manager coordinating the HAP server, the persisted config and the
//! set of live Daikin accessories. Device add/edit/remove requests from the
//! web UI are applied to the running HomeKit bridge and persisted to disk.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use hap::server::{IpServer, Server};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::activity::Activity;
use crate::config::{AppConfig, DeviceConfig};
use crate::daikin::{Command, DaikinClient};
use crate::hk::{
    build_accessory, mode_name_to_daikin, refresh, spawn_poll, AccessoryPtr, DeviceRuntime,
    DeviceStatus,
};

/// A live device: its config, HomeKit accessory pointer, poll task and runtime.
struct DeviceEntry {
    cfg: DeviceConfig,
    ptr: AccessoryPtr,
    poll: JoinHandle<()>,
    rt: Arc<DeviceRuntime>,
}

struct State {
    cfg: AppConfig,
    devices: HashMap<u64, DeviceEntry>,
}

pub struct Manager {
    server: IpServer,
    activity: Activity,
    config_path: PathBuf,
    state: tokio::sync::Mutex<State>,
}

/// Bridge-level info for the web UI header.
#[derive(Debug, Clone, Serialize)]
pub struct BridgeInfo {
    pub name: String,
    pub pin: String,
    pub hap_port: u16,
    pub web_port: u16,
    pub device_count: usize,
}

/// Formats an 8-digit PIN as `123-45-678` for display.
fn format_pin(pin: &str) -> String {
    let digits: String = pin.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() == 8 {
        format!("{}-{}-{}", &digits[0..3], &digits[3..5], &digits[5..8])
    } else {
        digits
    }
}

/// A control request from the web UI. Any subset of fields may be present; they
/// are applied to the unit in order.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeviceCommand {
    pub power: Option<bool>,
    pub mode: Option<String>,
    pub temperature: Option<f32>,
    pub fan: Option<String>,
    pub swing: Option<bool>,
}

/// A device as presented to the web UI.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceView {
    pub id: u64,
    pub name: String,
    pub ip: String,
    #[serde(flatten)]
    pub status: DeviceStatus,
}

impl Manager {
    pub fn new(server: IpServer, activity: Activity, config_path: PathBuf, cfg: AppConfig) -> Self {
        Self {
            server,
            activity,
            config_path,
            state: tokio::sync::Mutex::new(State {
                cfg,
                devices: HashMap::new(),
            }),
        }
    }

    /// Registers all configured devices with the running server.
    pub async fn init_devices(&self) -> Result<()> {
        let mut state = self.state.lock().await;
        let poll_secs = state.cfg.poll_secs;
        let configs: Vec<DeviceConfig> = state.cfg.devices.clone();
        for cfg in configs {
            match self.spawn_device(&cfg, poll_secs).await {
                Ok(entry) => {
                    state.devices.insert(cfg.id, entry);
                }
                Err(e) => {
                    self.activity
                        .error(Some(&cfg.name), format!("failed to register: {e:#}"));
                }
            }
        }
        Ok(())
    }

    /// Builds an accessory, adds it to the server and starts its poll loop.
    async fn spawn_device(&self, cfg: &DeviceConfig, poll_secs: u64) -> Result<DeviceEntry> {
        let rt = Arc::new(DeviceRuntime::new(cfg, self.activity.clone())?);
        let acc = build_accessory(cfg, rt.clone())?;
        let ptr = self.server.add_accessory(acc).await?;
        let poll = spawn_poll(ptr.clone(), rt.clone(), Duration::from_secs(poll_secs));
        self.activity
            .info(Some(&cfg.name), format!("registered ({})", cfg.ip));
        Ok(DeviceEntry {
            cfg: cfg.clone(),
            ptr,
            poll,
            rt,
        })
    }

    /// Bridge-level info (name, pairing PIN, ports) for the web UI header.
    pub async fn info(&self) -> BridgeInfo {
        let state = self.state.lock().await;
        BridgeInfo {
            name: state.cfg.name.clone(),
            pin: format_pin(&state.cfg.pin),
            hap_port: state.cfg.hap_port,
            web_port: state.cfg.web_port,
            device_count: state.devices.len(),
        }
    }

    /// Snapshot of all devices with their latest status, for the web UI.
    pub async fn list(&self) -> Vec<DeviceView> {
        let state = self.state.lock().await;
        let mut views: Vec<DeviceView> = state
            .devices
            .values()
            .map(|e| DeviceView {
                id: e.cfg.id,
                name: e.cfg.name.clone(),
                ip: e.cfg.ip.clone(),
                status: e.rt.status.lock().unwrap().clone(),
            })
            .collect();
        views.sort_by_key(|v| v.id);
        views
    }

    /// Applies a control command to a device (from the web UI), then re-syncs
    /// HomeKit and the cached status immediately.
    pub async fn command(&self, id: u64, cmd: DeviceCommand) -> Result<()> {
        let (rt, ptr, name) = {
            let state = self.state.lock().await;
            let entry = state
                .devices
                .get(&id)
                .ok_or_else(|| anyhow!("no device with id {id}"))?;
            (entry.rt.clone(), entry.ptr.clone(), entry.cfg.name.clone())
        };

        let mut commands = Vec::new();
        let mut summary = Vec::new();
        if let Some(p) = cmd.power {
            commands.push(Command::Power(p));
            summary.push(format!("power {}", if p { "on" } else { "off" }));
        }
        if let Some(m) = cmd.mode {
            let dm = mode_name_to_daikin(&m).ok_or_else(|| anyhow!("unknown mode '{m}'"))?;
            commands.push(Command::Mode(dm));
            summary.push(format!("mode {m}"));
        }
        if let Some(t) = cmd.temperature {
            let t = t.clamp(10.0, 32.0);
            commands.push(Command::Temperature(t));
            summary.push(format!("setpoint {t:.1}°C"));
        }
        if let Some(f) = cmd.fan {
            summary.push(format!("fan {f}"));
            commands.push(Command::FanRate(f));
        }
        if let Some(s) = cmd.swing {
            commands.push(Command::FanDir(if s { 3 } else { 0 }));
            summary.push(format!("swing {}", if s { "on" } else { "off" }));
        }
        if commands.is_empty() {
            return Err(anyhow!("no command fields provided"));
        }

        for c in commands {
            rt.client
                .apply(c)
                .await
                .with_context(|| format!("applying command to {name}"))?;
        }
        self.activity
            .info(Some(&name), format!("web UI: {}", summary.join(", ")));

        // Reflect the change in HomeKit and the status snapshot right away.
        refresh(&ptr, &rt).await;
        Ok(())
    }

    /// Probes an IP for a valid Daikin unit, returning its configured name.
    pub async fn probe(ip: &str) -> Result<Option<String>> {
        let client = DaikinClient::new(ip)?;
        client
            .device_name()
            .await
            .with_context(|| format!("no Daikin unit responded at {ip}"))
    }

    /// Adds a new device: validates the IP, persists config and registers it.
    pub async fn add_device(&self, ip: &str, name: Option<String>) -> Result<DeviceView> {
        let probed_name = Self::probe(ip).await?;
        let mut state = self.state.lock().await;

        if state.devices.values().any(|e| e.cfg.ip == ip) {
            return Err(anyhow!("a device with IP {ip} already exists"));
        }

        let id = state.cfg.next_device_id();
        let name = name
            .filter(|n| !n.trim().is_empty())
            .or(probed_name)
            .unwrap_or_else(|| format!("Daikin {id}"));
        let cfg = DeviceConfig {
            id,
            name,
            ip: ip.to_string(),
        };
        let poll_secs = state.cfg.poll_secs;

        let entry = self.spawn_device(&cfg, poll_secs).await?;
        state.cfg.devices.push(cfg.clone());
        state.devices.insert(id, entry);
        self.persist(&state)?;

        self.activity.info(Some(&cfg.name), "added via web UI");
        Ok(DeviceView {
            id,
            name: cfg.name,
            ip: cfg.ip,
            status: DeviceStatus::default(),
        })
    }

    /// Edits an existing device's IP and/or name (re-registers it in place).
    pub async fn edit_device(
        &self,
        id: u64,
        ip: Option<String>,
        name: Option<String>,
    ) -> Result<DeviceView> {
        let mut state = self.state.lock().await;
        let existing = state
            .devices
            .get(&id)
            .map(|e| e.cfg.clone())
            .ok_or_else(|| anyhow!("no device with id {id}"))?;

        let new_ip = ip.unwrap_or_else(|| existing.ip.clone());
        let new_name = name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| existing.name.clone());

        if new_ip != existing.ip {
            Self::probe(&new_ip).await?;
        }

        let new_cfg = DeviceConfig {
            id,
            name: new_name,
            ip: new_ip,
        };
        let poll_secs = state.cfg.poll_secs;

        // Remove the old accessory, then add the updated one under the same id.
        if let Some(entry) = state.devices.remove(&id) {
            entry.poll.abort();
            self.server.remove_accessory(&entry.ptr).await.ok();
        }
        let entry = self.spawn_device(&new_cfg, poll_secs).await?;
        state.devices.insert(id, entry);
        if let Some(d) = state.cfg.devices.iter_mut().find(|d| d.id == id) {
            *d = new_cfg.clone();
        }
        self.persist(&state)?;

        self.activity
            .info(Some(&new_cfg.name), "updated via web UI");
        Ok(DeviceView {
            id,
            name: new_cfg.name,
            ip: new_cfg.ip,
            status: DeviceStatus::default(),
        })
    }

    /// Removes a device from the running bridge and the persisted config.
    pub async fn remove_device(&self, id: u64) -> Result<()> {
        let mut state = self.state.lock().await;
        let entry = state
            .devices
            .remove(&id)
            .ok_or_else(|| anyhow!("no device with id {id}"))?;
        entry.poll.abort();
        self.server.remove_accessory(&entry.ptr).await.ok();
        state.cfg.devices.retain(|d| d.id != id);
        self.persist(&state)?;
        self.activity
            .info(Some(&entry.cfg.name), "removed via web UI");
        Ok(())
    }

    fn persist(&self, state: &State) -> Result<()> {
        state.cfg.save(&self.config_path)
    }
}
