//! HomeKit mapping: builds a `HeaterCooler` accessory per Daikin unit, wires
//! write callbacks (HomeKit -> device) and runs the poll loop (device ->
//! HomeKit), keeping the two in sync without feedback loops.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use futures::future::FutureExt;
use hap::{
    accessory::{heater_cooler::HeaterCoolerAccessory, AccessoryInformation},
    characteristic::{AsyncCharacteristicCallbacks, HapCharacteristic},
    serde_json::json,
    HapType,
};

/// Pointer to a live accessory managed by the HAP server. Mirrors the crate's
/// private `hap::pointer::Accessory` alias (the underlying types are public).
pub type AccessoryPtr =
    std::sync::Arc<hap::futures::lock::Mutex<Box<dyn hap::accessory::HapAccessory>>>;
use serde::Serialize;
use tokio::task::JoinHandle;

use crate::activity::Activity;
use crate::config::DeviceConfig;
use crate::daikin::{mode as dmode, Command, DaikinClient};

// HomeKit `TargetHeaterCoolerState` values.
const HK_TARGET_AUTO: u8 = 0;
const HK_TARGET_HEAT: u8 = 1;
const HK_TARGET_COOL: u8 = 2;

// HomeKit `CurrentHeaterCoolerState` values.
const HK_CURRENT_INACTIVE: u8 = 0;
const HK_CURRENT_IDLE: u8 = 1;
const HK_CURRENT_HEATING: u8 = 2;
const HK_CURRENT_COOLING: u8 = 3;

// Threshold temperature bounds (Celsius) exposed to HomeKit.
const HEAT_MIN: f32 = 10.0;
const HEAT_MAX: f32 = 30.0;
const COOL_MIN: f32 = 16.0;
const COOL_MAX: f32 = 32.0;

/// Fan-rate tokens ordered from lowest to highest airflow, mapped linearly onto
/// the HomeKit 0-100% rotation speed slider. `B` = silent, `3`..`7` = levels
/// 1-5, `A` = automatic (top of the range).
const FAN_ORDER: [&str; 7] = ["B", "3", "4", "5", "6", "7", "A"];

/// Per-device shared state used by both the poll loop and the write callbacks.
pub struct DeviceRuntime {
    pub name: String,
    pub client: DaikinClient,
    /// Set while the poll loop pushes device values into HomeKit, so the
    /// resulting `on_update` callbacks don't echo those values back to the unit.
    suppress: AtomicBool,
    /// Serializes writes to the unit (each is a read-modify-write cycle).
    write_lock: tokio::sync::Mutex<()>,
    pub status: Mutex<DeviceStatus>,
    pub activity: Activity,
}

/// Snapshot of a unit's state for the admin web UI.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DeviceStatus {
    pub reachable: bool,
    pub power: bool,
    pub mode: String,
    pub target_temp: Option<f32>,
    pub indoor_temp: Option<f32>,
    pub outdoor_temp: Option<f32>,
    pub fan: String,
    pub swing: bool,
    pub last_seen: Option<String>,
    pub last_error: Option<String>,
}

impl DeviceRuntime {
    pub fn new(cfg: &DeviceConfig, activity: Activity) -> anyhow::Result<Self> {
        Ok(Self {
            name: cfg.name.clone(),
            client: DaikinClient::new(&cfg.ip)?,
            suppress: AtomicBool::new(false),
            write_lock: tokio::sync::Mutex::new(()),
            status: Mutex::new(DeviceStatus::default()),
            activity,
        })
    }
}

/// Boxes any error into the type HomeKit callbacks require.
fn boxed(e: impl std::fmt::Display) -> Box<dyn std::error::Error + Send + Sync> {
    format!("{e:#}").into()
}

