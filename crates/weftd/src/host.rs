//! A coordination server run inside the daemon, so any user can host one from the app.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;

use loom::Server;
use loom::config::Config;
use loom::db::Db;
use weft_ipc::{PanelAccess, Reach};
use weft_portmap::{PortMapper, Transport};
use weft_proto::{Host, Link, PublicKey};
use weft_session::StaticKeypair;

/// 443 looks like HTTPS to filters; the others are tried when it is taken.
const PORTS: [u16; 3] = [443, 8443, 0];

pub struct Hosted {
    _server: Server,
    pub port: u16,
    pub key: PublicKey,
    panel: Option<PanelAccess>,
    tcp: PortMapper,
    udp: PortMapper,
    /// Firewall rules added for the port, taken away when hosting stops.
    opened: std::sync::Arc<std::sync::Mutex<Vec<Rule>>>,
}

impl Drop for Hosted {
    fn drop(&mut self) {
        let rules = std::mem::take(&mut *self.opened.lock().unwrap());
        // ufw rules outlive the daemon, so they go before it exits; PowerShell is too slow to wait.
        #[cfg(windows)]
        std::thread::spawn(move || close_firewall(rules));
        #[cfg(not(windows))]
        close_firewall(rules);
    }
}

/// Starts the server, waiting a moment when the port is still held by a server just stopped.
async fn start_server(dir: &Path, port: u16) -> io::Result<Server> {
    let mut attempts = 0;
    loop {
        let keypair = StaticKeypair::load_or_create(&dir.join("key"))?;
        let db = Db::open(&dir.join("loom.db")).map_err(io::Error::other)?;
        let config =
            Config { listen: SocketAddr::from(([0, 0, 0, 0], port)), data_dir: dir.to_path_buf(), ..Config::default() };
        match Server::start(config, keypair, db).await {
            Err(error) if error.kind() == io::ErrorKind::AddrInUse && attempts < 10 => {
                attempts += 1;
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            result => return result,
        }
    }
}

impl Hosted {
    /// Starts the server with its key and database in `dir`; `port` 0 picks one.
    pub async fn start(dir: &Path, port: u16) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let candidates = if port == 0 { PORTS.to_vec() } else { vec![port] };
        let mut last = io::Error::other("no port to listen on");
        for port in candidates {
            match start_server(dir, port).await {
                Ok(server) => {
                    let port = server.tcp_addr.port();
                    tracing::info!(port, key = %server.public_key, "hosting a server");
                    // Firewall tools are slow external programs: they run aside from the daemon.
                    let opened: std::sync::Arc<std::sync::Mutex<Vec<Rule>>> = std::sync::Arc::default();
                    let record = std::sync::Arc::clone(&opened);
                    std::thread::spawn(move || {
                        let rules = open_firewall(port);
                        record.lock().unwrap().extend(rules);
                    });
                    return Ok(Self {
                        opened,
                        port,
                        key: server.public_key,
                        panel: panel(dir, port),
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

    pub fn panel(&self) -> Option<PanelAccess> {
        self.panel.clone()
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
    /// router's public address, then this computer's address in the local network. `observed`
    /// is this device's address as other servers see it, for routers that forward the port
    /// without telling their address.
    pub fn share(&self, address: Option<&str>, observed: Option<IpAddr>) -> (Option<Link>, Reach) {
        let link = |host: &str| loom::server_link(host, self.port, &self.key).ok();
        if let Some(address) = address.map(str::trim).filter(|address| !address.is_empty()) {
            return (link(address), Reach::Public);
        }
        let mapped = self.tcp.current().and(self.udp.current());
        let external = mapped.as_ref().and_then(|mapped| mapped.ip).or(mapped.and(observed));
        let reach = match external {
            Some(ip) if is_public(ip) => return (link(&ip.to_string()), Reach::Public),
            Some(_) => Reach::Behind,
            None => Reach::Local,
        };
        let lan = weft_portmap::local_addresses(None).into_iter().find(|ip| ip.is_ipv4() && !is_public(*ip));
        (lan.and_then(|ip| link(&ip.to_string())), reach)
    }
}

/// The panel of the hosted server, opened on this computer. Its password is kept next to the
/// server's data so the app can show it; the directory belongs to root.
fn panel(dir: &Path, port: u16) -> Option<PanelAccess> {
    let secret = loom::panel::secret(dir).ok()?;
    let stored = dir.join("panel-password");
    let password = match std::fs::read_to_string(&stored) {
        Ok(password) if !password.trim().is_empty() && loom::panel::has_password(dir) => password.trim().to_string(),
        _ => {
            let password = loom::panel::generate_password();
            loom::panel::set_password(dir, &password).ok()?;
            loom::panel::write_private(&stored, &password).ok()?;
            password
        }
    };
    Some(PanelAccess { url: loom::panel::url("127.0.0.1", port, &secret), password })
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

/// A firewall rule this daemon added.
enum Rule {
    #[cfg(target_os = "linux")]
    Firewalld(String),
    #[cfg(target_os = "linux")]
    Ufw(String),
    #[cfg(windows)]
    Windows,
}

#[cfg(target_os = "linux")]
fn quiet(program: &str, args: &[&str]) -> Option<std::process::Output> {
    std::process::Command::new(program).args(args).output().ok().filter(|output| output.status.success())
}

/// Opens the port in firewalld or ufw and returns the rules that were not there before.
#[cfg(target_os = "linux")]
fn open_firewall(port: u16) -> Vec<Rule> {
    let mut rules = Vec::new();
    let specs = [format!("{port}/tcp"), format!("{port}/udp")];
    if quiet("firewall-cmd", &["--state"]).is_some() {
        for spec in &specs {
            let known = quiet("firewall-cmd", &[&format!("--query-port={spec}")]).is_some();
            if !known && quiet("firewall-cmd", &[&format!("--add-port={spec}")]).is_some() {
                rules.push(Rule::Firewalld(spec.clone()));
            }
        }
        tracing::info!(port, added = rules.len(), "firewalld");
    }
    let ufw = quiet("ufw", &["status"]);
    if ufw.is_some_and(|output| String::from_utf8_lossy(&output.stdout).contains("Status: active")) {
        for spec in &specs {
            // ufw says "Skipping adding existing rule" for a rule the user already has.
            let added = quiet("ufw", &["allow", spec])
                .is_some_and(|output| !String::from_utf8_lossy(&output.stdout).contains("Skipping"));
            if added {
                rules.push(Rule::Ufw(spec.clone()));
            }
        }
        tracing::info!(port, "ufw");
    }
    rules
}

#[cfg(windows)]
fn open_firewall(port: u16) -> Vec<Rule> {
    let script = format!(
        "Remove-NetFirewallRule -DisplayName 'Weft server' -ErrorAction SilentlyContinue; \
         New-NetFirewallRule -DisplayName 'Weft server' -Direction Inbound -Action Allow -Profile Any \
         -Protocol TCP -LocalPort {port} | Out-Null; \
         New-NetFirewallRule -DisplayName 'Weft server' -Direction Inbound -Action Allow -Profile Any \
         -Protocol UDP -LocalPort {port} | Out-Null"
    );
    let status = powershell(&script);
    tracing::info!(port, ?status, "windows firewall rule for the server");
    vec![Rule::Windows]
}

#[cfg(windows)]
fn powershell(script: &str) -> std::io::Result<std::process::ExitStatus> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
}

#[cfg(not(any(target_os = "linux", windows)))]
fn open_firewall(_port: u16) -> Vec<Rule> {
    Vec::new()
}

fn close_firewall(rules: Vec<Rule>) {
    for rule in rules {
        match rule {
            #[cfg(target_os = "linux")]
            Rule::Firewalld(spec) => {
                quiet("firewall-cmd", &[&format!("--remove-port={spec}")]);
            }
            #[cfg(target_os = "linux")]
            Rule::Ufw(spec) => {
                quiet("ufw", &["delete", "allow", &spec]);
            }
            #[cfg(windows)]
            Rule::Windows => {
                let _ = powershell("Remove-NetFirewallRule -DisplayName 'Weft server' -ErrorAction SilentlyContinue");
            }
        }
    }
}

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
        let (link, reach) = hosted.share(Some("vpn.example.com"), None);
        assert_eq!(reach, Reach::Public);
        assert_eq!(link.unwrap(), loom::server_link("vpn.example.com", hosted.port, &hosted.key).unwrap());
        let key = hosted.key;
        drop(hosted);
        let again = Hosted::start(&dir, 0).await;
        if let Ok(again) = again {
            assert_eq!(again.key, key);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
