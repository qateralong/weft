//! Desktop notifications about members and the connection, from consecutive status snapshots.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;

use weft_i18n::Localizer;
use weft_ipc::{Connection, NetworkStatus, PeerLink, Status};

use crate::settings::Notify;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Online {
        nickname: String,
        network: String,
        address: Ipv4Addr,
    },
    Offline {
        nickname: String,
        network: String,
        address: Ipv4Addr,
    },
    Joined {
        nickname: String,
        network: String,
        address: Ipv4Addr,
    },
    Request {
        network: String,
    },
    /// This device's own join request was answered.
    Approved {
        network: String,
    },
    Denied {
        network: String,
    },
    Disconnected,
    Reconnected,
}

impl Event {
    /// The member, by nickname and address so namesakes stay apart, and whether this event says
    /// they are online.
    fn presence(&self) -> Option<((&str, Ipv4Addr), bool)> {
        match self {
            Event::Online { nickname, address, .. } | Event::Joined { nickname, address, .. } => {
                Some(((nickname, *address), true))
            }
            Event::Offline { nickname, address, .. } => Some(((nickname, *address), false)),
            _ => None,
        }
    }

    /// The kind of notification, as in `settings::Notify`.
    fn kind(&self) -> &'static str {
        match self {
            Event::Online { .. } => "online",
            Event::Offline { .. } => "offline",
            Event::Joined { .. } => "joined",
            Event::Request { .. } | Event::Approved { .. } | Event::Denied { .. } => "requests",
            Event::Disconnected | Event::Reconnected => "connection",
        }
    }

    fn text(&self, l: &Localizer) -> String {
        match self {
            Event::Online { nickname, network, .. } => {
                l.tr_args("notify-online", &[("nickname", nickname), ("network", network)])
            }
            Event::Offline { nickname, network, .. } => {
                l.tr_args("notify-offline", &[("nickname", nickname), ("network", network)])
            }
            Event::Joined { nickname, network, .. } => {
                l.tr_args("notify-joined", &[("nickname", nickname), ("network", network)])
            }
            Event::Request { network } => l.tr_args("notify-request", &[("network", network)]),
            Event::Approved { network } => l.tr_args("notify-approved", &[("network", network)]),
            Event::Denied { network } => l.tr_args("notify-denied", &[("network", network)]),
            Event::Disconnected => l.tr("notify-disconnected"),
            Event::Reconnected => l.tr("notify-reconnected"),
        }
    }
}

/// Compares two status snapshots taken a few seconds apart; `lost` tells whether the connection
/// dropped by itself before `previous`. Turning Weft off and on is not news: the daemon then
/// reports `Disconnected`, while a dropped connection is `Connecting` until it comes back.
pub fn events(previous: &Status, next: &Status, lost: bool) -> Vec<Event> {
    let connected = |status: &Status| status.connection() == Connection::Connected;
    match (connected(previous), connected(next)) {
        (true, false) if next.connection() == Connection::Connecting => return vec![Event::Disconnected],
        (false, true) if lost => return vec![Event::Reconnected],
        (true, true) => {}
        _ => return Vec::new(),
    }

    let online = |link: PeerLink| link != PeerLink::Offline;
    let networks = |status: &'_ Status| -> Vec<(String, NetworkStatus)> {
        status
            .servers
            .iter()
            .flat_map(|server| {
                server.networks.iter().map(|network| (format!("{}/{}", server.host, network.name), network.clone()))
            })
            .collect()
    };
    let (old, new) = (networks(previous), networks(next));
    let before: HashMap<(&str, Ipv4Addr), PeerLink> = old
        .iter()
        .flat_map(|(id, network)| network.members.iter().map(move |m| ((id.as_str(), m.address), m.link)))
        .collect();
    let mut events = Vec::new();
    for server in &next.servers {
        let Some(known) = previous.servers.iter().find(|known| known.host == server.host) else { continue };
        for name in known.waiting.iter().filter(|name| !server.waiting.contains(name)) {
            let joined = server.networks.iter().any(|network| network.name.eq_ignore_ascii_case(name));
            let network = name.clone();
            events.push(if joined { Event::Approved { network } } else { Event::Denied { network } });
        }
    }
    let mut reported = HashSet::new();
    for (id, network) in &new {
        let Some((_, known)) = old.iter().find(|(known, _)| known == id) else { continue };
        if network.requests > known.requests {
            events.push(Event::Request { network: network.name.clone() });
        }
        for member in &network.members {
            let (nickname, network, address) = (member.nickname.clone(), network.name.clone(), member.address);
            let event = match before.get(&(id.as_str(), address)) {
                None => Some(Event::Joined { nickname, network, address }),
                Some(&link) if !online(link) && online(member.link) => {
                    Some(Event::Online { nickname, network, address })
                }
                Some(&link) if online(link) && !online(member.link) => {
                    Some(Event::Offline { nickname, network, address })
                }
                Some(_) => None,
            };
            if let Some(event) = event
                && (matches!(event, Event::Joined { .. }) || reported.insert(member.address))
            {
                events.push(event);
            }
        }
    }
    events
}

