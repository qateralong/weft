use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub public_host: Option<String>,
    pub data_dir: PathBuf,
    pub pool: Pool,
    pub max_members: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 443)),
            public_host: None,
            data_dir: PathBuf::from("/var/lib/loom"),
            pool: Pool::DEFAULT,
            max_members: 250,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("invalid config {path}: {source}")]
    Parse { path: PathBuf, source: toml::de::Error },
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text =
            std::fs::read_to_string(path).map_err(|source| ConfigError::Read { path: path.to_path_buf(), source })?;
        toml::from_str(&text).map_err(|source| ConfigError::Parse { path: path.to_path_buf(), source })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct Pool {
    network: u32,
    prefix: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("pool must look like 100.64.0.0/10 with a prefix from 8 to 30")]
pub struct PoolError;

impl Pool {
    pub const DEFAULT: Pool = Pool { network: 0x6440_0000, prefix: 10 };

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    pub fn first_host(&self) -> u32 {
        self.network + 1
    }

    pub fn last_host(&self) -> u32 {
        self.broadcast() - 1
    }

    pub fn broadcast(&self) -> u32 {
        self.network | (u32::MAX >> self.prefix)
    }
}

impl FromStr for Pool {
    type Err = PoolError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (ip, prefix) = s.split_once('/').ok_or(PoolError)?;
        let ip: Ipv4Addr = ip.parse().map_err(|_| PoolError)?;
        let prefix: u8 = prefix.parse().map_err(|_| PoolError)?;
        if !(8..=30).contains(&prefix) {
            return Err(PoolError);
        }
        let mask = u32::MAX << (32 - prefix);
        Ok(Pool { network: u32::from(ip) & mask, prefix })
    }
}

impl TryFrom<String> for Pool {
    type Error = PoolError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool() {
        let pool: Pool = "100.64.0.0/10".parse().unwrap();
        assert_eq!(pool, Pool::DEFAULT);
        assert_eq!(Ipv4Addr::from(pool.first_host()), Ipv4Addr::new(100, 64, 0, 1));
        assert_eq!(Ipv4Addr::from(pool.last_host()), Ipv4Addr::new(100, 127, 255, 254));
        assert_eq!(Ipv4Addr::from(pool.broadcast()), Ipv4Addr::new(100, 127, 255, 255));
        assert_eq!("10.1.2.3/24".parse::<Pool>().unwrap().first_host(), u32::from(Ipv4Addr::new(10, 1, 2, 1)));
        for bad in ["10.0.0.0", "10.0.0.0/7", "10.0.0.0/31", "x/10", "10.0.0.0/x"] {
            assert!(bad.parse::<Pool>().is_err(), "{bad}");
        }
    }

    #[test]
    fn config_file() {
        let config: Config = toml::from_str(
            r#"
            listen = "0.0.0.0:7443"
            public_host = "loom.example.com"
            pool = "10.10.0.0/16"
            "#,
        )
        .unwrap();
        assert_eq!(config.listen.port(), 7443);
        assert_eq!(config.pool.prefix(), 16);
        assert_eq!(config.max_members, 250);
        assert!(toml::from_str::<Config>("unknown = 1").is_err());
    }
}
