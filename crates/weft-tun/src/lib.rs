use std::io;
use std::net::Ipv4Addr;

use tun_rs::{AsyncDevice, DeviceBuilder};

pub use dns::Dns;

mod dns;
mod routes;
#[cfg(windows)]
mod windows;

pub const DEFAULT_MTU: u16 = 1280;
#[cfg(windows)]
pub const DEFAULT_NAME: &str = "Weft";
#[cfg(not(windows))]
pub const DEFAULT_NAME: &str = "weft0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunConfig {
    pub name: String,
    pub address: Ipv4Addr,
    pub prefix: u8,
    pub mtu: u16,
    pub routes: Vec<Route>,
    pub dns: Option<Dns>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub destination: Ipv4Addr,
    pub prefix: u8,
}

impl Route {
    pub fn host(destination: Ipv4Addr) -> Self {
        Self { destination, prefix: 32 }
    }
}

impl TunConfig {
    pub fn network(&self) -> String {
        let mask = u32::MAX.checked_shl(32 - u32::from(self.prefix)).unwrap_or(0);
        format!("{}/{}", Ipv4Addr::from(u32::from(self.address) & mask), self.prefix)
    }
}

pub struct Tun {
    device: AsyncDevice,
    name: String,
    dns_configured: bool,
}

impl Tun {
    pub fn create(config: &TunConfig) -> io::Result<Self> {
        let builder = DeviceBuilder::new().ipv4(config.address, config.prefix, None).mtu(config.mtu);
        #[cfg(not(target_os = "macos"))]
        let builder = builder.name(&config.name);
        #[cfg(windows)]
        let builder = match windows::wintun_path() {
            Some(path) => builder.wintun_file(path),
            None => builder,
        };
        let device = builder.build_async()?;
        let name = device.name().unwrap_or_else(|_| config.name.clone());
        #[cfg(windows)]
        let dns_configured = {
            windows::configure(&name, &config.network(), &config.routes, config.dns.as_ref());
            config.dns.is_some()
        };
        #[cfg(not(windows))]
        let dns_configured = {
            routes::apply(&name, &config.routes);
            config.dns.as_ref().is_some_and(|dns| dns::apply(&name, dns))
        };
        Ok(Self { device, name, dns_configured })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the system resolver was told to send peer names to the daemon.
    pub fn dns_configured(&self) -> bool {
        self.dns_configured
    }

    /// Adds an address from another server's pool.
    pub fn add_address(&self, address: Ipv4Addr, prefix: u8) -> io::Result<()> {
        self.device.add_address_v4(address, prefix)
    }

    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.device.recv(buf).await
    }

    pub async fn send(&self, packet: &[u8]) -> io::Result<usize> {
        self.device.send(packet).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network() {
        let config = TunConfig {
            name: "t".into(),
            address: Ipv4Addr::new(100, 64, 3, 7),
            prefix: 10,
            mtu: 1280,
            routes: Vec::new(),
            dns: None,
        };
        assert_eq!(config.network(), "100.64.0.0/10");
        let config = TunConfig { address: Ipv4Addr::new(10, 1, 2, 3), prefix: 24, ..config };
        assert_eq!(config.network(), "10.1.2.0/24");
    }
}
