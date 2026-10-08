use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use serde::{Deserialize, Serialize};

use crate::{Connection, PeerLink};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Diagnostics {
    pub version: String,
    pub os: String,
    pub connection: Connection,
    pub server: Option<String>,
    pub server_udp: Option<bool>,
    pub observed: Option<SocketAddr>,
    pub local_port: u16,
    pub local_addresses: Vec<IpAddr>,
    pub port_mapping: Option<PortMapping>,
    pub nat: Nat,
    pub peers: Vec<PeerDiagnostics>,
    pub logs: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PortMapping {
    pub protocol: String,
    pub external: Option<SocketAddr>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Nat {
    Unknown,
    None,
    Preserving,
    PortChanged,
    Cone,
    Symmetric,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PeerDiagnostics {
    pub nickname: String,
    pub address: Ipv4Addr,
    pub link: PeerLink,
    pub endpoint: Option<SocketAddr>,
    pub latency_ms: Option<u32>,
    pub candidates: Vec<SocketAddr>,
    pub observed: Option<SocketAddr>,
}

/// Classifies the NAT from the address the server sees and the addresses peers see.
pub fn classify_nat(
    observed: Option<SocketAddr>,
    local_port: u16,
    local_addresses: &[IpAddr],
    seen_by_peers: &[SocketAddr],
) -> Nat {
    let Some(observed) = observed else { return Nat::Unknown };
    if local_addresses.contains(&observed.ip()) && observed.port() == local_port {
        return Nat::None;
    }
    let external: Vec<&SocketAddr> = seen_by_peers.iter().filter(|addr| addr.ip() == observed.ip()).collect();
    if external.iter().any(|addr| addr.port() != observed.port()) {
        Nat::Symmetric
    } else if !external.is_empty() {
        Nat::Cone
    } else if observed.port() == local_port {
        Nat::Preserving
    } else {
        Nat::PortChanged
    }
}

/// Looks up a localized message by id with named arguments.
pub type Translate<'a> = dyn Fn(&str, &[(&str, &str)]) -> String + 'a;

pub fn format(diagnostics: &Diagnostics, tr: &Translate<'_>) -> String {
    let d = diagnostics;
    let none = tr("status-none", &[]);
    let connection = match d.connection {
        Connection::Disconnected => "state-disconnected",
        Connection::Connecting => "state-connecting",
        Connection::Connected => "state-connected",
    };
    let udp = match d.server_udp {
        Some(true) => "diag-udp-ok",
        Some(false) => "diag-udp-blocked",
        None => "diag-udp-unknown",
    };
    let mapping = match &d.port_mapping {
        Some(mapping) => match mapping.external {
            Some(external) => format!("{} {external}", mapping.protocol),
            None => mapping.protocol.clone(),
        },
        None => tr("diag-mapping-none", &[]),
    };
    let nat = match d.nat {
        Nat::Unknown => "diag-nat-unknown",
        Nat::None => "diag-nat-none",
        Nat::Preserving => "diag-nat-preserving",
        Nat::PortChanged => "diag-nat-port-changed",
        Nat::Cone => "diag-nat-cone",
        Nat::Symmetric => "diag-nat-symmetric",
    };
    let addresses: Vec<String> = d.local_addresses.iter().map(ToString::to_string).collect();
    let rows = [
        (tr("diag-version", &[]), format!("{} · {}", d.version, d.os)),
        (tr("diag-server", &[]), d.server.clone().unwrap_or_else(|| none.clone())),
        (tr("diag-connection", &[]), tr(connection, &[])),
        (tr("diag-server-udp", &[]), tr(udp, &[])),
        (tr("diag-public", &[]), d.observed.map_or_else(|| none.clone(), |addr| addr.to_string())),
        (tr("diag-local-port", &[]), d.local_port.to_string()),
        (tr("diag-local-addresses", &[]), if addresses.is_empty() { none.clone() } else { addresses.join(", ") }),
        (tr("diag-mapping", &[]), mapping),
        (tr("diag-nat", &[]), tr(nat, &[])),
    ];
    let width = rows.iter().map(|(label, _)| label.chars().count()).max().unwrap_or(0) + 1;
    let mut text = String::new();
    for (label, value) in &rows {
        text += &format!("{:<width$} {value}\n", format!("{label}:"));
    }

    text += &format!("\n{}\n", tr("diag-peers", &[]));
    if d.peers.is_empty() {
        text += &format!("  {}\n", tr("diag-peers-none", &[]));
    }
    for peer in &d.peers {
        let state = match (peer.link, peer.endpoint, peer.latency_ms) {
            (PeerLink::Offline, ..) => tr("diag-peer-offline", &[]),
            (PeerLink::Connecting, ..) => tr("diag-peer-connecting", &[]),
            (PeerLink::Relay, ..) => tr("diag-peer-relay", &[]),
            (PeerLink::Direct, Some(endpoint), Some(ms)) => {
                tr("diag-peer-direct-latency", &[("endpoint", &endpoint.to_string()), ("ms", &ms.to_string())])
            }
            (PeerLink::Direct, endpoint, _) => {
                tr("diag-peer-direct", &[("endpoint", &endpoint.map_or_else(|| none.clone(), |e| e.to_string()))])
            }
        };
        text += &format!("  {} ({}): {state}\n", peer.nickname, peer.address);
        if matches!(peer.link, PeerLink::Relay | PeerLink::Connecting) {
            let reason = if peer.candidates.is_empty() {
                tr("diag-reason-no-candidates", &[])
            } else {
                let list: Vec<String> = peer.candidates.iter().map(ToString::to_string).collect();
                tr("diag-reason-no-answer", &[("count", &list.len().to_string()), ("addresses", &list.join(", "))])
            };
            text += &format!("    {reason}\n");
        }
        if let Some(observed) = peer.observed {
            text += &format!("    {}\n", tr("diag-peer-sees", &[("endpoint", &observed.to_string())]));
        }
    }

    if !d.logs.is_empty() {
        text += &format!("\n{}\n", tr("diag-logs", &[]));
        for line in &d.logs {
            text += line;
            text.push('\n');
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn nat_types() {
        let local = [IpAddr::from([192, 168, 1, 5])];
        let observed = Some(addr("203.0.113.7:41000"));
        assert_eq!(classify_nat(None, 41000, &local, &[]), Nat::Unknown);
        assert_eq!(classify_nat(Some(addr("192.168.1.5:41000")), 41000, &local, &[]), Nat::None);
        assert_eq!(classify_nat(observed, 41000, &local, &[]), Nat::Preserving);
        assert_eq!(classify_nat(observed, 5000, &local, &[]), Nat::PortChanged);
        assert_eq!(classify_nat(observed, 5000, &local, &[addr("203.0.113.7:41000")]), Nat::Cone);
        assert_eq!(classify_nat(observed, 5000, &local, &[addr("203.0.113.7:41007")]), Nat::Symmetric);
        assert_eq!(classify_nat(observed, 5000, &local, &[addr("192.168.1.5:5000")]), Nat::PortChanged);
    }

    #[test]
    fn formats_a_report() {
        let diagnostics = Diagnostics {
            version: "0.1.0".into(),
            os: "linux x86_64".into(),
            connection: Connection::Connected,
            server: Some("weft://example.com".into()),
            server_udp: Some(false),
            observed: None,
            local_port: 41000,
            local_addresses: vec![],
            port_mapping: None,
            nat: Nat::Unknown,
            peers: vec![PeerDiagnostics {
                nickname: "bob".into(),
                address: Ipv4Addr::new(100, 64, 0, 2),
                link: PeerLink::Relay,
                endpoint: None,
                latency_ms: None,
                candidates: vec![addr("198.51.100.4:5000")],
                observed: None,
            }],
            logs: vec!["INFO started".into()],
        };
        let text = format(&diagnostics, &|id, args| {
            args.iter().fold(id.to_string(), |text, (name, value)| format!("{text} {name}={value}"))
        });
        assert!(text.lines().any(|line| line.starts_with("diag-server-udp:") && line.ends_with(" diag-udp-blocked")));
        assert!(text.contains("bob (100.64.0.2): diag-peer-relay"));
        assert!(text.contains("diag-reason-no-answer count=1 addresses=198.51.100.4:5000"));
        assert!(text.ends_with("INFO started\n"));
    }
}
