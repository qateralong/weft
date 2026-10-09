//! The app's own preferences, kept as JSON in the user's config directory.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use weft_ipc::PanelAccess;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Settings {
    pub notifications: bool,
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
pub struct Release {
    pub tag: String,
    pub url: String,
    pub checked: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            notifications: true,
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