/// Builds a fully-configured `HeaterCoolerAccessory` with write callbacks bound
/// to `rt`.
pub fn build_accessory(
    cfg: &DeviceConfig,
    rt: std::sync::Arc<DeviceRuntime>,
) -> anyhow::Result<HeaterCoolerAccessory> {
    let mut acc = HeaterCoolerAccessory::new(
        cfg.id,
        AccessoryInformation {
            manufacturer: "Daikin".into(),
            model: "BRP069B".into(),
            name: cfg.name.clone(),
            serial_number: format!("daikin-{}", cfg.id),
            ..Default::default()
        },
    )?;

    let hc = &mut acc.heater_cooler;

    // Drop the child-lock control we don't model; keep display units.
    hc.lock_physical_controls = None;

    // Name the service so it reads nicely in the Home app.
    if let Some(name) = hc.name.as_mut() {
        name.set_value(json!(cfg.name)).now_or_never();
    }

    // Constrain threshold ranges to what the units actually accept.
    if let Some(c) = hc.heating_threshold_temperature.as_mut() {
        let _ = c.set_min_value(Some(json!(HEAT_MIN)));
        let _ = c.set_max_value(Some(json!(HEAT_MAX)));
        let _ = c.set_step_value(Some(json!(0.5f32)));
    }
    if let Some(c) = hc.cooling_threshold_temperature.as_mut() {
        let _ = c.set_min_value(Some(json!(COOL_MIN)));
        let _ = c.set_max_value(Some(json!(COOL_MAX)));
        let _ = c.set_step_value(Some(json!(0.5f32)));
    }

    // --- Write callbacks: HomeKit -> device ---

    let r = rt.clone();
    hc.active.on_update_async(Some(move |_c: u8, new: u8| {
        let rt = r.clone();
        async move {
            if rt.suppress.load(Ordering::SeqCst) {
                return Ok(());
            }
            let on = new == 1;
            let _g = rt.write_lock.lock().await;
            rt.client.apply(Command::Power(on)).await.map_err(boxed)?;
            rt.activity.info(
                Some(&rt.name),
                format!("HomeKit: power {}", if on { "on" } else { "off" }),
            );
            Ok(())
        }
        .boxed()
    }));

    let r = rt.clone();
    hc.target_heater_cooler_state
        .on_update_async(Some(move |_c: u8, new: u8| {
            let rt = r.clone();
            async move {
                if rt.suppress.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let mode = hk_target_to_daikin_mode(new);
                let _g = rt.write_lock.lock().await;
                rt.client.apply(Command::Mode(mode)).await.map_err(boxed)?;
                rt.activity.info(
                    Some(&rt.name),
                    format!("HomeKit: mode {}", daikin_mode_name(mode)),
                );
                Ok(())
            }
            .boxed()
        }));

    if let Some(c) = hc.heating_threshold_temperature.as_mut() {
        let r = rt.clone();
        c.on_update_async(Some(move |_c: f32, new: f32| {
            let rt = r.clone();
            async move {
                if rt.suppress.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let t = new.clamp(HEAT_MIN, HEAT_MAX);
                let _g = rt.write_lock.lock().await;
                rt.client
                    .apply(Command::Temperature(t))
                    .await
                    .map_err(boxed)?;
                rt.activity.info(
                    Some(&rt.name),
                    format!("HomeKit: heating setpoint {t:.1}°C"),
                );
                Ok(())
            }
            .boxed()
        }));
    }

    if let Some(c) = hc.cooling_threshold_temperature.as_mut() {
        let r = rt.clone();
        c.on_update_async(Some(move |_c: f32, new: f32| {
            let rt = r.clone();
            async move {
                if rt.suppress.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let t = new.clamp(COOL_MIN, COOL_MAX);
                let _g = rt.write_lock.lock().await;
                rt.client
                    .apply(Command::Temperature(t))
                    .await
                    .map_err(boxed)?;
                rt.activity.info(
                    Some(&rt.name),
                    format!("HomeKit: cooling setpoint {t:.1}°C"),
                );
                Ok(())
            }
            .boxed()
        }));
    }

    if let Some(c) = hc.rotation_speed.as_mut() {
        let r = rt.clone();
        c.on_update_async(Some(move |_c: f32, new: f32| {
            let rt = r.clone();
            async move {
                if rt.suppress.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let rate = percent_to_fan_rate(new);
                let _g = rt.write_lock.lock().await;
                rt.client
                    .apply(Command::FanRate(rate.clone()))
                    .await
                    .map_err(boxed)?;
                rt.activity
                    .info(Some(&rt.name), format!("HomeKit: fan rate {rate}"));
                Ok(())
            }
            .boxed()
        }));
    }

    if let Some(c) = hc.swing_mode.as_mut() {
        let r = rt.clone();
        c.on_update_async(Some(move |_c: u8, new: u8| {
            let rt = r.clone();
            async move {
                if rt.suppress.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let dir = if new == 1 { 3 } else { 0 };
                let _g = rt.write_lock.lock().await;
                rt.client.apply(Command::FanDir(dir)).await.map_err(boxed)?;
                rt.activity.info(
                    Some(&rt.name),
                    format!("HomeKit: swing {}", if new == 1 { "on" } else { "off" }),
                );
                Ok(())
            }
            .boxed()
        }));
    }

    Ok(acc)
}

