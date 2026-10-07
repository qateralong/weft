use std::fmt;

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
}
