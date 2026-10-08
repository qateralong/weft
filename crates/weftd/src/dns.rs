use std::collections::HashMap;
use std::net::Ipv4Addr;

use weft_proto::DNS_ADDRESS;

pub const ZONE: &str = "weft";
const TTL: u32 = 30;
const TYPE_A: u16 = 1;
const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;
const NO_ERROR: u16 = 0;
const NX_DOMAIN: u16 = 3;
const REFUSED: u16 = 5;
const MAX_LABEL: usize = 63;

/// The DNS label for a nickname: lowercase, IDNA-encoded, with other characters turned into dashes.
pub fn label(nickname: &str) -> Option<String> {
    let mut text = String::new();
    for char in nickname.trim().to_lowercase().chars() {
        let char = if char.is_alphanumeric() { char } else { '-' };
        if !(char == '-' && (text.is_empty() || text.ends_with('-'))) {
            text.push(char);
        }
    }
    let text = text.trim_end_matches('-');
    let ascii = idna::domain_to_ascii(text).ok()?;
    (!ascii.is_empty() && ascii.len() <= MAX_LABEL && !ascii.contains('.')).then_some(ascii)
}

/// Builds the name table; on clashes the first device keeps the name.
pub fn names<'a>(devices: impl Iterator<Item = (&'a str, Ipv4Addr)>) -> HashMap<String, Ipv4Addr> {
    let mut names = HashMap::new();
    for (nickname, address) in devices {
        if let Some(label) = label(nickname) {
            names.entry(label).or_insert(address);
        }
    }
    names
}

pub fn is_query(packet: &[u8]) -> bool {
    packet.len() >= 20 && packet[0] >> 4 == 4 && packet[9] == 17 && packet[16..20] == DNS_ADDRESS.octets()
}

/// Answers a DNS query sent through the TUN interface to the resolver address.
pub fn respond(packet: &[u8], names: &HashMap<String, Ipv4Addr>) -> Option<Vec<u8>> {
    if !is_query(packet) {
        return None;
    }
    let header_len = usize::from(packet[0] & 0x0f) * 4;
    let fragmented = u16::from_be_bytes([packet[6], packet[7]]) & 0x3fff != 0;
    if header_len < 20 || fragmented || packet.len() < header_len + 8 {
        return None;
    }
    let source = Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]);
    let udp = &packet[header_len..];
    let (source_port, destination_port) = (u16::from_be_bytes([udp[0], udp[1]]), u16::from_be_bytes([udp[2], udp[3]]));
    let udp_len = usize::from(u16::from_be_bytes([udp[4], udp[5]]));
    if destination_port != 53 || udp_len < 8 || udp_len > udp.len() {
        return None;
    }
    let message = answer(&udp[8..udp_len], names)?;
    Some(udp_packet(DNS_ADDRESS, source, 53, source_port, &message))
}

fn answer(query: &[u8], names: &HashMap<String, Ipv4Addr>) -> Option<Vec<u8>> {
    if query.len() < 12 {
        return None;
    }
    let flags = u16::from_be_bytes([query[2], query[3]]);
    let questions = u16::from_be_bytes([query[4], query[5]]);
    if flags & 0x8000 != 0 || questions != 1 {
        return None;
    }
    let mut labels = Vec::new();
    let mut at = 12;
    loop {
        let len = usize::from(*query.get(at)?);
        at += 1;
        if len == 0 {
            break;
        }
        if len > MAX_LABEL || labels.len() > 127 {
            return None;
        }
        labels.push(String::from_utf8_lossy(query.get(at..at + len)?).to_ascii_lowercase());
        at += len;
    }
    let question = query.get(12..at + 4)?;
    let kind = u16::from_be_bytes([query[at], query[at + 1]]);
    let class = u16::from_be_bytes([query[at + 2], query[at + 3]]);

    let (rcode, address) = match labels.as_slice() {
        [.., zone] if zone != ZONE || class != CLASS_IN => (REFUSED, None),
        [_] => (NO_ERROR, None),
        [name, _] => match names.get(name) {
            Some(&address) => (NO_ERROR, (kind == TYPE_A || kind == TYPE_ANY).then_some(address)),
            None => (NX_DOMAIN, None),
        },
        _ => (NX_DOMAIN, None),
    };

    let mut reply = Vec::with_capacity(question.len() + 28);
    reply.extend_from_slice(&query[0..2]);
    let flags = 0x8000 | (flags & 0x7900) | 0x0400 | rcode;
    reply.extend_from_slice(&flags.to_be_bytes());
    reply.extend_from_slice(&[0, 1, 0, u8::from(address.is_some()), 0, 0, 0, 0]);
    reply.extend_from_slice(question);
    if let Some(address) = address {
        reply.extend_from_slice(&[0xc0, 12]);
        reply.extend_from_slice(&TYPE_A.to_be_bytes());
        reply.extend_from_slice(&CLASS_IN.to_be_bytes());
        reply.extend_from_slice(&TTL.to_be_bytes());
        reply.extend_from_slice(&4u16.to_be_bytes());
        reply.extend_from_slice(&address.octets());
    }
    Some(reply)
}

