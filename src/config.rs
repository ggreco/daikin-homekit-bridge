//! Application configuration, persisted to `config.toml`.
//!
//! The device list is owned by this file but is rewritten by the app whenever
//! devices are changed through the admin web UI, so the web UI is the source of
//! truth for devices at runtime.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

fn default_hap_port() -> u16 {
    32000
}
fn default_web_port() -> u16 {
    8090
}
fn default_pin() -> String {
    "11122333".into()
}
fn default_name() -> String {
    "Daikin Bridge".into()
}
fn default_storage_dir() -> PathBuf {
    PathBuf::from("./data")
}
fn default_poll_secs() -> u64 {
    10
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// 8-digit HomeKit pairing PIN (digits only; formatting added when shown).
    #[serde(default = "default_pin")]
    pub pin: String,
    /// Display name of the HomeKit bridge.
    #[serde(default = "default_name")]
    pub name: String,
    /// TCP port for the HAP server.
    #[serde(default = "default_hap_port")]
    pub hap_port: u16,
    /// TCP port for the admin web UI.
    #[serde(default = "default_web_port")]
    pub web_port: u16,
    /// IP address to advertise and bind the HAP server on. When unset, the
    /// bridge auto-detects the primary IPv4 address of the machine (preferring
    /// the interface used to reach the LAN). Set this to pin a specific address.
    #[serde(default)]
    pub host: Option<String>,
    /// Directory where HAP pairing state is persisted.
    #[serde(default = "default_storage_dir")]
    pub storage_dir: PathBuf,
    /// How often (seconds) to poll each unit.
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
    /// Managed Daikin units.
    #[serde(default, rename = "device")]
    pub devices: Vec<DeviceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    /// Stable HomeKit accessory ID (aid). Must be unique and >= 2 (aid 1 is the
    /// bridge). Kept stable so HomeKit room/pairing assignments survive edits.
    pub id: u64,
    /// Display name shown in the Home app.
    pub name: String,
    /// IPv4 address of the unit on the LAN.
    pub ip: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            pin: default_pin(),
            name: default_name(),
            hap_port: default_hap_port(),
            web_port: default_web_port(),
            host: None,
            storage_dir: default_storage_dir(),
            poll_secs: default_poll_secs(),
            devices: vec![
                DeviceConfig {
                    id: 2,
                    name: "Salone".into(),
                    ip: "10.0.0.142".into(),
                },
                DeviceConfig {
                    id: 3,
                    name: "Home Theater".into(),
                    ip: "10.0.0.247".into(),
                },
            ],
        }
    }
}

impl AppConfig {
    /// Loads config from `path`, creating it with defaults if missing.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let cfg: AppConfig =
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            Ok(cfg)
        } else {
            let cfg = AppConfig::default();
            cfg.save(path)?;
            Ok(cfg)
        }
    }

    /// Persists the config to `path`.
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).context("serializing config")?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Returns the smallest unused accessory ID (>= 2).
    pub fn next_device_id(&self) -> u64 {
        let mut id = 2;
        while self.devices.iter().any(|d| d.id == id) {
            id += 1;
        }
        id
    }

    /// Parses the PIN string into the `[u8; 8]` HAP expects.
    pub fn pin_digits(&self) -> Result<[u8; 8]> {
        let digits: Vec<u8> = self
            .pin
            .chars()
            .filter(|c| c.is_ascii_digit())
            .map(|c| c as u8 - b'0')
            .collect();
        let arr: [u8; 8] = digits.as_slice().try_into().map_err(|_| {
            anyhow::anyhow!("PIN must contain exactly 8 digits, got {:?}", self.pin)
        })?;
        Ok(arr)
    }
}