/// Spawns the poll loop that refreshes HomeKit from the device.
pub fn spawn_poll(
    ptr: AccessoryPtr,
    rt: std::sync::Arc<DeviceRuntime>,
    period: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            refresh(&ptr, &rt).await;
        }
    })
}

/// Performs a single poll + HomeKit refresh cycle. Public within the crate so a
/// web-UI command can immediately re-sync after writing to the unit.
pub(crate) async fn refresh(ptr: &AccessoryPtr, rt: &DeviceRuntime) {
    let control = match rt.client.control_info().await {
        Ok(c) => c,
        Err(e) => {
            mark_unreachable(rt, &format!("{e:#}"));
            return;
        }
    };
    let sensor = rt.client.sensor_info().await.ok();

    let power = control.power();
    let dmode_val = control.mode();
    let target_temp = control.target_temp();
    let indoor = sensor.as_ref().and_then(|s| s.indoor_temp());
    let outdoor = sensor.as_ref().and_then(|s| s.outdoor_temp());
    let fan_rate = control.fan_rate();
    let swing = control.fan_dir() != 0;

    let hk_active: u8 = if power { 1 } else { 0 };
    let hk_target = daikin_mode_to_hk_target(dmode_val);
    let hk_current = current_state(power, dmode_val, indoor, target_temp);
    let hk_speed = fan_rate_to_percent(&fan_rate);
    let hk_swing: u8 = if swing { 1 } else { 0 };
    // HomeKit always wants a current temperature; fall back to the setpoint.
    let hk_temp = indoor.or(target_temp).unwrap_or(20.0);
    let heat_setpoint = target_temp.unwrap_or(20.0).clamp(HEAT_MIN, HEAT_MAX);
    let cool_setpoint = target_temp.unwrap_or(24.0).clamp(COOL_MIN, COOL_MAX);

    // Push values into HomeKit with callbacks suppressed.
    rt.suppress.store(true, Ordering::SeqCst);
    {
        let mut acc = ptr.lock().await;
        if let Some(svc) = acc.get_mut_service(HapType::HeaterCooler) {
            set_char(svc, HapType::Active, json!(hk_active)).await;
            set_char(svc, HapType::CurrentHeaterCoolerState, json!(hk_current)).await;
            set_char(svc, HapType::TargetHeaterCoolerState, json!(hk_target)).await;
            set_char(svc, HapType::CurrentTemperature, json!(hk_temp)).await;
            set_char(
                svc,
                HapType::HeatingThresholdTemperature,
                json!(heat_setpoint),
            )
            .await;
            set_char(
                svc,
                HapType::CoolingThresholdTemperature,
                json!(cool_setpoint),
            )
            .await;
            set_char(svc, HapType::RotationSpeed, json!(hk_speed)).await;
            set_char(svc, HapType::SwingMode, json!(hk_swing)).await;
        }
        // Re-enable callbacks while still holding the accessory lock so no
        // controller write can slip through while suppression is on.
        rt.suppress.store(false, Ordering::SeqCst);
    }

    update_status(rt, |st| {
        let was_reachable = st.reachable;
        let prev = st.clone();
        st.reachable = true;
        st.power = power;
        st.mode = daikin_mode_name(dmode_val).to_string();
        st.target_temp = target_temp;
        st.indoor_temp = indoor;
        st.outdoor_temp = outdoor;
        st.fan = fan_rate.clone();
        st.swing = swing;
        st.last_error = None;
        st.last_seen = Some(now_rfc3339());

        if !was_reachable {
            rt.activity.info(Some(&rt.name), "device reachable");
        }
        if prev.power != power
            || prev.mode != st.mode
            || prev.target_temp != target_temp
            || prev.fan != fan_rate
            || prev.swing != swing
        {
            rt.activity.info(
                Some(&rt.name),
                format!(
                    "state: {} {} setpoint {} fan {}{}",
                    if power { "on" } else { "off" },
                    st.mode,
                    target_temp
                        .map(|t| format!("{t:.1}°C"))
                        .unwrap_or_else(|| "-".into()),
                    fan_rate,
                    if swing { " swing" } else { "" },
                ),
            );
        }
    });
}

async fn set_char(
    svc: &mut dyn hap::service::HapService,
    ty: HapType,
    value: hap::serde_json::Value,
) {
    if let Some(c) = svc.get_mut_characteristic(ty) {
        if let Err(e) = c.set_value(value).await {
            tracing::debug!("failed to set {:?}: {:?}", ty, e);
        }
    }
}

