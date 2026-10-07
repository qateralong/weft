use std::fmt;
use std::str::FromStr;

use data_encoding::BASE32_NOPAD;

pub const KEY_LEN: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PublicKey([u8; KEY_LEN]);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("key is not valid base32")]
    Encoding,
    #[error("key must be {KEY_LEN} bytes, got {0}")]
    Length(usize),
}

impl PublicKey {
    pub const fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self, KeyError> {
        bytes.try_into().map(Self).map_err(|_| KeyError::Length(bytes.len()))
    }

    pub const fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&BASE32_NOPAD.encode(&self.0).to_ascii_lowercase())
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = self.to_string();
        write!(f, "PublicKey({}…)", &text[..8])
    }
}

impl FromStr for PublicKey {
    type Err = KeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = BASE32_NOPAD.decode(s.to_ascii_uppercase().as_bytes()).map_err(|_| KeyError::Encoding)?;
        Self::from_slice(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_roundtrip() {
        let key = PublicKey::from_bytes(std::array::from_fn(|i| i as u8 * 7));
        let text = key.to_string();
        assert_eq!(text.len(), 52);
        assert!(text.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert_eq!(text.parse::<PublicKey>().unwrap(), key);
        assert_eq!(text.to_ascii_uppercase().parse::<PublicKey>().unwrap(), key);
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!("not base32!".parse::<PublicKey>(), Err(KeyError::Encoding));
        assert_eq!("a".parse::<PublicKey>(), Err(KeyError::Encoding));
        assert_eq!("aaaaaaaa".parse::<PublicKey>(), Err(KeyError::Length(5)));
    }
}
