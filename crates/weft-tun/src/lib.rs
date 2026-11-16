use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
        network(self.address, self.prefix)
    }
}

fn network(address: Ipv4Addr, prefix: u8) -> String {
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    format!("{}/{}", Ipv4Addr::from(u32::from(address) & mask), prefix)
}

pub struct Tun {
    device: AsyncDevice,
    name: String,
    dns: Option<Dns>,
    dns_configured: Arc<AtomicBool>,
    /// Address pools whose peers the Windows firewall lets in.
    #[cfg(windows)]
    pools: windows::Pools,
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
        let dns_configured = Arc::new(AtomicBool::new(false));
        #[cfg(windows)]
        let pools = {
            let pools = windows::Pools::new(config.network());
            windows::configure(&name, pools.clone(), &config.routes, config.dns.as_ref());
            dns_configured.store(config.dns.is_some(), Ordering::Relaxed);
            pools
        };
        // Routes and the resolver are set up by external programs, which must not hold up the caller.
        #[cfg(not(windows))]
        {
            let (name, routes, dns, configured) =
                (name.clone(), config.routes.clone(), config.dns.clone(), dns_configured.clone());
            std::thread::spawn(move || {
                routes::apply(&name, &routes);
                let ok = dns.as_ref().is_some_and(|dns| dns::apply(&name, dns));
                configured.store(ok, Ordering::Relaxed);
            });
        }
        Ok(Self {
            device,
            name,
            dns: config.dns.clone(),
            dns_configured,
            #[cfg(windows)]
            pools,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the system resolver was told to send peer names to the daemon.
    pub fn dns_configured(&self) -> bool {
        self.dns_configured.load(Ordering::Relaxed)
    }

    /// Adds an address from another server's pool.
    pub fn add_address(&self, address: Ipv4Addr, prefix: u8) -> io::Result<()> {
        self.device.add_address_v4(address, prefix)?;
        #[cfg(windows)]
        windows::allow_pools(&self.name, self.pools.clone(), Some(network(address, prefix)), None);
        Ok(())
    }

    /// Takes away the address of a server that is gone, with its route.
    pub fn remove_address(&self, address: Ipv4Addr, prefix: u8) -> io::Result<()> {
        self.device.remove_address(IpAddr::V4(address))?;
        #[cfg(windows)]
        windows::allow_pools(&self.name, self.pools.clone(), None, Some(network(address, prefix)));
        #[cfg(not(windows))]
        let _ = prefix;
        Ok(())
    }

    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.device.recv(buf).await
    }

    pub async fn send(&self, packet: &[u8]) -> io::Result<usize> {
        self.device.send(packet).await
    }
}

impl Drop for Tun {
    fn drop(&mut self) {
        if let Some(dns) = &self.dns {
            dns::remove(dns);
        }
    }
}

/// Removes what Weft set up in the system outside its interface, for an uninstaller.
pub fn cleanup() {
    #[cfg(windows)]
    windows::cleanup();
    #[cfg(not(windows))]
    dns::remove(&Dns { server: Ipv4Addr::UNSPECIFIED, domain: dns::DOMAIN.to_string() });
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
