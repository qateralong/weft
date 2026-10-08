use std::time::Instant;

use serde::{Deserialize, Serialize};

const MIN_BURST: f64 = 64.0 * 1024.0;

/// Relayed traffic counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Traffic {
    pub bytes: u64,
    pub packets: u64,
    pub dropped: u64,
}

impl Traffic {
    pub fn add(&mut self, len: usize) {
        self.bytes += len as u64;
        self.packets += 1;
    }
}

/// Token bucket in bytes with half a second of burst.
#[derive(Clone, Debug)]
pub struct Bucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    at: Instant,
}

impl Bucket {
    pub fn mbit(mbit: u32, now: Instant) -> Option<Self> {
        let rate = f64::from(mbit) * 125_000.0;
        let burst = (rate / 2.0).max(MIN_BURST);
        (mbit > 0).then_some(Self { rate, burst, tokens: burst, at: now })
    }

    pub fn take(&mut self, len: usize, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.burst);
        self.at = now;
        if self.tokens < len as f64 {
            return false;
        }
        self.tokens -= len as f64;
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn limits_the_rate() {
        let now = Instant::now();
        assert!(Bucket::mbit(0, now).is_none());
        let mut bucket = Bucket::mbit(8, now).unwrap();
        let sent = (0..1000).take_while(|_| bucket.take(1000, now)).count();
        assert_eq!(sent, 500);
        assert!(!bucket.take(1000, now + Duration::from_micros(500)));
        assert!(bucket.take(1000, now + Duration::from_millis(1)));
        let later = now + Duration::from_secs(10);
        assert_eq!((0..1000).take_while(|_| bucket.take(1000, later)).count(), 500);
    }
}
