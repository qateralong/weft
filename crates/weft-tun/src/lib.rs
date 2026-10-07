use std::io;
use std::net::Ipv4Addr;

use tun_rs::{AsyncDevice, DeviceBuilder};

pub const DEFAULT_MTU: u16 = 1280;
pub const DEFAULT_NAME: &str = "weft0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunConfig {
    pub name: String,
    pub address: Ipv4Addr,
    pub prefix: u8,
    pub mtu: u16,
}

pub struct Tun {
    device: AsyncDevice,
    name: String,
}

impl Tun {
    pub fn create(config: &TunConfig) -> io::Result<Self> {
        let builder = DeviceBuilder::new().ipv4(config.address, config.prefix, None).mtu(config.mtu);
        #[cfg(not(target_os = "macos"))]
        let builder = builder.name(&config.name);
        let device = builder.build_async()?;
        let name = device.name().unwrap_or_else(|_| config.name.clone());
        Ok(Self { device, name })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.device.recv(buf).await
    }

    pub async fn send(&self, packet: &[u8]) -> io::Result<usize> {
        self.device.send(packet).await
    }
}
