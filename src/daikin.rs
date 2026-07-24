//! Minimal client for the classic (pre-Onecta) Daikin local HTTP API.
//!
//! These adapters (e.g. BRP069Bxx) speak a plaintext protocol: every endpoint
//! returns a comma-separated list of `key=value` pairs prefixed with a `ret=`
//! status. Writes are performed via HTTP GET with the parameters in the query
//! string (this firmware generation does not accept POST).

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use percent_encoding::percent_decode_str;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Daikin operation modes as reported/accepted by `mode=`.
pub mod mode {
    pub const AUTO: u8 = 0;
    pub const AUTO1: u8 = 1;
    pub const DRY: u8 = 2;
    pub const COOL: u8 = 3;
    pub const HEAT: u8 = 4;
    pub const FAN: u8 = 6;
    pub const AUTO7: u8 = 7;
}

/// A client bound to a single Daikin unit.
///
/// These adapters run a minimal, quirky HTTP server that rejects requests
/// (403) unless the `Host` header is sent in canonical case. Common HTTP client
/// libraries lowercase header names on HTTP/1.1, so we speak HTTP directly over
/// TCP to fully control the wire format.
#[derive(Clone)]
pub struct DaikinClient {
    host: String,
    port: u16,
    timeout: Duration,
}

/// Parsed `get_control_info` response. The raw map is retained so writes can
/// perform a read-modify-write that preserves fields we do not model.
#[derive(Debug, Clone)]
pub struct ControlInfo {
    pub raw: HashMap<String, String>,
}

/// Parsed `get_sensor_info` response.
#[derive(Debug, Clone)]
pub struct SensorInfo {
    pub raw: HashMap<String, String>,
}

/// A single field mutation to apply to a unit's control state.
#[derive(Debug, Clone)]
pub enum Command {
    Power(bool),
    Mode(u8),
    /// Target temperature in Celsius.
    Temperature(f32),
    /// Raw fan-rate token (`A`, `B`, or `3`..=`7`).
    FanRate(String),
    /// Fan direction / swing (`0` off .. `3` both axes).
    FanDir(u8),
}

impl DaikinClient {
    pub fn new(ip: &str) -> Result<Self> {
        Ok(Self {
            host: ip.to_string(),
            port: 80,
            timeout: Duration::from_secs(5),
        })
    }

    /// Issues a raw HTTP/1.1 GET with a canonical-case `Host` header and returns
    /// the response body.
    async fn http_get(&self, path: &str) -> Result<String> {
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
            self.host
        );

        let io = async {
            let mut stream = TcpStream::connect((self.host.as_str(), self.port)).await?;
            stream.write_all(request.as_bytes()).await?;
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await?;
            Ok::<_, std::io::Error>(buf)
        };

        let raw = timeout(self.timeout, io)
            .await
            .with_context(|| format!("timed out talking to {}{}", self.host, path))?
            .with_context(|| format!("I/O error talking to {}{}", self.host, path))?;

        let text = String::from_utf8_lossy(&raw);
        let (head, body) = text
            .split_once("\r\n\r\n")
            .ok_or_else(|| anyhow!("malformed HTTP response from {}{}", self.host, path))?;

