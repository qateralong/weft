//! Turns the daemon's status into what the window shows.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use slint::{Model, ModelRc, SharedString, VecModel};
use weft_i18n::Localizer;
use weft_ipc::{Connection, HostStatus, MemberStatus, NetworkStatus, PeerLink, Reach, Role, ServerStatus, Status};

use crate::ui::{MemberRow, NetworkCard, ServerRow};

/// How many avatar tones the theme has.
const AVATAR_TONES: u32 = 8;

/// The rows on screen, kept between polls so they are updated in place.
#[derive(Default)]
pub struct Networks {
    pub cards: Rc<VecModel<NetworkCard>>,
    members: HashMap<String, Rc<VecModel<MemberRow>>>,
    /// The network and its server behind every card, in the same order.
    pub shown: Vec<(ServerStatus, NetworkStatus)>,
}

impl Networks {
    pub fn update(&mut self, l: &Localizer, status: &Status, collapsed: &HashSet<String>) {
        let several = status.servers.len() > 1;
        let mut cards = Vec::new();
        let mut shown = Vec::new();
        for server in &status.servers {
            for network in &server.networks {
                let id = network_id(server, network);
                let members = self.members.entry(id.clone()).or_insert_with(|| Rc::new(VecModel::default())).clone();
                sync(&members, member_rows(l, status, server, network, several));
                cards.push(card(l, &id, server, network, several, collapsed.contains(&id), members));
                shown.push((server.clone(), network.clone()));
            }
        }
        let live: HashSet<SharedString> = cards.iter().map(|card| card.id.clone()).collect();
        self.members.retain(|id, _| live.contains(id.as_str()));
        sync(&self.cards, cards);
        self.shown = shown;
    }
}

pub fn network_id(server: &ServerStatus, network: &NetworkStatus) -> String {
    format!("{}/{}", server.server, network.name)
}

pub fn overall(servers: &[ServerStatus]) -> &'static str {
    if servers.iter().any(|server| server.connection == Connection::Connected) {
        "connected"
    } else if servers.iter().any(|server| server.connection == Connection::Connecting) {
        "connecting"
    } else {
        "disconnected"
    }
}

pub fn state_name(connection: Connection) -> &'static str {
    match connection {
        Connection::Connected => "connected",
        Connection::Connecting => "connecting",
        Connection::Disconnected => "disconnected",
    }
}

fn card(
    l: &Localizer,
    id: &str,
    server: &ServerStatus,
    network: &NetworkStatus,
    several: bool,
    collapsed: bool,
    members: Rc<VecModel<MemberRow>>,
) -> NetworkCard {
    let role = match network.role {
        Role::Owner => "role-owner",
        Role::Admin => "role-admin",
        Role::Member => "role-member",
    };
    let mut tags = vec![SharedString::from(l.tr(role))];
    if network.locked {
        tags.push(l.tr("network-locked").into());
    }
    if network.approval {
        tags.push(l.tr("network-approval").into());
    }
    if several {
        tags.push(server_name(l, server).into());
    }
    if l.language().is_rtl() {
        tags.reverse();
    }
    let online = network.members.iter().filter(|member| member.link != PeerLink::Offline).count();
    NetworkCard {
        id: id.into(),
        name: network.name.clone().into(),
        count: format!("{}/{}", online + 1, network.members.len() + 1).into(),
        tags: ModelRc::new(VecModel::from(tags)),
        members: ModelRc::from(members),
        collapsed,
        requests: network.requests as i32,
        manager: network.role != Role::Member,
    }
}

pub fn server_name(l: &Localizer, server: &ServerStatus) -> String {
    if server.public {
        l.tr("gui-mode-online")
    } else if server.hosted {
        l.tr("gui-mode-local")
    } else {
        server.host.clone()
    }
}

pub fn kind(server: &ServerStatus) -> &'static str {
    if server.hosted {
        "local"
    } else if server.public {
        "online"
    } else {
        "vps"
    }
}

/// Servers to list, with the hosted one even before the daemon has connected to it.
pub fn servers(status: &Status) -> Vec<ServerStatus> {
    let mut servers = status.servers.clone();
    if status.host.is_some() && !servers.iter().any(|server| server.hosted) {
        servers.insert(
            0,
            ServerStatus {
                server: String::new(),
                host: "127.0.0.1".into(),
                connection: Connection::Connecting,
                latency_ms: None,
                address: None,
                networks: Vec::new(),
                hosted: true,
                public: false,
            },
        );
    }
    servers
}

pub fn server_rows(l: &Localizer, status: &Status) -> Vec<ServerRow> {
    servers(status)
        .iter()
        .map(|server| {
            let latency = server.latency_ms.filter(|_| server.connection == Connection::Connected);
            let note = match (&status.host, server.hosted) {
                (Some(host), true) => reach_note(l, host),
                _ => String::new(),
            };
            ServerRow {
                kind: kind(server).into(),
                name: server_name(l, server).into(),
                state: state_name(server.connection).into(),
                host: if server.hosted { String::new() } else { server.host.clone() }.into(),
                ping: ping_text(l, latency, false).into(),
                level: level(latency),
                note: note.into(),
            }
        })
        .collect()
}

