//! The app's own preferences, kept as JSON in the user's config directory.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use weft_ipc::PanelAccess;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Settings {
    pub notifications: bool,
    /// Which kinds of notifications to show while they are on.
    pub notify: Notify,
    pub updates: bool,
    /// A language code; the system language when unset.
    pub language: Option<String>,
    /// The chosen theme; the system one when unset.
    pub dark: Option<bool>,
    /// Networks shown folded, as `server/name`.
    pub collapsed: Vec<String>,
    /// The latest release seen and when it was checked.
    pub release: Option<Release>,
    /// Web panels of servers installed from this app, by server link.
    pub panels: BTreeMap<String, PanelAccess>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Notify {
    pub online: bool,
    pub offline: bool,
    pub joined: bool,
    pub requests: bool,
    pub connection: bool,
}

impl Default for Notify {
    fn default() -> Self {
        Self { online: true, offline: true, joined: true, requests: true, connection: true }
    }
}

impl Notify {
    pub const KINDS: [&str; 5] = ["online", "offline", "joined", "requests", "connection"];

    pub fn get(&self, kind: &str) -> bool {
        match kind {
            "online" => self.online,
            "offline" => self.offline,
            "joined" => self.joined,
            "requests" => self.requests,
            _ => self.connection,
        }
    }

    pub fn set(&mut self, kind: &str, on: bool) {
        match kind {
            "online" => self.online = on,
            "offline" => self.offline = on,
            "joined" => self.joined = on,
            "requests" => self.requests = on,
            _ => self.connection = on,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub url: String,
    pub checked: u64,
    /// Downloadable files: name, address and SHA-256 when GitHub gives one.
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub sha256: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            notifications: true,
            notify: Notify::default(),
            updates: true,
            language: None,
            dark: None,
            collapsed: Vec::new(),
            release: None,
            panels: BTreeMap::new(),
        }
    }
}

pub struct Store {
    path: Option<PathBuf>,
    pub current: Settings,
}

impl Store {
    pub fn load() -> Self {
        let path = dirs::config_dir().map(|dir| dir.join("weft").join("gui.json"));
        let current = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self { path, current }
    }

    /// Applies `change` and writes the result; a failed write only loses persistence.
    pub fn update(&mut self, change: impl FnOnce(&mut Settings)) {
        change(&mut self.current);
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(&self.current) {
            let _ = std::fs::write(path, text);
        }
    }
}
