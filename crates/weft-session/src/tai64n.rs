use std::time::{SystemTime, UNIX_EPOCH};

pub const TAI64N_LEN: usize = 12;
const TAI64_BASE: u64 = 0x4000_0000_0000_000a;
const GRANULARITY_NANOS: u32 = 20_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tai64N([u8; TAI64N_LEN]);

impl Tai64N {
    pub fn from_system_time(time: SystemTime) -> Self {
        let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
        let nanos = since_epoch.subsec_nanos();
        let mut bytes = [0; TAI64N_LEN];
        bytes[..8].copy_from_slice(&(TAI64_BASE + since_epoch.as_secs()).to_be_bytes());
        bytes[8..].copy_from_slice(&(nanos - nanos % GRANULARITY_NANOS).to_be_bytes());
        Self(bytes)
    }

    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }

    pub fn as_bytes(&self) -> &[u8; TAI64N_LEN] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn ordering_follows_time() {
        let base = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let a = Tai64N::from_system_time(base);
        let b = Tai64N::from_system_time(base + Duration::from_millis(25));
        let c = Tai64N::from_system_time(base + Duration::from_secs(1));
        assert!(a < b && b < c);
        assert_eq!(Tai64N::from_slice(c.as_bytes()), Some(c));
    }

    #[test]
    fn precision_is_reduced() {
        let base = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert_eq!(Tai64N::from_system_time(base), Tai64N::from_system_time(base + Duration::from_millis(19)));
    }
}
