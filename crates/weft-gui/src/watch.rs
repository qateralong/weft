use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;
use weft_i18n::Localizer;
use weft_ipc::{Connection, NetworkStatus, PeerLink, Request, Response, Status};

const POLL: Duration = Duration::from_secs(2);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Settings {
    pub notifications: bool,
    pub updates: bool,
    /// A language code; the system language when unset.
    pub language: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { notifications: true, updates: true, language: None }
    }
}

pub struct SettingsStore {
    path: Option<PathBuf>,
    current: Mutex<Settings>,
}

impl SettingsStore {
    pub fn load(app: &AppHandle) -> Self {
        let path = app.path().app_config_dir().ok().map(|dir| dir.join("settings.json"));
        let current = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self { path, current: Mutex::new(current) }
    }

    pub fn get(&self) -> Settings {
        self.current.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    pub fn set(&self, settings: Settings) -> Result<(), String> {
        let text = serde_json::to_string_pretty(&settings).map_err(|error| error.to_string())?;
        *self.current.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = settings;
        let Some(path) = &self.path else { return Ok(()) };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        }
        std::fs::write(path, text).map_err(|error| error.to_string())
    }
}

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

pub async fn run(app: AppHandle) {
    let mut previous: Option<Status> = None;
    let mut was_connected = false;
    let mut announced: HashMap<String, bool> = HashMap::new();
    loop {
        tokio::time::sleep(POLL).await;
        restart_if_replaced(&app);
        let l = app.state::<Localizer>();
        let Ok(Response::Status(status)) = crate::send(&l, Request::Status).await else { continue };
        if let Some(previous) = &previous
            && app.state::<SettingsStore>().get().notifications
        {
            for event in events(previous, &status, was_connected) {
                if let Some((nickname, online)) = event.presence()
                    && announced.insert(nickname.to_string(), online) == Some(online)
                {
                    continue;
                }
                let _ = app.notification().builder().title("Weft").body(event.text(&l)).show();
            }
        }
        was_connected |= status.connection() == Connection::Connected;
        previous = Some(status);
    }
}

/// After a package update the running binary is gone; start the new one in its place, keeping
/// the window hidden or shown as it was.
#[cfg(target_os = "linux")]
fn restart_if_replaced(app: &AppHandle) {
    let Ok(exe) = std::fs::read_link("/proc/self/exe") else { return };
    let Some(path) = exe.to_str().and_then(|path| path.strip_suffix(" (deleted)")) else { return };
    if !std::path::Path::new(path).exists() {
        return;
    }
    let visible = app.get_webview_window("main").is_some_and(|window| window.is_visible().unwrap_or(false));
    let hidden = if visible { "" } else { " --hidden" };
    let script = format!("sleep 1; exec '{}'{hidden}", path.replace('\'', ""));
    if std::process::Command::new("setsid").args(["-f", "sh", "-c", &script]).spawn().is_ok() {
        app.exit(0);
    }
}

#[cfg(not(target_os = "linux"))]
fn restart_if_replaced(_app: &AppHandle) {}

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
