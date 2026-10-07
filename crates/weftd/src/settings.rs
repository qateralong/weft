use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub server: Option<String>,
    pub nickname: Option<String>,
    pub port: Option<u16>,
    pub up: bool,
}

pub struct SettingsFile {
    path: PathBuf,
}

impl SettingsFile {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> io::Result<Settings> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => toml::from_str(&text).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, settings: &Settings) -> io::Result<()> {
        let text = toml::to_string(settings).map_err(io::Error::other)?;
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn default_nickname() -> String {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| {
            let output = std::process::Command::new("hostname").output().ok()?;
            String::from_utf8(output.stdout).ok()
        })
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "weft".to_string());
    host.chars().take(32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("weftd-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = SettingsFile::new(dir.join("weftd.toml"));
        assert_eq!(file.load().unwrap(), Settings::default());
        let settings =
            Settings { server: Some("weft://x#k=y".into()), nickname: Some("n".into()), port: Some(4000), up: true };
        file.save(&settings).unwrap();
        assert_eq!(file.load().unwrap(), settings);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!default_nickname().is_empty());
    }
}
