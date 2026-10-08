//! Desktop notifications about members and the connection, from consecutive status snapshots.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;

use weft_i18n::Localizer;
use weft_ipc::{Connection, NetworkStatus, PeerLink, Status};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Online { nickname: String, network: String },
    Offline { nickname: String, network: String },
    Joined { nickname: String, network: String },
    Request { network: String },
    Disconnected,
    Reconnected,
}

impl Event {
    /// The member and whether this event says they are online.
    fn presence(&self) -> Option<(&str, bool)> {
        match self {
            Event::Online { nickname, .. } | Event::Joined { nickname, .. } => Some((nickname, true)),
            Event::Offline { nickname, .. } => Some((nickname, false)),
            _ => None,
        }
    }

    fn text(&self, l: &Localizer) -> String {
        match self {
            Event::Online { nickname, network } => {
                l.tr_args("notify-online", &[("nickname", nickname), ("network", network)])
            }
            Event::Offline { nickname, network } => {
                l.tr_args("notify-offline", &[("nickname", nickname), ("network", network)])
            }
            Event::Joined { nickname, network } => {
                l.tr_args("notify-joined", &[("nickname", nickname), ("network", network)])
            }
            Event::Request { network } => l.tr_args("notify-request", &[("network", network)]),
            Event::Disconnected => l.tr("notify-disconnected"),
            Event::Reconnected => l.tr("notify-reconnected"),
        }
    }
}

/// Compares two status snapshots taken a few seconds apart; `was_connected` tells whether the
/// connection to the server ever came up before `previous`.
pub fn events(previous: &Status, next: &Status, was_connected: bool) -> Vec<Event> {
    let connected = |status: &Status| status.connection() == Connection::Connected;
    match (connected(previous), connected(next)) {
        (true, false) if !next.servers.is_empty() => return vec![Event::Disconnected],
        (false, true) if was_connected && !previous.servers.is_empty() => return vec![Event::Reconnected],
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
    let mut reported = HashSet::new();
    for (id, network) in &new {
        let Some((_, known)) = old.iter().find(|(known, _)| known == id) else { continue };
        if network.requests > known.requests {
            events.push(Event::Request { network: network.name.clone() });
        }
        for member in &network.members {
            let (nickname, network) = (member.nickname.clone(), network.name.clone());
            let event = match before.get(&(id.as_str(), member.address)) {
                None => Some(Event::Joined { nickname, network }),
                Some(&link) if !online(link) && online(member.link) => Some(Event::Online { nickname, network }),
                Some(&link) if online(link) && !online(member.link) => Some(Event::Offline { nickname, network }),
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
    was_connected: bool,
    announced: HashMap<String, bool>,
}

impl Notifier {
    /// Shows a notification for every change since the last snapshot when `enabled`.
    pub fn observe(&mut self, status: &Status, enabled: bool, l: &Localizer) {
        if let Some(previous) = &self.previous
            && enabled
        {
            for event in events(previous, status, self.was_connected) {
                if let Some((nickname, online)) = event.presence()
                    && self.announced.insert(nickname.to_string(), online) == Some(online)
                {
                    continue;
                }
                show(event.text(l));
            }
        }
        self.was_connected |= status.connection() == Connection::Connected;
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
            servers: vec![ServerStatus {
                hosted: false,
                public: false,
                server: "weft://example.com".into(),
                host: "example.com:443".into(),
                connection,
                latency_ms: None,
                address: Some(Ipv4Addr::new(100, 64, 0, 1)),
                networks: vec![NetworkStatus {
                    name: "lan".into(),
                    role: Role::Owner,
                    locked: false,
                    approval: true,
                    requests,
                    members: members
                        .iter()
                        .map(|&(nickname, host, link)| MemberStatus {
                            nickname: nickname.into(),
                            dns: None,
                            address: Ipv4Addr::new(100, 64, 0, host),
                            link,
                            latency_ms: None,
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
                Event::Online { nickname: "bob".into(), network: lan() },
                Event::Offline { nickname: "eve".into(), network: lan() },
                Event::Joined { nickname: "ann".into(), network: lan() },
            ]
        );
        assert!(events(&after, &after, true).is_empty());
    }

    #[test]
    fn connection_changes_hide_member_changes() {
        let online = status(Connection::Connected, &[("bob", 2, PeerLink::Direct)], 0);
        let lost = status(Connection::Connecting, &[], 0);
        assert_eq!(events(&online, &lost, true), [Event::Disconnected]);
        assert_eq!(events(&lost, &online, true), [Event::Reconnected]);
        assert!(events(&lost, &online, false).is_empty());
    }
}
