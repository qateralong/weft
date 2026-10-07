use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use crate::key::PublicKey;

pub const SCHEME: &str = "weft://";
pub const DEFAULT_PORT: u16 = 443;
const MAX_INVITE_LEN: usize = 64;
const MAX_DOMAIN_LEN: usize = 253;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Host {
    Domain(String),
    Ipv4(Ipv4Addr),
    Ipv6(Ipv6Addr),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub host: Host,
    pub port: u16,
    pub invite: Option<String>,
    pub server_key: PublicKey,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("link must start with {SCHEME}")]
    Scheme,
    #[error("invalid host")]
    Host,
    #[error("invalid port")]
    Port,
    #[error("invalid invite code")]
    Invite,
    #[error("server key is missing")]
    MissingKey,
    #[error("invalid server key")]
    Key,
}

impl Link {
    pub fn server(&self) -> Link {
        Link { invite: None, ..self.clone() }
    }
}

impl FromStr for Link {
    type Err = LinkError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rest = s
            .get(..SCHEME.len())
            .filter(|scheme| scheme.eq_ignore_ascii_case(SCHEME))
            .map(|_| &s[SCHEME.len()..])
            .ok_or(LinkError::Scheme)?;
        let (rest, fragment) = rest.split_once('#').ok_or(LinkError::MissingKey)?;
        let server_key = parse_fragment(fragment)?;
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (host, port) = parse_authority(authority)?;
        let invite = parse_invite(path)?;
        Ok(Link { host, port, invite, server_key })
    }
}

impl fmt::Display for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(SCHEME)?;
        match &self.host {
            Host::Domain(domain) => f.write_str(domain)?,
            Host::Ipv4(ip) => write!(f, "{ip}")?,
            Host::Ipv6(ip) => write!(f, "[{ip}]")?,
        }
        if self.port != DEFAULT_PORT {
            write!(f, ":{}", self.port)?;
        }
        if let Some(invite) = &self.invite {
            write!(f, "/{invite}")?;
        }
        write!(f, "#k={}", self.server_key)
    }
}

fn parse_fragment(fragment: &str) -> Result<PublicKey, LinkError> {
    let value = fragment
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find_map(|(name, value)| (name == "k").then_some(value))
        .ok_or(LinkError::MissingKey)?;
    value.parse().map_err(|_| LinkError::Key)
}

fn parse_authority(authority: &str) -> Result<(Host, u16), LinkError> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (ip, after) = rest.split_once(']').ok_or(LinkError::Host)?;
        let ip = ip.parse().map_err(|_| LinkError::Host)?;
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':').ok_or(LinkError::Port)?),
        };
        (Host::Ipv6(ip), port)
    } else {
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        (parse_host(host)?, port)
    };
    let port = match port {
        None => DEFAULT_PORT,
        Some(port) => port.parse().ok().filter(|&port| port != 0).ok_or(LinkError::Port)?,
    };
    Ok((host, port))
}

fn parse_host(host: &str) -> Result<Host, LinkError> {
    if let Ok(ip) = host.parse() {
        return Ok(Host::Ipv4(ip));
    }
    let valid_label = |label: &str| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() || host.len() > MAX_DOMAIN_LEN || !host.split('.').all(valid_label) {
        return Err(LinkError::Host);
    }
    if host.split('.').next_back().is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit())) {
        return Err(LinkError::Host);
    }
    Ok(Host::Domain(host.to_ascii_lowercase()))
}

fn parse_invite(path: &str) -> Result<Option<String>, LinkError> {
    let path = path.strip_suffix('/').unwrap_or(path);
    if path.is_empty() {
        return Ok(None);
    }
    let valid = path.len() <= MAX_INVITE_LEN && path.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !valid {
        return Err(LinkError::Invite);
    }
    Ok(Some(path.to_ascii_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> PublicKey {
        PublicKey::from_bytes([42; 32])
    }

    #[test]
    fn server_link() {
        let text = format!("weft://Loom.Example.com#k={}", key());
        let link: Link = text.parse().unwrap();
        assert_eq!(link.host, Host::Domain("loom.example.com".into()));
        assert_eq!(link.port, DEFAULT_PORT);
        assert_eq!(link.invite, None);
        assert_eq!(link.server_key, key());
        assert_eq!(link.to_string(), format!("weft://loom.example.com#k={}", key()));
    }

    #[test]
    fn invite_link_with_port() {
        let link: Link = format!("weft://203.0.113.5:7000/k7qf-m2xa#k={}", key()).parse().unwrap();
        assert_eq!(link.host, Host::Ipv4(Ipv4Addr::new(203, 0, 113, 5)));
        assert_eq!(link.port, 7000);
        assert_eq!(link.invite.as_deref(), Some("K7QF-M2XA"));
        assert_eq!(link.server().invite, None);
    }

    #[test]
    fn ipv6_host() {
        let link: Link = format!("weft://[2001:db8::1]:8443/ABC#k={}", key()).parse().unwrap();
        assert_eq!(link.host, Host::Ipv6("2001:db8::1".parse().unwrap()));
        assert_eq!(link.port, 8443);
        assert_eq!(link.to_string().parse::<Link>().unwrap(), link);
    }

    #[test]
    fn unknown_fragment_params_are_ignored() {
        let link: Link = format!("weft://host.lan#v=2&k={}&x", key()).parse().unwrap();
        assert_eq!(link.server_key, key());
    }

    #[test]
    fn roundtrip() {
        for text in [
            format!("weft://loom.example.com#k={}", key()),
            format!("weft://loom.example.com:444/ABCD-1234#k={}", key()),
            format!("weft://[::1]#k={}", key()),
        ] {
            assert_eq!(text.parse::<Link>().unwrap().to_string(), text);
        }
    }

    #[test]
    fn errors() {
        let k = key();
        let cases = [
            (format!("http://host#k={k}"), LinkError::Scheme),
            ("weft://host".to_string(), LinkError::MissingKey),
            ("weft://host#x=1".to_string(), LinkError::MissingKey),
            ("weft://host#k=zz".to_string(), LinkError::Key),
            (format!("weft://#k={k}"), LinkError::Host),
            (format!("weft://ho_st#k={k}"), LinkError::Host),
            (format!("weft://-host#k={k}"), LinkError::Host),
            (format!("weft://1.2.3.999#k={k}"), LinkError::Host),
            (format!("weft://[::1#k={k}"), LinkError::Host),
            (format!("weft://host:0#k={k}"), LinkError::Port),
            (format!("weft://host:99999#k={k}"), LinkError::Port),
            (format!("weft://[::1]x#k={k}"), LinkError::Port),
            (format!("weft://host/a/b#k={k}"), LinkError::Invite),
            (format!("weft://host/a b#k={k}"), LinkError::Invite),
        ];
        for (text, error) in cases {
            assert_eq!(text.parse::<Link>(), Err(error), "{text}");
        }
    }
}
