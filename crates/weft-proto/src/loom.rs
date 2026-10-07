use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::control::DISCOVERY_TOKEN_LEN;
use crate::packet::{HEADER_LEN, Header, MIN_PACKET_LEN, TAG_LEN};

pub type Token = [u8; DISCOVERY_TOKEN_LEN];

const ENDPOINT_LEN: usize = 18;

pub fn relay_packet(token: &Token, destination: Ipv4Addr, inner: &[u8]) -> Vec<u8> {
    [&Header::Relay.encode()[..], token, &destination.octets(), inner].concat()
}

pub fn parse_relay(packet: &[u8]) -> Option<(Token, Ipv4Addr, &[u8])> {
    let body = packet.get(HEADER_LEN..)?;
    let token = body.get(..DISCOVERY_TOKEN_LEN)?.try_into().ok()?;
    let destination = ipv4(body.get(DISCOVERY_TOKEN_LEN..DISCOVERY_TOKEN_LEN + 4)?)?;
    let inner = body.get(DISCOVERY_TOKEN_LEN + 4..)?;
    (inner.len() >= MIN_PACKET_LEN).then_some((token, destination, inner))
}

pub fn relayed_packet(source: Ipv4Addr, inner: &[u8]) -> Vec<u8> {
    [&Header::Relayed.encode()[..], &source.octets(), inner].concat()
}

pub fn parse_relayed(packet: &[u8]) -> Option<(Ipv4Addr, &[u8])> {
    let body = packet.get(HEADER_LEN..)?;
    let source = ipv4(body.get(..4)?)?;
    let inner = body.get(4..)?;
    (inner.len() >= MIN_PACKET_LEN).then_some((source, inner))
}

pub fn observed_packet(token: &Token, observed: SocketAddr, padding: usize, trailer: [u8; TAG_LEN]) -> Vec<u8> {
    let mut packet = [&Header::Observed.encode()[..], token, &encode_endpoint(observed)].concat();
    packet.resize(packet.len() + padding, 0);
    packet.extend_from_slice(&trailer);
    packet
}

pub fn parse_observed(packet: &[u8]) -> Option<(Token, SocketAddr)> {
    let body = packet.get(HEADER_LEN..packet.len().checked_sub(TAG_LEN)?)?;
    let token = body.get(..DISCOVERY_TOKEN_LEN)?.try_into().ok()?;
    let endpoint = decode_endpoint(body.get(DISCOVERY_TOKEN_LEN..DISCOVERY_TOKEN_LEN + ENDPOINT_LEN)?)?;
    Some((token, endpoint))
}

pub fn encode_endpoint(addr: SocketAddr) -> [u8; ENDPOINT_LEN] {
    let ip = match addr.ip() {
        IpAddr::V4(ip) => ip.to_ipv6_mapped(),
        IpAddr::V6(ip) => ip,
    };
    let mut out = [0; ENDPOINT_LEN];
    out[..16].copy_from_slice(&ip.octets());
    out[16..].copy_from_slice(&addr.port().to_be_bytes());
    out
}

pub fn decode_endpoint(bytes: &[u8]) -> Option<SocketAddr> {
    let ip = Ipv6Addr::from(<[u8; 16]>::try_from(bytes.get(..16)?).ok()?);
    let port = u16::from_be_bytes(bytes.get(16..18)?.try_into().ok()?);
    Some(SocketAddr::new(ip.to_canonical(), port))
}

fn ipv4(bytes: &[u8]) -> Option<Ipv4Addr> {
    Some(Ipv4Addr::from(<[u8; 4]>::try_from(bytes).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ObfsKey, PublicKey};

    fn inner() -> Vec<u8> {
        (0..48).map(|i| i as u8).collect()
    }

    #[test]
    fn relay_roundtrip_through_masking() {
        let key = ObfsKey::for_receiver(&PublicKey::from_bytes([4; 32]));
        let mut packet = relay_packet(&[1; 16], Ipv4Addr::new(100, 64, 0, 9), &inner());
        key.seal(&mut packet).unwrap();
        assert_eq!(key.open(&mut packet), Ok(Header::Relay));
        let (token, destination, payload) = parse_relay(&packet).unwrap();
        assert_eq!((token, destination, payload), ([1; 16], Ipv4Addr::new(100, 64, 0, 9), inner().as_slice()));

        let mut packet = relayed_packet(Ipv4Addr::new(100, 64, 0, 1), &inner());
        key.seal(&mut packet).unwrap();
        assert_eq!(key.open(&mut packet), Ok(Header::Relayed));
        assert_eq!(parse_relayed(&packet), Some((Ipv4Addr::new(100, 64, 0, 1), inner().as_slice())));
    }

    #[test]
    fn short_relays_are_rejected() {
        assert!(parse_relay(&relay_packet(&[1; 16], Ipv4Addr::LOCALHOST, &[0; 31])).is_none());
        assert!(parse_relayed(&relayed_packet(Ipv4Addr::LOCALHOST, &[0; 10])).is_none());
    }

    #[test]
    fn observed_roundtrip() {
        for addr in ["198.51.100.7:4000", "[2001:db8::1]:9"] {
            let addr: SocketAddr = addr.parse().unwrap();
            let packet = observed_packet(&[3; 16], addr, 7, [9; 16]);
            assert_eq!(parse_observed(&packet), Some(([3; 16], addr)));
        }
    }
}
