use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::num::NonZeroU16;
use std::time::Duration;

use crab_nat::{GatewayAddress, InternetProtocol, PortMapping, PortMappingOptions, PortMappingType, TimeoutConfig};
use igd_next::PortMappingProtocol;
use igd_next::aio::tokio::search_gateway;
use tokio::sync::watch;
use tokio::task::JoinHandle;

const LIFETIME: u32 = 600;
const RENEW_AFTER: Duration = Duration::from_secs(300);
const RETRY_AFTER: Duration = Duration::from_secs(300);
const DESCRIPTION: &str = "weft";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mapped {
    pub port: u16,
    pub ip: Option<IpAddr>,
}

impl Mapped {
    pub fn endpoint(&self, observed: Option<IpAddr>) -> Option<SocketAddr> {
        self.ip.or(observed).map(|ip| SocketAddr::new(ip, self.port))
    }
}

pub struct PortMapper {
    rx: watch::Receiver<Option<Mapped>>,
    task: JoinHandle<()>,
}

impl PortMapper {
    pub fn spawn(local_port: u16) -> Self {
        let (tx, rx) = watch::channel(None);
        let task = tokio::spawn(run(local_port, tx));
        Self { rx, task }
    }

    pub fn current(&self) -> Option<Mapped> {
        *self.rx.borrow()
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<Mapped>> {
        self.rx.clone()
    }
}

impl Drop for PortMapper {
    fn drop(&mut self) {
        self.task.abort();
    }
}

enum Lease {
    Pmp(PortMapping),
    Upnp { gateway: igd_next::aio::Gateway<igd_next::aio::tokio::Tokio>, local: SocketAddrV4, port: u16 },
}

async fn run(local_port: u16, tx: watch::Sender<Option<Mapped>>) {
    let Some(local_port) = NonZeroU16::new(local_port) else { return };
    loop {
        match map(local_port).await {
            Some((mut lease, mapped)) => {
                tracing::info!(port = mapped.port, ip = ?mapped.ip, "port mapping created");
                tx.send_replace(Some(mapped));
                loop {
                    tokio::time::sleep(RENEW_AFTER).await;
                    if !renew(&mut lease).await {
                        tracing::info!("port mapping lost");
                        tx.send_replace(None);
                        break;
                    }
                }
            }
            None => {
                tracing::debug!("no port mapping protocol is available");
                tx.send_replace(None);
                tokio::time::sleep(RETRY_AFTER).await;
            }
        }
    }
}

async fn map(local_port: NonZeroU16) -> Option<(Lease, Mapped)> {
    let local = local_ipv4()?;
    if let Some(gateway) = gateway_ipv4() {
        let options = PortMappingOptions {
            external_port: Some(local_port),
            lifetime_seconds: Some(LIFETIME),
            timeout_config: Some(TimeoutConfig {
                initial_timeout: Duration::from_millis(250),
                max_retries: 2,
                max_retry_timeout: Some(Duration::from_secs(1)),
            }),
        };
        let result =
            PortMapping::new(GatewayAddress::IpV4(gateway), local.into(), InternetProtocol::Udp, local_port, options)
                .await;
        match result {
            Ok(mapping) => {
                let ip = match mapping.mapping_type() {
                    PortMappingType::Pcp { external_ip, .. } => Some(external_ip),
                    PortMappingType::NatPmp => None,
                };
                let mapped = Mapped { port: mapping.external_port().get(), ip };
                return Some((Lease::Pmp(mapping), mapped));
            }
            Err(error) => tracing::debug!(%error, "pcp and nat-pmp failed"),
        }
    }

    let mut options = igd_next::SearchOptions::default();
    options.timeout = Some(Duration::from_secs(3));
    let gateway = search_gateway(options).await.ok()?;
    let local = SocketAddrV4::new(local, local_port.get());
    let port = gateway
        .add_any_port(PortMappingProtocol::UDP, SocketAddr::V4(local), LIFETIME, DESCRIPTION)
        .await
        .map_err(|error| tracing::debug!(%error, "upnp mapping failed"))
        .ok()?;
    let ip = gateway.get_external_ip().await.ok();
    Some((Lease::Upnp { gateway, local, port }, Mapped { port, ip }))
}

async fn renew(lease: &mut Lease) -> bool {
    match lease {
        Lease::Pmp(mapping) => mapping.renew().await.is_ok(),
        Lease::Upnp { gateway, local, port } => gateway
            .add_port(PortMappingProtocol::UDP, *port, SocketAddr::V4(*local), LIFETIME, DESCRIPTION)
            .await
            .is_ok(),
    }
}

fn gateway_ipv4() -> Option<Ipv4Addr> {
    netdev::get_default_gateway().ok()?.ipv4.first().copied()
}

fn local_ipv4() -> Option<Ipv4Addr> {
    netdev::get_default_interface().ok()?.ipv4.first().map(|net| net.addr())
}

pub fn local_addresses(exclude_interface: Option<&str>) -> Vec<IpAddr> {
    netdev::get_interfaces()
        .into_iter()
        .filter(|interface| interface.is_up() && !interface.is_loopback())
        .filter(|interface| exclude_interface.is_none_or(|name| interface.name != name))
        .flat_map(|interface| interface.ipv4.iter().map(|net| IpAddr::V4(net.addr())).collect::<Vec<_>>())
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
        .collect()
}