fn udp_packet(
    source: Ipv4Addr,
    destination: Ipv4Addr,
    source_port: u16,
    destination_port: u16,
    data: &[u8],
) -> Vec<u8> {
    let udp_len = 8 + data.len();
    let total = 20 + udp_len;
    let mut packet = Vec::with_capacity(total);
    packet.extend_from_slice(&[0x45, 0]);
    packet.extend_from_slice(&(total as u16).to_be_bytes());
    packet.extend_from_slice(&[0, 0, 0x40, 0, 64, 17, 0, 0]);
    packet.extend_from_slice(&source.octets());
    packet.extend_from_slice(&destination.octets());
    let checksum = internet_checksum(&[&packet[..20]]);
    packet[10..12].copy_from_slice(&checksum.to_be_bytes());

    let mut udp = Vec::with_capacity(udp_len);
    udp.extend_from_slice(&source_port.to_be_bytes());
    udp.extend_from_slice(&destination_port.to_be_bytes());
    udp.extend_from_slice(&(udp_len as u16).to_be_bytes());
    udp.extend_from_slice(&[0, 0]);
    udp.extend_from_slice(data);
    let mut pseudo = Vec::with_capacity(12);
    pseudo.extend_from_slice(&source.octets());
    pseudo.extend_from_slice(&destination.octets());
    pseudo.extend_from_slice(&[0, 17]);
    pseudo.extend_from_slice(&(udp_len as u16).to_be_bytes());
    let checksum = match internet_checksum(&[&pseudo, &udp]) {
        0 => 0xffff,
        sum => sum,
    };
    udp[6..8].copy_from_slice(&checksum.to_be_bytes());
    packet.extend_from_slice(&udp);
    packet
}

fn internet_checksum(parts: &[&[u8]]) -> u16 {
    let mut sum = 0u32;
    for part in parts {
        for chunk in part.chunks(2) {
            let word =
                if chunk.len() == 2 { u16::from_be_bytes([chunk[0], chunk[1]]) } else { u16::from(chunk[0]) << 8 };
            sum += u32::from(word);
        }
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 1);

    fn query(name: &str, kind: u16) -> Vec<u8> {
        let mut dns = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            dns.push(label.len() as u8);
            dns.extend_from_slice(label.as_bytes());
        }
        dns.push(0);
        dns.extend_from_slice(&kind.to_be_bytes());
        dns.extend_from_slice(&CLASS_IN.to_be_bytes());
        udp_packet(CLIENT, DNS_ADDRESS, 40000, 53, &dns)
    }

    fn parse(packet: &[u8]) -> (u16, Option<Ipv4Addr>) {
        assert_eq!(internet_checksum(&[&packet[..20]]), 0);
        assert_eq!(&packet[16..20], &CLIENT.octets());
        assert_eq!(u16::from_be_bytes([packet[22], packet[23]]), 40000);
        let dns = &packet[28..];
        assert_eq!(&dns[..2], &[0x12, 0x34]);
        let flags = u16::from_be_bytes([dns[2], dns[3]]);
        assert_eq!(flags & 0x8500, 0x8500);
        let answers = u16::from_be_bytes([dns[6], dns[7]]);
        let address = (answers == 1).then(|| {
            let end = dns.len();
            Ipv4Addr::new(dns[end - 4], dns[end - 3], dns[end - 2], dns[end - 1])
        });
        (flags & 0x0f, address)
    }

    #[test]
    fn labels() {
        assert_eq!(label("Bob").as_deref(), Some("bob"));
        assert_eq!(label("  Big  Bob_2! ").as_deref(), Some("big-bob-2"));
        assert_eq!(label("\u{0432}\u{0430}\u{0441}\u{044f}").as_deref(), Some("xn--80ad0c0c"));
        assert_eq!(label("!!!"), None);
    }

    #[test]
    fn answers_queries() {
        let names = names([("Bob", Ipv4Addr::new(100, 64, 0, 2)), ("bob", Ipv4Addr::new(100, 64, 0, 9))].into_iter());
        assert_eq!(names.len(), 1);
        let respond = |name, kind| parse(&respond(&query(name, kind), &names).unwrap());
        assert_eq!(respond("BOB.weft", TYPE_A), (NO_ERROR, Some(Ipv4Addr::new(100, 64, 0, 2))));
        assert_eq!(respond("bob.weft", 28), (NO_ERROR, None));
        assert_eq!(respond("eve.weft", TYPE_A), (NX_DOMAIN, None));
        assert_eq!(respond("a.bob.weft", TYPE_A), (NX_DOMAIN, None));
        assert_eq!(respond("weft", TYPE_A), (NO_ERROR, None));
        assert_eq!(respond("example.com", TYPE_A), (REFUSED, None));
    }

    #[test]
    fn ignores_other_packets() {
        let names = HashMap::new();
        let mut packet = query("bob.weft", TYPE_A);
        packet[19] = 99;
        assert!(respond(&packet, &names).is_none());
        assert!(respond(&[0x45; 10], &names).is_none());
        let mut truncated = query("bob.weft", TYPE_A);
        truncated.truncate(35);
        truncated[2..4].copy_from_slice(&35u16.to_be_bytes());
        truncated[24..26].copy_from_slice(&15u16.to_be_bytes());
        assert!(respond(&truncated, &names).is_none());
    }
}
