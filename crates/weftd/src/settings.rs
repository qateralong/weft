use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Older single-server settings, moved into `servers` on load.
    #[serde(skip_serializing)]
    pub server: Option<String>,
    #[serde(skip_serializing)]
    pub up: bool,
    pub nickname: Option<String>,
    /// The virtual address asked from every server, so it is the same everywhere.
    pub address: Option<Ipv4Addr>,
    pub port: Option<u16>,
    pub broadcast: bool,
    pub multicast_groups: Vec<Ipv4Addr>,
    /// Resolve peer names like bob.weft through the system resolver.
    pub dns: bool,
    /// How the control connection to the server looks on the wire.
    pub transport: Transport,
    /// Whether the public server was added once: `Some(false)` after the user removed it.
    pub public_server: Option<bool>,
    pub host: HostSettings,
    pub servers: Vec<ServerEntry>,
}

/// A server hosted by the daemon.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostSettings {
    pub enabled: bool,
    /// 0 picks a free port; the chosen one is kept so links stay valid.
    pub port: u16,
    /// Host name or address to put in links, when the automatic one is wrong.
    pub address: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerEntry {
    pub link: String,
    /// Whether to stay connected.
    #[serde(default)]
    pub up: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// TLS that looks like HTTPS, with the Noise stream inside.
    #[default]
    Tls,
    /// The bare Noise stream, as in Weft 0.1.0.
    Raw,
}

pub const MINECRAFT_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 2, 60);

/// The server every new installation joins, so the app works without setup.
pub const PUBLIC_SERVER: &str = "weft://141.11.211.11:8443#k=g4xfbqwhwx3slxpw73sszriu2w5bsgms7ahkso2mxuk7253bbn7a";

/// The public server link; `WEFT_PUBLIC_SERVER` replaces it, and an empty value turns it off.
pub fn public_server() -> Option<weft_proto::Link> {
    let link = std::env::var("WEFT_PUBLIC_SERVER").unwrap_or_else(|_| PUBLIC_SERVER.to_string());
    link.parse::<weft_proto::Link>().ok().map(|link| link.server())
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server: None,
            nickname: None,
            address: None,
            port: None,
            up: false,
            broadcast: true,
            multicast_groups: vec![MINECRAFT_GROUP],
            dns: true,
            transport: Transport::Tls,
            public_server: None,
            host: HostSettings::default(),
            servers: Vec::new(),
        }
    }
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
            Ok(text) => {
                let mut settings: Settings =
                    toml::from_str(&text).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                if let Some(link) = settings.server.take()
                    && settings.servers.is_empty()
                {
                    settings.servers.push(ServerEntry { link, up: settings.up });
                }
                settings.up = false;
                Ok(settings)
            }
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
        let settings = Settings {
            nickname: Some("n".into()),
            port: Some(4000),
            servers: vec![
                ServerEntry { link: "weft://x#k=y".into(), up: true },
                ServerEntry { link: "weft://z#k=y".into(), up: false },
            ],
            ..Settings::default()
        };
        file.save(&settings).unwrap();
        assert_eq!(file.load().unwrap(), settings);
        std::fs::write(file.path(), "server = \"weft://old#k=y\"\nup = true\n").unwrap();
        let loaded = file.load().unwrap();
        assert!(loaded.broadcast);
        assert_eq!(loaded.multicast_groups, vec![MINECRAFT_GROUP]);
        assert_eq!(loaded.servers, vec![ServerEntry { link: "weft://old#k=y".into(), up: true }]);
        assert!(loaded.server.is_none() && !loaded.up);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!default_nickname().is_empty());
    }
}
