use std::net::Ipv4Addr;

/// Builds an ICMP echo reply for an echo request addressed to `own`.
pub fn reply(packet: &[u8], own: Ipv4Addr) -> Option<Vec<u8>> {
    let header_len = usize::from(*packet.first()? & 0x0f) * 4;
    let total = usize::from(u16::from_be_bytes([*packet.get(2)?, *packet.get(3)?]));
    if packet[0] >> 4 != 4 || header_len < 20 || total > packet.len() || total < header_len + 8 {
        return None;
    }
    if packet[9] != 1 || packet[16..20] != own.octets() || packet[header_len] != 8 {
        return None;
    }
    let mut out = packet[..total].to_vec();
    out.copy_within(12..16, 16);
    out[12..16].copy_from_slice(&packet[16..20]);
    out[8] = 64;
    out[10..12].fill(0);
    let sum = checksum(&out[..header_len]);
    out[10..12].copy_from_slice(&sum.to_be_bytes());
    out[header_len] = 0;
    out[header_len + 2..header_len + 4].fill(0);
    let sum = checksum(&out[header_len..]);
    out[header_len + 2..header_len + 4].copy_from_slice(&sum.to_be_bytes());
    Some(out)
}

fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = data.chunks(2).map(|c| u32::from(u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))).sum();
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(source: Ipv4Addr, destination: Ipv4Addr) -> Vec<u8> {
        let mut packet = vec![0x45, 0, 0, 36, 0, 1, 0, 0, 63, 1, 0, 0];
        packet.extend_from_slice(&source.octets());
        packet.extend_from_slice(&destination.octets());
        let sum = checksum(&packet);
        packet[10..12].copy_from_slice(&sum.to_be_bytes());
        let mut icmp = vec![8, 0, 0, 0, 0x12, 0x34, 0, 1, b'w', b'e', b'f', b't', 1, 2, 3, 4];
        let sum = checksum(&icmp);
        icmp[2..4].copy_from_slice(&sum.to_be_bytes());
        packet.extend_from_slice(&icmp);
        packet
    }

    #[test]
    fn replies_to_echo_requests() {
        let (a, b) = (Ipv4Addr::new(100, 64, 0, 1), Ipv4Addr::new(100, 64, 0, 2));
        let reply = reply(&request(a, b), b).unwrap();
        assert_eq!(reply[12..16], b.octets());
        assert_eq!(reply[16..20], a.octets());
        assert_eq!(reply[20], 0);
        assert_eq!(checksum(&reply[..20]), 0);
        assert_eq!(checksum(&reply[20..]), 0);
        assert_eq!(reply[24..], request(a, b)[24..]);
    }

    #[test]
    fn ignores_other_packets() {
        let (a, b) = (Ipv4Addr::new(100, 64, 0, 1), Ipv4Addr::new(100, 64, 0, 2));
        assert!(reply(&request(a, b), a).is_none());
        let mut tcp = request(a, b);
        tcp[9] = 6;
        assert!(reply(&tcp, b).is_none());
        assert!(reply(&request(a, b)[..30], b).is_none());
    }
}