/// Remembers the previous snapshot and what was already announced.
#[derive(Default)]
pub struct Notifier {
    previous: Option<Status>,
    lost: bool,
    announced: HashMap<(String, Ipv4Addr), bool>,
}

impl Notifier {
    /// Shows a notification for every change since the last snapshot of the kinds `wanted`.
    pub fn observe(&mut self, status: &Status, wanted: Option<&Notify>, l: &Localizer) {
        if let Some(previous) = &self.previous
            && let Some(wanted) = wanted
        {
            for event in events(previous, status, self.lost) {
                if let Some(((nickname, address), online)) = event.presence()
                    && self.announced.insert((nickname.to_string(), address), online) == Some(online)
                {
                    continue;
                }
                if wanted.get(event.kind()) {
                    show(event.text(l));
                }
            }
        }
        self.lost = match status.connection() {
            Connection::Connected | Connection::Disconnected => false,
            Connection::Connecting => {
                self.lost
                    || self.previous.as_ref().is_some_and(|previous| previous.connection() == Connection::Connected)
            }
        };
        self.previous = Some(status.clone());
    }
}

fn show(text: String) {
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new().appname("Weft").summary("Weft").body(&text).show();
    });
}

#[cfg(test)]
mod tests {
    use weft_ipc::{MemberStatus, Role, ServerStatus};

    use super::*;

    fn status(connection: Connection, members: &[(&str, u8, PeerLink)], requests: u32) -> Status {
        Status {
            nickname: "me".into(),
            public_key: String::new(),
            host: None,
            public_link: None,
            avatar: None,
            servers: vec![ServerStatus {
                hosted: false,
                public: false,
                waiting: Vec::new(),
                server: "weft://example.com".into(),
                host: "example.com:443".into(),
                connection,
                latency_ms: None,
                refused: None,
                address: Some(Ipv4Addr::new(100, 64, 0, 1)),
                networks: vec![NetworkStatus {
                    name: "lan".into(),
                    role: Role::Owner,
                    locked: false,
                    approval: true,
                    requests,
                    muted_until: None,
                    members: members
                        .iter()
                        .map(|&(nickname, host, link)| MemberStatus {
                            nickname: nickname.into(),
                            dns: None,
                            address: Ipv4Addr::new(100, 64, 0, host),
                            link,
                            latency_ms: None,
                            role: None,
                            muted_until: None,
                            avatar: None,
                        })
                        .collect(),
                }],
            }],
        }
    }

    #[test]
    fn detects_changes() {
        let lan = || "lan".to_string();
        let before = status(Connection::Connected, &[("bob", 2, PeerLink::Offline), ("eve", 3, PeerLink::Direct)], 0);
        let after = status(
            Connection::Connected,
            &[("bob", 2, PeerLink::Relay), ("eve", 3, PeerLink::Offline), ("ann", 4, PeerLink::Direct)],
            1,
        );
        assert_eq!(
            events(&before, &after, true),
            [
                Event::Request { network: lan() },
                Event::Online { nickname: "bob".into(), network: lan(), address: Ipv4Addr::new(100, 64, 0, 2) },
                Event::Offline { nickname: "eve".into(), network: lan(), address: Ipv4Addr::new(100, 64, 0, 3) },
                Event::Joined { nickname: "ann".into(), network: lan(), address: Ipv4Addr::new(100, 64, 0, 4) },
            ]
        );
        assert!(events(&after, &after, true).is_empty());
    }

    #[test]
    fn connection_changes_hide_member_changes() {
        let online = status(Connection::Connected, &[("bob", 2, PeerLink::Direct)], 0);
        let lost = status(Connection::Connecting, &[], 0);
        assert_eq!(events(&online, &lost, false), [Event::Disconnected]);
        assert_eq!(events(&lost, &online, true), [Event::Reconnected]);
        assert!(events(&lost, &online, false).is_empty());
    }

    #[test]
    fn turning_off_and_on_is_quiet() {
        let online = status(Connection::Connected, &[("bob", 2, PeerLink::Direct)], 0);
        let off = status(Connection::Disconnected, &[], 0);
        let starting = status(Connection::Connecting, &[], 0);
        let l = Localizer::new(weft_i18n::Language::English);
        let mut notifier = Notifier::default();
        for snapshot in [&online, &off, &starting, &online] {
            notifier.observe(snapshot, None, &l);
            assert!(!notifier.lost);
        }
        assert!(events(&online, &off, false).is_empty());
        assert!(events(&starting, &online, false).is_empty());
        notifier.observe(&starting, None, &l);
        assert!(notifier.lost);
    }

    #[test]
    fn answered_requests() {
        let mut waiting = status(Connection::Connected, &[], 0);
        waiting.servers[0].waiting = vec!["lan".into(), "club".into()];
        let answered = status(Connection::Connected, &[], 0);
        assert_eq!(
            events(&waiting, &answered, false),
            [Event::Approved { network: "lan".into() }, Event::Denied { network: "club".into() }]
        );
    }
}
