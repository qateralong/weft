use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};

use weft_proto::control::DISCOVERY_TOKEN_LEN;
use weft_proto::loom::{Token, observed_packet, parse_relay, relayed_packet};
use weft_proto::{HEADER_LEN, Header, ObfsKey, PublicKey};
use weft_session::StaticKeypair;

#[derive(Debug, PartialEq, Eq)]
pub enum Delivery {
    Udp { to: SocketAddr, datagram: Vec<u8> },
    Tcp { to: Ipv4Addr, source: Ipv4Addr, packet: Vec<u8> },
}

pub struct FakeLoom {
    pub addr: SocketAddr,
    public: PublicKey,
    obfs: ObfsKey,
    devices: HashMap<Token, (PublicKey, Ipv4Addr)>,
    by_address: HashMap<Ipv4Addr, Token>,
    endpoints: HashMap<Token, SocketAddr>,
    pub relayed: u64,
}

impl FakeLoom {
    pub fn new(addr: SocketAddr, keypair: &StaticKeypair) -> Self {
        Self {
            addr,
            public: keypair.public(),
            obfs: ObfsKey::for_receiver(&keypair.public()),
            devices: HashMap::new(),
            by_address: HashMap::new(),
            endpoints: HashMap::new(),
            relayed: 0,
        }
    }

    pub fn public_key(&self) -> PublicKey {
        self.public
    }

    pub fn register(&mut self, key: PublicKey, address: Ipv4Addr) -> Token {
        let mut token = [0; DISCOVERY_TOKEN_LEN];
        token[..4].copy_from_slice(&address.octets());
        token[4..].copy_from_slice(&key.as_bytes()[..DISCOVERY_TOKEN_LEN - 4]);
        self.devices.insert(token, (key, address));
        self.by_address.insert(address, token);
        token
    }

    pub fn endpoint(&self, address: Ipv4Addr) -> Option<SocketAddr> {
        self.by_address.get(&address).and_then(|token| self.endpoints.get(token)).copied()
    }

    pub fn relay_tcp(&mut self, source: Ipv4Addr, destination: Ipv4Addr, inner: &[u8]) -> Option<Delivery> {
        let target = self.by_address.get(&destination)?;
        let (key, _) = *self.devices.get(target)?;
        self.relayed += 1;
        Some(match self.endpoints.get(target) {
            Some(&endpoint) => {
                let mut forwarded = relayed_packet(source, inner);
                ObfsKey::for_receiver(&key).seal(&mut forwarded).expect("valid packet");
                Delivery::Udp { to: endpoint, datagram: forwarded }
            }
            None => Delivery::Tcp { to: destination, source, packet: inner.to_vec() },
        })
    }

    pub fn handle(&mut self, from: SocketAddr, datagram: &[u8]) -> Vec<Delivery> {
        let mut packet = datagram.to_vec();
        match self.obfs.open(&mut packet) {
            Ok(Header::Discover) => {
                let Some(token) = packet.get(HEADER_LEN..HEADER_LEN + DISCOVERY_TOKEN_LEN) else { return vec![] };
                let token: Token = token.try_into().expect("slice length");
                let Some(&(key, _)) = self.devices.get(&token) else { return vec![] };
                self.endpoints.insert(token, from);
                let mut reply = observed_packet(&token, from, 0, [0x5a; 16]);
                ObfsKey::for_receiver(&key).seal(&mut reply).expect("valid packet");
                vec![Delivery::Udp { to: from, datagram: reply }]
            }
            Ok(Header::Relay) => {
                let Some((token, destination, inner)) = parse_relay(&packet) else { return vec![] };
                let Some(&(_, source)) = self.devices.get(&token) else { return vec![] };
                self.relay_tcp(source, destination, inner).into_iter().collect()
            }
            _ => vec![],
        }
    }
}
