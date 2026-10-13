use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::net::IpAddr;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(60);
const PER_IP: usize = 5;
const PER_NETWORK: usize = 20;
/// New devices one address may register per minute; mobile carriers put many people behind one.
const REGISTRATIONS_PER_IP: usize = 30;

#[derive(Default)]
pub struct Limiter {
    ips: Failures<IpAddr>,
    networks: Failures<String>,
    registrations: Failures<IpAddr>,
}

struct Failures<K> {
    entries: HashMap<K, VecDeque<Instant>>,
}

impl<K> Default for Failures<K> {
    fn default() -> Self {
        Self { entries: HashMap::new() }
    }
}

impl<K: Hash + Eq + Clone> Failures<K> {
    fn count(&mut self, key: &K, now: Instant) -> usize {
        let Some(times) = self.entries.get_mut(key) else {
            return 0;
        };
        while times.front().is_some_and(|&time| now.duration_since(time) >= WINDOW) {
            times.pop_front();
        }
        if times.is_empty() {
            self.entries.remove(key);
            return 0;
        }
        times.len()
    }

    fn record(&mut self, key: &K, now: Instant) {
        self.entries.entry(key.clone()).or_default().push_back(now);
    }

    fn prune(&mut self, now: Instant) {
        self.entries.retain(|_, times| times.back().is_some_and(|&time| now.duration_since(time) < WINDOW));
    }
}

impl Limiter {
    /// Whether `ip` may try a password for `network`. While strangers guess at a network, it
    /// still takes tries from addresses without recent failures, so its members can join.
    pub fn allowed(&mut self, ip: IpAddr, network: &str, now: Instant) -> bool {
        let failures = self.ips.count(&ip, now);
        failures < PER_IP && (failures == 0 || self.networks.count(&network.to_string(), now) < PER_NETWORK)
    }

    pub fn failed(&mut self, ip: IpAddr, network: &str, now: Instant) {
        self.networks.record(&network.to_string(), now);
        if self.networks.entries.len() > 10_000 {
            self.networks.prune(now);
        }
        self.ip_failed(ip, now);
    }

    /// Counts a new device from `ip` and tells whether it may register.
    pub fn registration_allowed(&mut self, ip: IpAddr, now: Instant) -> bool {
        if self.registrations.count(&ip, now) >= REGISTRATIONS_PER_IP {
            return false;
        }
        self.registrations.record(&ip, now);
        if self.registrations.entries.len() > 10_000 {
            self.registrations.prune(now);
        }
        true
    }

    pub fn ip_allowed(&mut self, ip: IpAddr, now: Instant) -> bool {
        self.ips.count(&ip, now) < PER_IP
    }

    pub fn ip_failed(&mut self, ip: IpAddr, now: Instant) {
        self.ips.record(&ip, now);
        if self.ips.entries.len() > 10_000 {
            self.ips.prune(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_per_ip_and_network() {
        let mut limiter = Limiter::default();
        let now = Instant::now();
        let ip: IpAddr = "198.51.100.1".parse().unwrap();
        for _ in 0..PER_IP {
            assert!(limiter.allowed(ip, "lan", now));
            limiter.failed(ip, "lan", now);
        }
        assert!(!limiter.allowed(ip, "lan", now));
        assert!(!limiter.allowed(ip, "other", now));
        assert!(limiter.allowed("198.51.100.2".parse().unwrap(), "lan", now));
        assert!(limiter.allowed(ip, "lan", now + WINDOW));
        for _ in 0..REGISTRATIONS_PER_IP {
            assert!(limiter.registration_allowed(ip, now));
        }
        assert!(!limiter.registration_allowed(ip, now));
        assert!(limiter.registration_allowed(ip, now + WINDOW));

        for i in 0..PER_NETWORK {
            limiter.failed(IpAddr::from([10, 0, 0, i as u8]), "target", now);
        }
        assert!(!limiter.allowed(IpAddr::from([10, 0, 0, 1]), "target", now));
        assert!(limiter.allowed(IpAddr::from([10, 0, 0, 1]), "other", now));
        assert!(limiter.allowed("198.51.100.3".parse().unwrap(), "target", now));
    }
}