fn reach_note(l: &Localizer, host: &HostStatus) -> String {
    let reach = l.tr(match host.reach {
        Reach::Public => "host-reach-public",
        Reach::Local => "host-reach-local",
        Reach::Behind => "host-reach-behind",
    });
    if host.reach == Reach::Public && host.mapped {
        return reach;
    }
    let port = host.port.to_string();
    let mapping = l.tr_args(if host.mapped { "host-mapped" } else { "host-not-mapped" }, &[("port", &port)]);
    format!("{reach}. {mapping}")
}

fn member_rows(
    l: &Localizer,
    status: &Status,
    server: &ServerStatus,
    network: &NetworkStatus,
    several: bool,
) -> Vec<MemberRow> {
    let connected = server.connection == Connection::Connected;
    let to_server = server.latency_ms.filter(|_| connected);
    let state = format!("state-{}", state_name(server.connection));
    let me = MemberRow {
        nickname: status.nickname.clone().into(),
        initial: initial(&status.nickname).into(),
        tone: avatar_tone(&status.nickname),
        sub: if several { server_name(l, server) } else { l.tr(&state) }.into(),
        address: server.address.map(|address| address.to_string()).unwrap_or_default().into(),
        dns: SharedString::new(),
        ping: ping_text(l, to_server, false).into(),
        level: level(to_server),
        link: if connected { "direct" } else { "offline" }.into(),
        me: true,
    };
    let mut rows = vec![me];
    rows.extend(network.members.iter().map(|member| member_row(l, member, to_server)));
    rows
}

fn member_row(l: &Localizer, member: &MemberStatus, to_server: Option<u32>) -> MemberRow {
    let (link, sub, ms, estimate) = match member.link {
        PeerLink::Direct => ("direct", "link-direct", member.latency_ms, false),
        PeerLink::Relay => ("relay", "link-relay", to_server.map(|ms| ms * 2), true),
        PeerLink::Connecting => ("connecting", "link-connecting", None, false),
        PeerLink::Offline => ("offline", "link-offline", None, false),
    };
    MemberRow {
        nickname: member.nickname.clone().into(),
        initial: initial(&member.nickname).into(),
        tone: avatar_tone(&member.nickname),
        sub: l.tr(sub).into(),
        address: member.address.to_string().into(),
        dns: member.dns.clone().unwrap_or_default().into(),
        ping: ping_text(l, ms, estimate).into(),
        level: level(ms),
        link: link.into(),
        me: false,
    }
}

pub fn ping_text(l: &Localizer, ms: Option<u32>, estimate: bool) -> String {
    match ms {
        None => "\u{2014}".into(),
        Some(0) => l.tr_args("gui-ping-ms", &[("ms", "<1")]),
        Some(ms) => l.tr_args("gui-ping-ms", &[("ms", &format!("{}{ms}", if estimate { "~" } else { "" }))]),
    }
}

pub fn level(ms: Option<u32>) -> i32 {
    match ms {
        None => 0,
        Some(ms) if ms < 60 => 4,
        Some(ms) if ms < 120 => 3,
        Some(ms) if ms < 250 => 2,
        Some(_) => 1,
    }
}

fn initial(nickname: &str) -> String {
    nickname.chars().next().map_or_else(|| "?".into(), |c| c.to_uppercase().collect())
}

fn avatar_tone(nickname: &str) -> i32 {
    let hash = nickname.chars().fold(0u32, |hash, c| hash.wrapping_mul(31).wrapping_add(c as u32));
    (hash % AVATAR_TONES) as i32
}

/// Updates a model row by row, so unchanged rows keep their state and animations.
pub fn sync<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    for (index, row) in rows.iter().enumerate() {
        if index >= model.row_count() {
            model.push(row.clone());
        } else if model.row_data(index).as_ref() != Some(row) {
            model.set_row_data(index, row.clone());
        }
    }
    while model.row_count() > rows.len() {
        model.remove(model.row_count() - 1);
    }
}

/// A made-up state for looking at the window without a daemon.
pub fn demo() -> Status {
    use std::net::Ipv4Addr;
    let member = |nickname: &str, link, latency_ms, last| MemberStatus {
        nickname: nickname.into(),
        dns: Some(format!("{nickname}.weft")),
        address: Ipv4Addr::new(100, 64, 0, last),
        link,
        latency_ms,
    };
    Status {
        nickname: "qateralong".into(),
        public_key: String::new(),
        host: None,
        public_link: Some("weft://141.11.211.11:8443".into()),
        servers: vec![ServerStatus {
            server: "weft://141.11.211.11:8443".into(),
            host: "141.11.211.11:8443".into(),
            connection: Connection::Connected,
            latency_ms: Some(38),
            address: Some(Ipv4Addr::new(100, 64, 0, 1)),
            hosted: false,
            public: true,
            networks: vec![
                NetworkStatus {
                    name: "friends".into(),
                    role: Role::Owner,
                    locked: false,
                    approval: true,
                    requests: 1,
                    members: vec![
                        member("alice", PeerLink::Direct, Some(27), 2),
                        member("bob", PeerLink::Relay, None, 3),
                        member("carol", PeerLink::Offline, None, 4),
                    ],
                },
                NetworkStatus {
                    name: "minecraft".into(),
                    role: Role::Member,
                    locked: true,
                    approval: false,
                    requests: 0,
                    members: vec![member("dave", PeerLink::Direct, Some(143), 9)],
                },
            ],
        }],
    }
}
