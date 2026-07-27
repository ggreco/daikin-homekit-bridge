//! Daikin HomeKit Bridge.
//!
//! Exposes classic-API Daikin air conditioners to Apple HomeKit as HeaterCooler
//! accessories, and serves a small LAN-only admin web UI for managing devices
//! and viewing activity.

mod activity;
mod bridge;
mod config;
mod daikin;
mod hk;
mod web;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use hap::{
    accessory::{bridge::BridgeAccessory, AccessoryCategory, AccessoryInformation},
    server::{IpServer, Server},
    storage::{FileStorage, Storage},
    Config, Pin,
};

use crate::activity::Activity;
use crate::bridge::Manager;
use crate::config::AppConfig;
use crate::web::WebState;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config_path = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "config.toml".into()),
    );
    let app_cfg = AppConfig::load_or_create(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    std::fs::create_dir_all(&app_cfg.storage_dir)
        .with_context(|| format!("creating storage dir {}", app_cfg.storage_dir.display()))?;

    let activity = Activity::new();
    activity.info(
        None,
        format!("starting Daikin HomeKit Bridge \"{}\"", app_cfg.name),
    );

    // Build (or reload) the HAP server config, applying settings from our
    // config.toml while preserving the persisted device id and keypair.
    let pin_digits = app_cfg.pin_digits()?;
    let mut storage = FileStorage::new(&app_cfg.storage_dir)
        .await
        .context("opening HAP storage")?;

    // Prefer an explicit host, else the IPv4 used to reach the units (HomeKit
    // and the classic Daikin API both live on the IPv4 LAN). `hap`'s own
    // auto-detection may otherwise pick an IPv6 address.
    let host = resolve_host(&app_cfg);

    // A pre-existing config.json holds the bridge's HomeKit identity (device id
    // + Ed25519 keypair). If it's present but unreadable we must never fall
    // through to generating a fresh one: that silently invalidates every stored
    // pairing and leaves the Home app stuck on "No Response".
    let config_json = app_cfg.storage_dir.join("config.json");
    let config_existed = config_json.exists();

    let hap_config = match storage.load_config().await {
        Ok(mut c) => {
            c.host = host;
            c.pin = Pin::new(pin_digits)?;
            c.name = app_cfg.name.clone();
            c.port = app_cfg.hap_port;
            c.category = AccessoryCategory::Bridge;
            // Bump the HAP configuration number on every start so controllers
            // re-read the accessory database. The accessory layout can change
            // across upgrades (e.g. added characteristics) or when devices are
            // added/removed via the web UI, and controllers only re-enumerate
            // when this number increments in the mDNS TXT record.
            c.configuration_number = c.configuration_number.wrapping_add(1);
            storage.save_config(&c).await?;
            c
        }
        Err(e) if config_existed => {
            anyhow::bail!(
                "{} exists but could not be loaded ({e}). Refusing to start: generating a new \
                 identity here would invalidate the existing pairings in {}/pairings and every \
                 controller would show \"No Response\". Restore the file from a backup, or delete \
                 it to start fresh and re-pair the bridge in the Home app.",
                config_json.display(),
                app_cfg.storage_dir.display(),
            );
        }
        Err(_) => {
            let c = Config {
                host,
                pin: Pin::new(pin_digits)?,
                name: app_cfg.name.clone(),
                port: app_cfg.hap_port,
                category: AccessoryCategory::Bridge,
                ..Default::default()
            };
            storage.save_config(&c).await?;
            activity.info(None, "no existing HomeKit identity, generated a new one");
            c
        }
    };

    let host = hap_config.host;
    let hap_port = hap_config.port;
    let web_port = app_cfg.web_port;

    let server = IpServer::new(hap_config, storage)
        .await
        .context("creating HAP server")?;

    // Accessory ID 1 is the bridge itself; Daikin units are added as aid >= 2.
    server
        .add_accessory(BridgeAccessory::new(
            1,
            AccessoryInformation {
                manufacturer: "daikin-homekit".into(),
                model: "bridge".into(),
                name: app_cfg.name.clone(),
                serial_number: "daikin-homekit-bridge".into(),
                ..Default::default()
            },
        )?)
        .await
        .context("adding bridge accessory")?;

    let manager = Arc::new(Manager::new(
        server.clone(),
        activity.clone(),
        config_path.clone(),
        app_cfg,
    ));
    manager.init_devices().await?;

    // Start the admin web server.
    let web_state = WebState {
        manager: manager.clone(),
        activity: activity.clone(),
    };
    let web_app = web::router(web_state);
    let web_addr = format!("0.0.0.0:{web_port}");
    let listener = tokio::net::TcpListener::bind(&web_addr)
        .await
        .with_context(|| format!("binding web server to {web_addr}"))?;
    activity.info(None, format!("admin web UI on http://{host}:{web_port}"));
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, web_app).await {
            tracing::error!("web server error: {e:#}");
        }
    });

    activity.info(
        None,
        format!(
            "HomeKit bridge listening on {host}:{hap_port} — pair using the PIN in config.toml"
        ),
    );

    // Run the HAP server (HTTP + mDNS) until it exits.
    server.run_handle().await.context("HAP server stopped")?;
    Ok(())
}

/// Resolves the IPv4 address to advertise for HomeKit.
///
/// Uses `config.host` when set. Otherwise opens a UDP socket "connected" to a
/// unit on the LAN (no packets are sent) and reads back the local address the
/// OS would route through — the machine's primary LAN IPv4.
fn resolve_host(app_cfg: &AppConfig) -> std::net::IpAddr {
    use std::net::{IpAddr, Ipv4Addr, UdpSocket};

    if let Some(h) = app_cfg.host.as_ref().and_then(|h| h.parse::<IpAddr>().ok()) {
        return h;
    }

    let target = app_cfg
        .devices
        .first()
        .map(|d| format!("{}:80", d.ip))
        .unwrap_or_else(|| "10.255.255.255:80".into());

    let detected = UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect(&target)?;
            s.local_addr()
        })
        .ok()
        .map(|addr| addr.ip())
        .filter(|ip| ip.is_ipv4() && !ip.is_loopback());

    detected.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,daikin_homekit=info,hap=warn"));
    fmt().with_env_filter(filter).init();
}
