use std::net::Ipv4Addr;
use std::time::Instant;

const UDP: u8 = 17;
const BLOCKED_PORTS: [u16; 8] = [67, 68, 137, 138, 1900, 3702, 5353, 5355];
const BLOCKED_GROUPS: [Ipv4Addr; 4] = [
    Ipv4Addr::new(224, 0, 0, 251),
    Ipv4Addr::new(224, 0, 0, 252),
    Ipv4Addr::new(239, 255, 255, 250),
    Ipv4Addr::new(239, 255, 255, 253),
];
const RATE_PER_SECOND: f64 = 200.0;
const BURST: f64 = 400.0;

pub fn is_flood(destination: Ipv4Addr, broadcast: Option<Ipv4Addr>) -> bool {
    destination == Ipv4Addr::BROADCAST || Some(destination) == broadcast || destination.is_multicast()
}

/// Only the first fragment of UDP to a non-service port and group is flooded.
pub fn floodable(packet: &[u8]) -> bool {
    let Some(header_len) = packet.first().map(|b| usize::from(b & 0x0f) * 4) else { return false };
    if packet.len() < header_len + 4 || packet[9] != UDP {
        return false;
    }
    let fragment_offset = u16::from_be_bytes([packet[6], packet[7]]) & 0x1fff;
    let destination = Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]);
    let port = u16::from_be_bytes([packet[header_len + 2], packet[header_len + 3]]);
    fragment_offset == 0 && !BLOCKED_PORTS.contains(&port) && !BLOCKED_GROUPS.contains(&destination)
}

pub struct Bucket {
    tokens: f64,
    last: Option<Instant>,
}

impl Default for Bucket {
    fn default() -> Self {
        Self { tokens: BURST, last: None }
    }
}

impl Bucket {
    pub fn take(&mut self, now: Instant) -> bool {
        if let Some(last) = self.last {
            self.tokens =
                (self.tokens + now.saturating_duration_since(last).as_secs_f64() * RATE_PER_SECOND).min(BURST);
        }
        self.last = Some(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
pub(crate) fn udp(source: Ipv4Addr, destination: Ipv4Addr, port: u16, payload: &[u8]) -> Vec<u8> {
    let total = 28 + payload.len();
    let mut packet = vec![0; total];
    packet[0] = 0x45;
    packet[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    packet[8] = 64;
    packet[9] = UDP;
    packet[12..16].copy_from_slice(&source.octets());
    packet[16..20].copy_from_slice(&destination.octets());
    packet[20..22].copy_from_slice(&port.to_be_bytes());
    packet[22..24].copy_from_slice(&port.to_be_bytes());
    packet[24..26].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    packet[28..].copy_from_slice(payload);
    packet
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const ME: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 1);

    #[test]
    fn flood_destinations() {
        let broadcast = Some(Ipv4Addr::new(100, 127, 255, 255));
        assert!(is_flood(Ipv4Addr::BROADCAST, broadcast));
        assert!(is_flood(Ipv4Addr::new(100, 127, 255, 255), broadcast));
        assert!(is_flood(Ipv4Addr::new(224, 0, 2, 60), broadcast));
        assert!(!is_flood(Ipv4Addr::new(100, 64, 0, 2), broadcast));
        assert!(!is_flood(Ipv4Addr::new(192, 168, 1, 255), broadcast));
    }

    #[test]
    fn service_noise_is_not_flooded() {
        assert!(floodable(&udp(ME, Ipv4Addr::BROADCAST, 4445, b"game")));
        assert!(floodable(&udp(ME, Ipv4Addr::new(224, 0, 2, 60), 4445, b"minecraft")));
        assert!(!floodable(&udp(ME, Ipv4Addr::BROADCAST, 137, b"netbios")));
        assert!(!floodable(&udp(ME, Ipv4Addr::new(224, 0, 0, 251), 5353, b"mdns")));
        assert!(!floodable(&udp(ME, Ipv4Addr::new(239, 255, 255, 250), 4445, b"ssdp group")));
        let mut tcp = udp(ME, Ipv4Addr::BROADCAST, 4445, b"x");
        tcp[9] = 6;
        assert!(!floodable(&tcp));
        let mut fragment = udp(ME, Ipv4Addr::BROADCAST, 4445, b"x");
        fragment[7] = 10;
        assert!(!floodable(&fragment));
    }

    #[test]
    fn bucket_limits_rate() {
        let now = Instant::now();
        let mut bucket = Bucket::default();
        let passed = (0..1000).filter(|_| bucket.take(now)).count();
        assert_eq!(passed, BURST as usize);
        assert!(!bucket.take(now));
        assert!(bucket.take(now + Duration::from_millis(10)));
    }
}