fn mark_unreachable(rt: &DeviceRuntime, err: &str) {
    update_status(rt, |st| {
        if st.reachable {
            rt.activity
                .warn(Some(&rt.name), format!("device unreachable: {err}"));
        }
        st.reachable = false;
        st.last_error = Some(err.to_string());
    });
}

fn update_status(rt: &DeviceRuntime, f: impl FnOnce(&mut DeviceStatus)) {
    let mut st = rt.status.lock().unwrap();
    f(&mut st);
}

fn now_rfc3339() -> String {
    use time::{format_description::well_known::Rfc3339, OffsetDateTime};
    OffsetDateTime::now_local()
        .unwrap_or_else(|_| OffsetDateTime::now_utc())
        .format(&Rfc3339)
        .unwrap_or_default()
}

// --- Pure mapping helpers ---

pub fn daikin_mode_to_hk_target(mode: u8) -> u8 {
    match mode {
        dmode::HEAT => HK_TARGET_HEAT,
        dmode::COOL => HK_TARGET_COOL,
        // Auto, dry and fan have no distinct HomeKit target; present as Auto.
        _ => HK_TARGET_AUTO,
    }
}

pub fn hk_target_to_daikin_mode(target: u8) -> u8 {
    match target {
        HK_TARGET_HEAT => dmode::HEAT,
        HK_TARGET_COOL => dmode::COOL,
        _ => dmode::AUTO,
    }
}

/// Maps a UI mode name (`auto`/`heat`/`cool`) to a Daikin mode value.
pub fn mode_name_to_daikin(name: &str) -> Option<u8> {
    match name.to_ascii_lowercase().as_str() {
        "auto" => Some(dmode::AUTO),
        "heat" => Some(dmode::HEAT),
        "cool" => Some(dmode::COOL),
        "dry" => Some(dmode::DRY),
        "fan" => Some(dmode::FAN),
        _ => None,
    }
}

pub fn daikin_mode_name(mode: u8) -> &'static str {
    match mode {
        dmode::AUTO | dmode::AUTO1 | dmode::AUTO7 => "auto",
        dmode::DRY => "dry",
        dmode::COOL => "cool",
        dmode::HEAT => "heat",
        dmode::FAN => "fan",
        _ => "unknown",
    }
}

fn current_state(power: bool, mode: u8, indoor: Option<f32>, target: Option<f32>) -> u8 {
    if !power {
        return HK_CURRENT_INACTIVE;
    }
    match mode {
        dmode::COOL => HK_CURRENT_COOLING,
        dmode::HEAT => HK_CURRENT_HEATING,
        _ => match (indoor, target) {
            (Some(i), Some(t)) if i > t + 0.5 => HK_CURRENT_COOLING,
            (Some(i), Some(t)) if i < t - 0.5 => HK_CURRENT_HEATING,
            _ => HK_CURRENT_IDLE,
        },
    }
}

pub fn fan_rate_to_percent(rate: &str) -> f32 {
    let idx = FAN_ORDER
        .iter()
        .position(|r| *r == rate)
        .unwrap_or(FAN_ORDER.len() - 1);
    (((idx + 1) as f32 / FAN_ORDER.len() as f32) * 100.0).round()
}

pub fn percent_to_fan_rate(pct: f32) -> String {
    let len = FAN_ORDER.len() as f32;
    let idx = ((pct / 100.0 * len).round() as i32 - 1).clamp(0, FAN_ORDER.len() as i32 - 1);
    FAN_ORDER[idx as usize].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_roundtrip() {
        assert_eq!(daikin_mode_to_hk_target(dmode::HEAT), HK_TARGET_HEAT);
        assert_eq!(daikin_mode_to_hk_target(dmode::COOL), HK_TARGET_COOL);
        assert_eq!(daikin_mode_to_hk_target(dmode::DRY), HK_TARGET_AUTO);
        assert_eq!(hk_target_to_daikin_mode(HK_TARGET_HEAT), dmode::HEAT);
        assert_eq!(hk_target_to_daikin_mode(HK_TARGET_COOL), dmode::COOL);
    }

    #[test]
    fn fan_mapping_roundtrips_within_a_step() {
        for r in FAN_ORDER {
            let pct = fan_rate_to_percent(r);
            assert_eq!(percent_to_fan_rate(pct), r, "rate {r} at {pct}%");
        }
    }

    #[test]
    fn fan_extremes() {
        assert_eq!(percent_to_fan_rate(0.0), "B");
        assert_eq!(percent_to_fan_rate(100.0), "A");
    }
}