        let status_line = head.lines().next().unwrap_or_default();
        let code = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse::<u16>().ok())
            .unwrap_or(0);
        if code != 200 {
            return Err(anyhow!("HTTP {code} from {}{}", self.host, path));
        }

        Ok(body.to_string())
    }

    /// Performs a GET and parses the `key=value` body, verifying `ret=OK`.
    async fn get(&self, path: &str) -> Result<HashMap<String, String>> {
        let body = self.http_get(path).await?;
        let map = parse_response(&body);
        match map.get("ret").map(String::as_str) {
            Some("OK") => Ok(map),
            Some(other) => Err(anyhow!("device returned ret={other} for {path}")),
            None => Err(anyhow!("malformed response for {path}: {body}")),
        }
    }

    /// Reads `/common/basic_info` (device identity). Used to validate a newly
    /// added IP and to discover the device's own name.
    pub async fn basic_info(&self) -> Result<HashMap<String, String>> {
        self.get("/common/basic_info").await
    }

    /// The human-readable name configured on the unit itself, if present.
    pub async fn device_name(&self) -> Result<Option<String>> {
        let info = self.basic_info().await?;
        Ok(info.get("name").filter(|s| !s.is_empty()).cloned())
    }

    pub async fn control_info(&self) -> Result<ControlInfo> {
        Ok(ControlInfo {
            raw: self.get("/aircon/get_control_info").await?,
        })
    }

    pub async fn sensor_info(&self) -> Result<SensorInfo> {
        Ok(SensorInfo {
            raw: self.get("/aircon/get_sensor_info").await?,
        })
    }

    /// Applies a single field change using a read-modify-write cycle so the
    /// full required parameter set is always sent back to the device.
    pub async fn apply(&self, command: Command) -> Result<()> {
        let current = self.control_info().await?;
        let raw = &current.raw;

        let mut pow = raw.get("pow").cloned().unwrap_or_else(|| "1".into());
        let mut mode = raw.get("mode").cloned().unwrap_or_else(|| "3".into());
        let mut stemp = raw.get("stemp").cloned().unwrap_or_else(|| "22.0".into());
        let mut shum = raw.get("shum").cloned().unwrap_or_else(|| "0".into());
        let mut f_rate = raw.get("f_rate").cloned().unwrap_or_else(|| "A".into());
        let mut f_dir = raw.get("f_dir").cloned().unwrap_or_else(|| "0".into());

        match command {
            Command::Power(on) => pow = if on { "1".into() } else { "0".into() },
            Command::Mode(m) => {
                mode = m.to_string();
                // Modes with a numeric setpoint need a valid `stemp`/`shum`; if
                // we're coming from DRY/FAN (where they are "M"/"AUTO"), fall
                // back to sane defaults.
                if matches!(
                    m,
                    mode::AUTO | mode::AUTO1 | mode::COOL | mode::HEAT | mode::AUTO7
                ) {
                    if stemp.parse::<f32>().is_err() {
                        stemp = "22.0".into();
                    }
                    if shum.parse::<u32>().is_err() {
                        shum = "0".into();
                    }
                }
            }
            Command::Temperature(t) => stemp = format!("{:.1}", t),
            Command::FanRate(r) => f_rate = r,
            Command::FanDir(d) => f_dir = d.to_string(),
        }

        let query = format!(
            "/aircon/set_control_info?pow={pow}&mode={mode}&stemp={stemp}&shum={shum}&f_rate={f_rate}&f_dir={f_dir}"
        );
        self.get(&query).await.map(|_| ())
    }
}

impl ControlInfo {
    pub fn power(&self) -> bool {
        self.raw.get("pow").map(String::as_str) == Some("1")
    }

    pub fn mode(&self) -> u8 {
        self.raw
            .get("mode")
            .and_then(|s| s.parse().ok())
            .unwrap_or(mode::AUTO)
    }

    /// Target temperature, if the current mode exposes a numeric setpoint.
    pub fn target_temp(&self) -> Option<f32> {
        self.raw.get("stemp").and_then(|s| s.parse().ok())
    }

    pub fn fan_rate(&self) -> String {
        self.raw
            .get("f_rate")
            .cloned()
            .unwrap_or_else(|| "A".into())
    }

    pub fn fan_dir(&self) -> u8 {
        self.raw
            .get("f_dir")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }
}

impl SensorInfo {
    /// Indoor temperature (`htemp`).
    pub fn indoor_temp(&self) -> Option<f32> {
        self.raw.get("htemp").and_then(|s| s.parse().ok())
    }

    /// Outdoor temperature (`otemp`).
    pub fn outdoor_temp(&self) -> Option<f32> {
        self.raw.get("otemp").and_then(|s| s.parse().ok())
    }
}

/// Parses a `key=value,key=value` Daikin body into a map, percent-decoding
/// values (device names arrive percent-encoded).
fn parse_response(body: &str) -> HashMap<String, String> {
    body.trim()
        .split(',')
        .filter_map(|pair| {
            let mut it = pair.splitn(2, '=');
            let key = it.next()?.trim();
            if key.is_empty() {
                return None;
            }
            let raw_val = it.next().unwrap_or("");
            let val = percent_decode_str(raw_val).decode_utf8_lossy().into_owned();
            Some((key.to_string(), val))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_control_info() {
        let body = "ret=OK,pow=1,mode=3,stemp=22.5,shum=0,f_rate=A,f_dir=0";
        let ci = ControlInfo {
            raw: parse_response(body),
        };
        assert!(ci.power());
        assert_eq!(ci.mode(), mode::COOL);
        assert_eq!(ci.target_temp(), Some(22.5));
        assert_eq!(ci.fan_rate(), "A");
        assert_eq!(ci.fan_dir(), 0);
    }

    #[test]
    fn decodes_percent_encoded_name() {
        let body = "ret=OK,name=%53%61%6c%6f%6e%65";
        let map = parse_response(body);
        assert_eq!(map.get("name").unwrap(), "Salone");
    }

    #[test]
    fn non_numeric_setpoint_is_none() {
        let body = "ret=OK,stemp=M";
        let ci = ControlInfo {
            raw: parse_response(body),
        };
        assert_eq!(ci.target_temp(), None);
    }
}
