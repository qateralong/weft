//! A coordination server run inside the daemon, so any user can host one from the app.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;

use loom::Server;
use loom::config::Config;
use loom::db::Db;
use weft_ipc::Reach;
use weft_portmap::{PortMapper, Transport};
use weft_proto::{Host, Link, PublicKey};
use weft_session::StaticKeypair;

/// 443 looks like HTTPS to filters; the others are tried when it is taken.
const PORTS: [u16; 3] = [443, 8443, 0];

pub struct Hosted {
    _server: Server,
    pub port: u16,
    pub key: PublicKey,
    tcp: PortMapper,
    udp: PortMapper,
}

impl Hosted {
    /// Starts the server with its key and database in `dir`; `port` 0 picks one.
    pub async fn start(dir: &Path, port: u16) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let candidates = if port == 0 { PORTS.to_vec() } else { vec![port] };
        let mut last = io::Error::other("no port to listen on");
        for port in candidates {
            let keypair = StaticKeypair::load_or_create(&dir.join("key"))?;
            let db = Db::open(&dir.join("loom.db")).map_err(io::Error::other)?;
            let config = Config {
                listen: SocketAddr::from(([0, 0, 0, 0], port)),
                data_dir: dir.to_path_buf(),
                ..Config::default()
            };
            match Server::start(config, keypair, db).await {
                Ok(server) => {
                    let port = server.tcp_addr.port();
                    tracing::info!(port, key = %server.public_key, "hosting a server");
                    open_firewall(port);
                    return Ok(Self {
                        port,
                        key: server.public_key,
                        tcp: PortMapper::spawn_with(port, Transport::Tcp, true),
                        udp: PortMapper::spawn_with(port, Transport::Udp, true),
                        _server: server,
                    });
                }
                Err(error) => {
                    tracing::info!(port, %error, "cannot host on this port");
                    last = error;
                }
            }
        }
        Err(last)
    }

    /// How this device itself connects: over loopback, which needs no port forwarding.
    pub fn local_link(&self) -> Link {
        Link { host: Host::Ipv4(Ipv4Addr::LOCALHOST), port: self.port, invite: None, server_key: self.key }
    }

    /// Whether the router forwards the port in both protocols.
    pub fn mapped(&self) -> bool {
        self.tcp.current().is_some() && self.udp.current().is_some()
    }

    /// The link to give others and how far it reaches. A configured address wins, then the
    /// router's public address, then this computer's address in the local network.
    pub fn share(&self, address: Option<&str>) -> (Option<Link>, Reach) {
        let link = |host: &str| loom::server_link(host, self.port, &self.key).ok();
        if let Some(address) = address.map(str::trim).filter(|address| !address.is_empty()) {
            return (link(address), Reach::Public);
        }
        let external = self.tcp.current().and(self.udp.current()).and_then(|mapped| mapped.ip);
        let reach = match external {
            Some(ip) if is_public(ip) => return (link(&ip.to_string()), Reach::Public),
            Some(_) => Reach::Behind,
            None => Reach::Local,
        };
        let lan = weft_portmap::local_addresses(None).into_iter().find(|ip| ip.is_ipv4() && !is_public(*ip));
        (lan.and_then(|ip| link(&ip.to_string())), reach)
    }
}

/// Not private, carrier-grade NAT, loopback or link-local.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let cgnat = ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]);
            !(ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || cgnat)
        }
        IpAddr::V6(ip) => weft_portmap::is_global_v6(&ip),
    }
}

#[cfg(target_os = "linux")]
fn open_firewall(port: u16) {
    use std::process::Command;
    let quiet = |program: &str, args: &[&str]| {
        Command::new(program).args(args).output().is_ok_and(|output| output.status.success())
    };
    if quiet("firewall-cmd", &["--state"]) {
        let ports = [format!("--add-port={port}/tcp"), format!("--add-port={port}/udp")];
        let opened = quiet("firewall-cmd", &[&ports[0], &ports[1]]);
        tracing::info!(port, opened, "firewalld");
    }
    let ufw = Command::new("ufw").arg("status").output().ok();
    if ufw.is_some_and(|output| String::from_utf8_lossy(&output.stdout).contains("Status: active")) {
        let opened =
            quiet("ufw", &["allow", &format!("{port}/tcp")]) && quiet("ufw", &["allow", &format!("{port}/udp")]);
        tracing::info!(port, opened, "ufw");
    }
}

#[cfg(windows)]
fn open_firewall(port: u16) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = format!(
        "Remove-NetFirewallRule -DisplayName 'Weft server' -ErrorAction SilentlyContinue; \
         New-NetFirewallRule -DisplayName 'Weft server' -Direction Inbound -Action Allow -Profile Any \
         -Protocol TCP -LocalPort {port} | Out-Null"
    );
    let status = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    tracing::info!(port, ?status, "windows firewall rule for the server");
}

#[cfg(not(any(target_os = "linux", windows)))]
fn open_firewall(_port: u16) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_addresses() {
        assert!(is_public("203.0.113.5".parse().unwrap()));
        for ip in ["192.168.1.2", "10.0.0.1", "172.16.4.4", "100.72.1.1", "127.0.0.1", "169.254.1.1"] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn hosts_and_shares_a_link() {
        let dir = std::env::temp_dir().join(format!("weftd-host-{}", std::process::id()));
        let hosted = Hosted::start(&dir, 0).await.or(Hosted::start(&dir, 0).await);
        let hosted = match hosted {
            Ok(hosted) => hosted,
            Err(_) => {
                let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
                Hosted::start(&dir, free).await.unwrap()
            }
        };
        assert_eq!(hosted.local_link().host, Host::Ipv4(Ipv4Addr::LOCALHOST));
        let (link, reach) = hosted.share(Some("vpn.example.com"));
        assert_eq!(reach, Reach::Public);
        assert_eq!(link.unwrap().to_string(), format!("weft://vpn.example.com:{}#k={}", hosted.port, hosted.key));
        let key = hosted.key;
        drop(hosted);
        let again = Hosted::start(&dir, 0).await;
        if let Ok(again) = again {
            assert_eq!(again.key, key);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
