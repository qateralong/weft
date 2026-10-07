use std::fmt;
use std::io::{self, Write};
use std::path::Path;

use weft_proto::{KEY_LEN, PublicKey};
use x25519_dalek::StaticSecret;
use zeroize::Zeroizing;

pub struct StaticKeypair {
    secret: Zeroizing<[u8; KEY_LEN]>,
    public: PublicKey,
}

impl StaticKeypair {
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut secret = Zeroizing::new([0; KEY_LEN]);
        getrandom::fill(secret.as_mut())?;
        Ok(Self::from_secret(&secret))
    }

    pub fn from_secret(secret: &[u8; KEY_LEN]) -> Self {
        let scalar = StaticSecret::from(*secret);
        let public = PublicKey::from_bytes(x25519_dalek::PublicKey::from(&scalar).to_bytes());
        Self { secret: Zeroizing::new(*secret), public }
    }

    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        match Self::load(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => match Self::create(path) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Self::load(path),
                result => result,
            },
            result => result,
        }
    }

    fn load(path: &Path) -> io::Result<Self> {
        let bytes = std::fs::read(path)?;
        let secret: [u8; KEY_LEN] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "key file is corrupted"))?;
        Ok(Self::from_secret(&secret))
    }

    fn create(path: &Path) -> io::Result<Self> {
        let keypair = Self::generate().map_err(io::Error::other)?;
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(&tmp)?.write_all(keypair.secret())?;
        let linked = std::fs::hard_link(&tmp, path);
        std::fs::remove_file(&tmp)?;
        linked.map(|()| keypair)
    }

    pub fn secret(&self) -> &[u8; KEY_LEN] {
        &self.secret
    }

    pub fn public(&self) -> PublicKey {
        self.public
    }
}

impl fmt::Debug for StaticKeypair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StaticKeypair").field("public", &self.public).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_key_is_derived() {
        let a = StaticKeypair::from_secret(&[7; KEY_LEN]);
        let b = StaticKeypair::from_secret(&[7; KEY_LEN]);
        let c = StaticKeypair::from_secret(&[8; KEY_LEN]);
        assert_eq!(a.public(), b.public());
        assert_ne!(a.public(), c.public());
        assert_ne!(StaticKeypair::generate().unwrap().public(), StaticKeypair::generate().unwrap().public());
    }

    #[test]
    fn key_file() {
        let path = std::env::temp_dir().join(format!("weft-key-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let created = StaticKeypair::load_or_create(&path).unwrap();
        let loaded = StaticKeypair::load_or_create(&path).unwrap();
        assert_eq!(created.public(), loaded.public());
        std::fs::write(&path, b"short").unwrap();
        assert!(StaticKeypair::load_or_create(&path).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
