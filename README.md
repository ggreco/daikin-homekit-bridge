# Daikin HomeKit Bridge

A small, self-contained Rust binary that bridges classic-API Daikin air
conditioners (Wi-Fi adapters such as **BRP069Bxx**, the "pre-Onecta" generation)
to Apple HomeKit. Each unit appears in the Home app as a **Heater Cooler**
accessory. It also serves a tiny LAN-only web UI for managing devices and
watching activity.

Designed to run 24/7 on an always-on macOS machine (Intel or Apple Silicon) via
`launchd`.

## Features

- Exposes each Daikin unit as a HomeKit **Heater Cooler**: power, mode
  (auto/heat/cool), target temperature, current temperature, fan speed and swing.
- Two-way sync: changes made from the Home app are pushed to the unit, and
  changes made elsewhere (IR remote, Daikin app) are reflected back in HomeKit
  within one poll cycle (~10s).
- Bundled pure-Rust mDNS responder — **no Bonjour/Avahi system dependency**, so
  the release binary is self-contained.
- Built-in admin web UI (no auth, LAN-only) to add / edit / remove devices and
  view a live activity feed.

## How it works

```
iOS Home app  ──HAP over IP (mDNS + HTTP)──▶  daikin-homekit  ──HTTP GET──▶  Daikin units
Browser (LAN) ──HTTP admin UI (:8090)──────▶  daikin-homekit
```

The classic Daikin adapters speak a plaintext HTTP API (`/aircon/get_control_info`,
`/aircon/set_control_info`, `/aircon/get_sensor_info`, `/common/basic_info`)
returning comma-separated `key=value` pairs. Writes are performed via HTTP GET.

> Note: these adapters require the HTTP `Host` header in canonical case and
> return `403` otherwise, so the bridge talks HTTP directly over TCP rather than
> through a conventional HTTP client library.

## Requirements

- Rust (stable) to build.
- The bridge machine and your iOS devices must be on the same LAN/subnet as the
  Daikin units (HomeKit + mDNS do not cross subnets without extra setup).

## Configuration

Configuration lives in `config.toml` (created with defaults on first run):

```toml
pin = "11122333"          # 8-digit HomeKit pairing code
name = "Daikin Bridge"    # bridge name shown in the Home app
hap_port = 32000          # HomeKit Accessory Protocol port
web_port = 8090           # admin web UI port
# host = "10.0.0.206"     # optional: pin the advertised IPv4 (auto-detected if unset)
storage_dir = "./data"    # where HAP pairing state/keys are persisted
poll_secs = 10            # how often each unit is polled

[[device]]
id = 2                    # stable HomeKit accessory id (>= 2; 1 is the bridge)
name = "Salone"
ip = "10.0.0.142"

[[device]]
id = 3
name = "Home Theater"
ip = "10.0.0.247"
```

The device list is rewritten by the app when you change devices from the web UI,
so the web UI is the source of truth for devices at runtime. Keep the `id`
values stable so HomeKit room assignments and pairings survive edits.

## Build & run

```bash
# Development build + run (uses ./config.toml, creates ./data)
cargo run

# Optimized release build for the current machine
cargo build --release
./target/release/daikin-homekit /path/to/config.toml
```

### Cross-building for an Intel (x86_64) Mac

If you build on Apple Silicon but deploy to an Intel Mac:

```bash
rustup target add x86_64-apple-darwin
cargo build --release --target x86_64-apple-darwin
# binary at: target/x86_64-apple-darwin/release/daikin-homekit
```

(On an Intel Mac, a plain `cargo build --release` already targets x86_64.)

## Pairing with HomeKit

1. Start the bridge on the always-on machine.
2. In the iOS **Home** app: **Add Accessory → More options…** and select the
   bridge, or choose **I Don't Have a Code / Enter Code Manually** and type the
   `pin` from `config.toml` (shown formatted, e.g. `111-22-333`, in the web UI).
3. All configured units are added at once under the bridge.

The pairing code is also displayed in the admin web UI at
`http://<bridge-ip>:8090`.

## Admin web UI

Open `http://<bridge-ip>:8090` on any LAN device. You can:

- see each unit's live status (power, mode, setpoint, indoor/outdoor temp, fan);
- control each unit directly: power on/off, switch mode (auto/heat/cool) and
  adjust the target temperature — changes are written to the unit and re-synced
  to HomeKit immediately;
- add a device (the bridge probes the IP and auto-fills the name);
- edit a device's IP/name, or remove it;
- watch the live activity feed (polls, HomeKit + web commands, errors, edits).

The UI is unauthenticated by design — only expose it on a trusted LAN.

## Run at boot with launchd

A sample `LaunchDaemon` is provided in
[`deploy/com.gabry.daikin-homekit.plist`](deploy/com.gabry.daikin-homekit.plist).

```bash
# Install binary and config
sudo mkdir -p /usr/local/bin /usr/local/etc/daikin-homekit /usr/local/var/log
sudo cp target/release/daikin-homekit /usr/local/bin/
sudo cp config.toml /usr/local/etc/daikin-homekit/

# Install the service. launchd requires LaunchDaemon plists to be owned by
# root:wheel and not group/other-writable, or it refuses to load them with
# "Path had bad ownership/permissions".
sudo cp deploy/com.gabry.daikin-homekit.plist /Library/LaunchDaemons/
sudo chown root:wheel /Library/LaunchDaemons/com.gabry.daikin-homekit.plist
sudo chmod 644 /Library/LaunchDaemons/com.gabry.daikin-homekit.plist

# Start it
sudo launchctl load /Library/LaunchDaemons/com.gabry.daikin-homekit.plist

# Logs
tail -f /usr/local/var/log/daikin-homekit.log

# Stop / uninstall
sudo launchctl unload /Library/LaunchDaemons/com.gabry.daikin-homekit.plist
```

`KeepAlive` is enabled so the service restarts automatically if it exits.

## Mapping notes & limitations

- Daikin **dry** and **fan** modes have no Heater Cooler equivalent and are
  presented in HomeKit as **Auto**.
- Fan speed is mapped from Daikin's discrete rates (`silent`, levels 1–5, `auto`)
  onto the 0–100% HomeKit slider; `auto` sits at the top of the range.
- Swing is on/off in HomeKit and maps to Daikin's vertical+horizontal swing.
- Target/threshold temperatures are clamped to safe ranges (heat 10–30 °C,
  cool 16–32 °C).

## Project layout

- `src/daikin.rs` — Daikin local HTTP client and response parser.
- `src/hk.rs` — HomeKit Heater Cooler mapping, write callbacks and poll loop.
- `src/bridge.rs` — runtime manager (HAP server + config + live device set).
- `src/web.rs`, `src/web/index.html` — admin web service and UI.
- `src/activity.rs` — in-memory activity log.
- `src/config.rs` — `config.toml` schema and persistence.
- `vendor/get_if_addrs/` — pure-Rust shim replacing an unmaintained transitive
  dependency of `hap` to resolve a native-library conflict (see its `Cargo.toml`).

## Acknowledgements

- HomeKit side: [`ewilken/hap-rs`](https://github.com/ewilken/hap-rs).
- Daikin protocol reference:
  [`Apollon77/daikin-controller`](https://github.com/Apollon77/daikin-controller)
  and the [unofficial Daikin API docs](https://github.com/ael-code/daikin-control).
