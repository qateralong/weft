use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::net::IpAddr;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(60);
const PER_IP: usize = 5;
const PER_NETWORK: usize = 20;

#[derive(Default)]
pub struct Limiter {
    ips: Failures<IpAddr>,
    networks: Failures<String>,
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
    pub fn allowed(&mut self, ip: IpAddr, network: &str, now: Instant) -> bool {
        self.ips.count(&ip, now) < PER_IP && self.networks.count(&network.to_string(), now) < PER_NETWORK
    }

    pub fn failed(&mut self, ip: IpAddr, network: &str, now: Instant) {
        self.networks.record(&network.to_string(), now);
        self.ip_failed(ip, now);
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

        for i in 0..PER_NETWORK {
            limiter.failed(IpAddr::from([10, 0, 0, i as u8]), "target", now);
        }
        assert!(!limiter.allowed("198.51.100.3".parse().unwrap(), "target", now));
    }
}
