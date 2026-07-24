//! Minimal, pure-Rust re-implementation of the `get_if_addrs` 0.5 API surface
//! used by the `hap` crate, backed by the maintained `if-addrs` crate.

use std::net::IpAddr;

/// A network interface, mirroring the subset of the original
/// `get_if_addrs::Interface` API that dependents rely on.
pub struct Interface {
    inner: if_addrs::Interface,
}

impl Interface {
    /// The IP address bound to this interface.
    pub fn ip(&self) -> IpAddr {
        self.inner.ip()
    }

    /// Whether this interface is a loopback interface.
    pub fn is_loopback(&self) -> bool {
        self.inner.is_loopback()
    }

    /// The interface name (e.g. `en0`).
    pub fn name(&self) -> &str {
        &self.inner.name
    }
}

/// Return all network interfaces on the host.
pub fn get_if_addrs() -> std::io::Result<Vec<Interface>> {
    Ok(if_addrs::get_if_addrs()?
        .into_iter()
        .map(|inner| Interface { inner })
        .collect())
}
