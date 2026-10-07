use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant, SystemTime};

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use weft_proto::loom::{Token, parse_observed, parse_relayed, relay_packet};
use weft_proto::obfs::discover_packet;
use weft_proto::{Header, ObfsKey, PublicKey};
use weft_session::{Event, Node, PeerId, StaticKeypair};

use crate::disco::{self, DiscoKey, Message, TxId};
use crate::flood::{self, Bucket};

pub const PATH_PING_AFTER: Duration = Duration::from_secs(8);
pub const PATH_STALE_AFTER: Duration = Duration::from_secs(20);
pub const PING_TIMEOUT: Duration = Duration::from_secs(5);
pub const PROBE_RETRY: Duration = Duration::from_secs(3);
pub const PROBE_BACKOFF: Duration = Duration::from_secs(40);
pub const DISCOVER_INTERVAL: Duration = Duration::from_secs(20);
pub const DISCOVER_JITTER: Duration = Duration::from_secs(5);
pub const SERVER_UDP_FRESH: Duration = Duration::from_secs(60);
const MAX_PATHS: usize = 32;
const MAX_DISCO_PADDING: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerConfig {
    pub key: PublicKey,
    pub address: Ipv4Addr,
    pub online: bool,
    pub candidates: Vec<SocketAddr>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    Udp { to: SocketAddr, datagram: Vec<u8> },
    TcpRelay { to: Ipv4Addr, packet: Vec<u8> },
    CallMeMaybe { peer: PublicKey },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivered {
    pub source: Ipv4Addr,
    pub packet: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerLink {
    Connecting,
    Relay,
    Direct { addr: SocketAddr, latency: Option<Duration> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    #[error("no peer has this address")]
    NoRoute,
    #[error("invalid packet")]
    Invalid,
    #[error("broadcast is filtered")]
    Filtered,
    #[error("broadcast rate limit")]
    RateLimited,
}

pub struct Mesh {
    node: Node,
    secret: [u8; 32],
    public: PublicKey,
    obfs: ObfsKey,
    peers: HashMap<PeerId, Peer>,
    by_key: HashMap<PublicKey, PeerId>,
    by_address: HashMap<Ipv4Addr, PeerId>,
    server: Option<Server>,
    local: Option<Local>,
    flood: Bucket,
    outputs: VecDeque<Output>,
    rng: StdRng,
}

#[derive(Clone, Copy)]
struct Local {
    address: Ipv4Addr,
    broadcast: Ipv4Addr,
}

struct Server {
    udp: SocketAddr,
    obfs: ObfsKey,
    token: Token,
    observed: Option<SocketAddr>,
    udp_fresh_until: Option<Instant>,
    discover_at: Instant,
}

struct Peer {
    key: PublicKey,
    address: Ipv4Addr,
    online: bool,
    obfs: ObfsKey,
    disco: DiscoKey,
    candidates: Vec<SocketAddr>,
    paths: HashMap<SocketAddr, Path>,
    best: Option<SocketAddr>,
    probe_at: Option<Instant>,
    attempts: u32,
    pings: HashMap<TxId, (SocketAddr, Instant)>,
}

#[derive(Default)]
struct Path {
    heard: Option<Instant>,
    pinged: Option<Instant>,
    latency: Option<Duration>,
}

impl Mesh {
    pub fn new(keypair: StaticKeypair, now: Instant, wall: SystemTime) -> Result<Self, getrandom::Error> {
        let mut seed = [0; 32];
        getrandom::fill(&mut seed)?;
        let node = Node::new(StaticKeypair::from_secret(keypair.secret()), now, wall)?;
        Ok(Self::with_parts(keypair, node, StdRng::from_seed(seed)))
    }

    pub fn with_seed(keypair: StaticKeypair, now: Instant, wall: SystemTime, seed: u64) -> Self {
        let node = Node::with_seed(StaticKeypair::from_secret(keypair.secret()), now, wall, seed);
        Self::with_parts(keypair, node, StdRng::seed_from_u64(seed ^ 0x5eed))
    }

    fn with_parts(keypair: StaticKeypair, node: Node, rng: StdRng) -> Self {
        let public = keypair.public();
        Self {
            node,
            secret: *keypair.secret(),
            public,
            obfs: ObfsKey::for_receiver(&public),
            peers: HashMap::new(),
            by_key: HashMap::new(),
            by_address: HashMap::new(),
            server: None,
            local: None,
            flood: Bucket::default(),
            outputs: VecDeque::new(),
            rng,
        }
    }

    pub fn public_key(&self) -> PublicKey {
        self.public
    }

    pub fn observed(&self) -> Option<SocketAddr> {
        self.server.as_ref().and_then(|server| server.observed)
    }

    pub fn set_server(&mut self, now: Instant, udp: SocketAddr, key: PublicKey, token: Token) {
        self.server = Some(Server {
            udp,
            obfs: ObfsKey::for_receiver(&key),
            token,
            observed: None,
            udp_fresh_until: None,
            discover_at: now,
        });
        self.tick(now);
    }

    pub fn set_local(&mut self, address: Ipv4Addr, prefix: u8) {
        let host_bits = u32::MAX.checked_shr(u32::from(prefix)).unwrap_or(0);
        let broadcast = Ipv4Addr::from(u32::from(address) | host_bits);
        self.local = Some(Local { address, broadcast });
    }

    pub fn clear_server(&mut self) {
        self.server = None;
    }

    pub fn update_peers(&mut self, now: Instant, configs: Vec<PeerConfig>) {
        let listed: HashMap<PublicKey, PeerConfig> =
            configs.into_iter().filter(|config| config.key != self.public).map(|c| (c.key, c)).collect();
        let gone: Vec<PeerId> = self
            .peers
            .iter()
            .filter(|(id, peer)| match listed.get(&peer.key) {
                None => true,
                Some(config) => !config.online && !self.node.is_established(**id),
            })
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            self.remove(id);
        }

        for (key, config) in listed {
            let id = match self.by_key.get(&key) {
                Some(&id) => id,
                None if config.online => match self.add(now, &config) {
                    Some(id) => id,
                    None => continue,
                },
                None => continue,
            };
            let Some(peer) = self.peers.get_mut(&id) else { continue };
            peer.online = config.online;
            if peer.address != config.address {
                self.by_address.remove(&peer.address);
                peer.address = config.address;
            }
            self.by_address.insert(config.address, id);
            let mut candidates = config.candidates;
            candidates.dedup();
            if peer.candidates != candidates {
                peer.candidates = candidates;
                if peer.best.is_none() {
                    peer.probe_at = Some(now);
                    peer.attempts = 0;
                }
            }
        }
        self.tick(now);
    }

    pub fn call_me_maybe(&mut self, now: Instant, key: &PublicKey, candidates: Vec<SocketAddr>) {
        let Some(&id) = self.by_key.get(key) else { return };
        if let Some(peer) = self.peers.get_mut(&id)
            && !candidates.is_empty()
        {
            peer.candidates = candidates;
        }
        self.burst(now, id);
        self.flush(now);
    }

    pub fn send(&mut self, now: Instant, packet: &[u8]) -> Result<(), SendError> {
        let destination = ipv4_destination(packet).ok_or(SendError::Invalid)?;
        if flood::is_flood(destination, self.local.map(|local| local.broadcast)) {
            return self.flood(now, packet);
        }
        let &id = self.by_address.get(&destination).ok_or(SendError::NoRoute)?;
        let result = self.node.send(now, id, packet).map_err(|_| SendError::Invalid);
        self.flush(now);
        result
    }

    fn flood(&mut self, now: Instant, packet: &[u8]) -> Result<(), SendError> {
        if !flood::floodable(packet) {
            return Err(SendError::Filtered);
        }
        if !self.flood.take(now) {
            return Err(SendError::RateLimited);
        }
        let targets: Vec<PeerId> = self
            .peers
            .iter()
            .filter(|(id, peer)| peer.online && self.node.is_established(**id))
            .map(|(id, _)| *id)
            .collect();
        for id in targets {
            let _ = self.node.send(now, id, packet);
        }
        self.flush(now);
        Ok(())
    }

    pub fn receive_udp(&mut self, now: Instant, from: SocketAddr, datagram: &[u8]) -> Option<Delivered> {
        let mut packet = datagram.to_vec();
        let header = self.obfs.open(&mut packet).ok()?;
        let delivered = match header {
            Header::Observed => {
                self.on_observed(now, from, &packet);
                None
            }
            Header::Relayed => {
                let from_server = self.server.as_ref().is_some_and(|server| server.udp == from);
                let (source, inner) = parse_relayed(&packet).filter(|_| from_server)?;
                self.on_relayed(now, source, inner)
            }
            Header::Disco => {
                self.on_disco(now, from, &packet);
                None
            }
            header => self.on_session(now, Some(from), header, &packet),
        };
        self.flush(now);
        delivered
    }

    pub fn receive_tcp_relay(&mut self, now: Instant, source: Ipv4Addr, packet: &[u8]) -> Option<Delivered> {
        let delivered = self.on_relayed(now, source, packet);
        self.flush(now);
        delivered
    }

    pub fn tick(&mut self, now: Instant) {
        self.node.tick(now);
        let ids: Vec<PeerId> = self.peers.keys().copied().collect();
        for id in ids {
            self.tick_peer(now, id);
        }
        if let Some(server) = &mut self.server
            && now >= server.discover_at
        {
            let jitter = self.rng.random_range(0..=2 * DISCOVER_JITTER.as_millis() as u64);
            server.discover_at = now + DISCOVER_INTERVAL - DISCOVER_JITTER + Duration::from_millis(jitter);
            let mut packet = discover_packet(&server.token, self.rng.random_range(0..=64), self.rng.random());
            if server.obfs.seal(&mut packet).is_ok() {
                self.outputs.push_back(Output::Udp { to: server.udp, datagram: packet });
            }
        }
        self.flush(now);
    }

    pub fn next_timeout(&self) -> Option<Instant> {
        let peers = self.peers.values().flat_map(|peer| {
            let best = peer.best.and_then(|addr| peer.paths.get(&addr));
            let path_check = best.and_then(|path| {
                let heard = path.heard?;
                let ping = match path.pinged {
                    Some(pinged) => (heard + PATH_PING_AFTER).max(pinged + PING_TIMEOUT),
                    None => heard + PATH_PING_AFTER,
                };
                Some(ping.min(heard + PATH_STALE_AFTER))
            });
            let pings = peer.pings.values().map(|&(_, sent)| sent + PING_TIMEOUT).min();
            [peer.probe_at, path_check, pings]
        });
        let server = self.server.as_ref().map(|server| server.discover_at);
        peers.flatten().chain(server).chain(self.node.next_timeout()).min()
    }

    pub fn poll_output(&mut self) -> Option<Output> {
        self.outputs.pop_front()
    }

    pub fn link(&self, key: &PublicKey) -> Option<PeerLink> {
        let id = *self.by_key.get(key)?;
        let peer = self.peers.get(&id)?;
        if !self.node.is_established(id) {
            return Some(PeerLink::Connecting);
        }
        Some(match peer.best {
            Some(addr) => PeerLink::Direct { addr, latency: peer.paths.get(&addr).and_then(|path| path.latency) },
            None => PeerLink::Relay,
        })
    }

    fn add(&mut self, now: Instant, config: &PeerConfig) -> Option<PeerId> {
        let disco = DiscoKey::derive(&self.secret, &config.key)?;
        let id = self.node.add_peer(now, config.key);
        self.peers.insert(
            id,
            Peer {
                key: config.key,
                address: config.address,
                online: config.online,
                obfs: ObfsKey::for_receiver(&config.key),
                disco,
                candidates: Vec::new(),
                paths: HashMap::new(),
                best: None,
                probe_at: Some(now),
                attempts: 0,
                pings: HashMap::new(),
            },
        );
        self.by_key.insert(config.key, id);
        Some(id)
    }

    fn remove(&mut self, id: PeerId) {
        if let Some(peer) = self.peers.remove(&id) {
            self.node.remove_peer(id);
            self.by_key.remove(&peer.key);
            if self.by_address.get(&peer.address) == Some(&id) {
                self.by_address.remove(&peer.address);
            }
        }
    }

    fn tick_peer(&mut self, now: Instant, id: PeerId) {
        let Some(peer) = self.peers.get_mut(&id) else { return };
        peer.pings.retain(|_, &mut (_, sent)| now < sent + PING_TIMEOUT);
        if let Some(best) = peer.best {
            let heard = peer.paths.get(&best).and_then(|path| path.heard);
            match heard {
                Some(heard) if now < heard + PATH_STALE_AFTER => {
                    let pinged = peer.paths.get(&best).and_then(|path| path.pinged);
                    if now >= heard + PATH_PING_AFTER && pinged.is_none_or(|at| now >= at + PING_TIMEOUT) {
                        self.ping(now, id, best);
                    }
                }
                _ => {
                    tracing::debug!(peer = %peer.address, %best, "direct path lost");
                    peer.best = None;
                    peer.probe_at = Some(now);
                    peer.attempts = 0;
                }
            }
        }
        let due =
            self.peers.get(&id).is_some_and(|peer| peer.best.is_none() && peer.probe_at.is_some_and(|at| now >= at));
        if due {
            self.probe(now, id);
        }
    }

    fn probe(&mut self, now: Instant, id: PeerId) {
        let Some(peer) = self.peers.get_mut(&id) else { return };
        if !peer.online {
            peer.probe_at = None;
            return;
        }
        peer.attempts += 1;
        peer.probe_at = Some(now + if peer.attempts == 1 { PROBE_RETRY } else { PROBE_BACKOFF });
        let key = peer.key;
        if self.server.is_some() {
            self.outputs.push_back(Output::CallMeMaybe { peer: key });
        } else {
            self.burst(now, id);
        }
    }

    fn burst(&mut self, now: Instant, id: PeerId) {
        let Some(peer) = self.peers.get_mut(&id) else { return };
        if !peer.online {
            return;
        }
        let mut targets: Vec<SocketAddr> = peer.candidates.clone();
        targets.extend(peer.paths.keys().copied().filter(|addr| !peer.candidates.contains(addr)));
        tracing::trace!(peer = %peer.address, ?targets, "probing");
        for target in targets {
            self.ping(now, id, target);
        }
    }

    fn ping(&mut self, now: Instant, id: PeerId, to: SocketAddr) {
        let Some(peer) = self.peers.get_mut(&id) else { return };
        let tx: TxId = self.rng.random();
        let padding = self.rng.random_range(0..=MAX_DISCO_PADDING);
        let nonce = self.rng.random();
        let Ok(datagram) = disco::seal(&peer.disco, &self.public, &peer.obfs, &Message::Ping { tx }, padding, nonce)
        else {
            return;
        };
        peer.pings.insert(tx, (to, now));
        if peer.paths.len() < MAX_PATHS || peer.paths.contains_key(&to) {
            peer.paths.entry(to).or_default().pinged = Some(now);
        }
        self.outputs.push_back(Output::Udp { to, datagram });
    }

    fn on_observed(&mut self, now: Instant, from: SocketAddr, packet: &[u8]) {
        let Some(server) = self.server.as_mut().filter(|server| server.udp == from) else { return };
        let Some((_, observed)) = parse_observed(packet).filter(|(token, _)| *token == server.token) else {
            return;
        };
        tracing::trace!(%observed, "observed by the server");
        server.observed = Some(observed);
        server.udp_fresh_until = Some(now + SERVER_UDP_FRESH);
    }

    fn on_relayed(&mut self, now: Instant, source: Ipv4Addr, inner: &[u8]) -> Option<Delivered> {
        let id = *self.by_address.get(&source)?;
        let mut packet = inner.to_vec();
        let header = self.obfs.open(&mut packet).ok()?;
        if !matches!(header, Header::HandshakeInit { .. } | Header::HandshakeResp { .. } | Header::Data { .. }) {
            return None;
        }
        let delivered = self.on_session(now, None, header, &packet)?;
        (self.by_address.get(&delivered.source) == Some(&id)).then_some(delivered)
    }

    fn on_session(
        &mut self,
        now: Instant,
        from: Option<SocketAddr>,
        header: Header,
        packet: &[u8],
    ) -> Option<Delivered> {
        let received = match self.node.receive_opened(now, header, packet) {
            Ok(received) => received,
            Err(error) => {
                tracing::trace!(?from, %error, "dropped packet");
                return None;
            }
        };
        let peer = self.peers.get_mut(&received.peer)?;
        if let Some(from) = from {
            heard(peer, from, now, None);
        }
        let packet = received.packet?;
        if ipv4_source(&packet) != Some(peer.address) {
            tracing::debug!(peer = %peer.address, "dropped packet with a foreign source address");
            return None;
        }
        let destination = ipv4_destination(&packet)?;
        let accepted = self
            .local
            .is_none_or(|local| destination == local.address || flood::is_flood(destination, Some(local.broadcast)));
        if !accepted {
            tracing::debug!(peer = %peer.address, %destination, "dropped packet for a foreign destination");
            return None;
        }
        Some(Delivered { source: peer.address, packet })
    }

    fn on_disco(&mut self, now: Instant, from: SocketAddr, packet: &[u8]) {
        let Some(&id) = disco::sender(packet).and_then(|key| self.by_key.get(&key)) else { return };
        let Some(peer) = self.peers.get_mut(&id) else { return };
        let Some(message) = disco::open(&peer.disco, packet) else { return };
        tracing::trace!(peer = %peer.address, %from, ?message, "disco");
        match message {
            Message::Ping { tx } => {
                let padding = self.rng.random_range(0..=MAX_DISCO_PADDING);
                let nonce = self.rng.random();
                let pong = Message::Pong { tx, observed: from };
                if let Ok(datagram) = disco::seal(&peer.disco, &self.public, &peer.obfs, &pong, padding, nonce) {
                    self.outputs.push_back(Output::Udp { to: from, datagram });
                }
                let known = peer.paths.contains_key(&from);
                if !known && peer.paths.len() < MAX_PATHS {
                    peer.paths.insert(from, Path::default());
                }
                if peer.best.is_none() && !peer.pings.values().any(|&(addr, _)| addr == from) {
                    self.ping(now, id, from);
                }
            }
            Message::Pong { tx, .. } => {
                let Some((addr, sent)) = peer.pings.remove(&tx) else { return };
                if addr != from {
                    return;
                }
                heard(peer, from, now, Some(now.duration_since(sent)));
            }
        }
    }

    fn flush(&mut self, now: Instant) {
        while let Some(event) = self.node.poll_event() {
            if let Event::Unreachable(id) = event
                && self.peers.get(&id).is_some_and(|peer| !peer.online)
            {
                self.remove(id);
            }
        }
        while let Some(transmit) = self.node.poll_transmit() {
            let Some(peer) = self.peers.get(&transmit.peer) else { continue };
            if let Some(best) = peer.best {
                self.outputs.push_back(Output::Udp { to: best, datagram: transmit.datagram });
                continue;
            }
            let Some(server) = &self.server else { continue };
            if server.udp_fresh_until.is_some_and(|until| now < until) {
                let mut packet = relay_packet(&server.token, peer.address, &transmit.datagram);
                if server.obfs.seal(&mut packet).is_ok() {
                    self.outputs.push_back(Output::Udp { to: server.udp, datagram: packet });
                }
            } else {
                self.outputs.push_back(Output::TcpRelay { to: peer.address, packet: transmit.datagram });
            }
        }
    }
}

fn heard(peer: &mut Peer, from: SocketAddr, now: Instant, latency: Option<Duration>) {
    if !peer.paths.contains_key(&from) && peer.paths.len() >= MAX_PATHS {
        return;
    }
    let path = peer.paths.entry(from).or_default();
    path.heard = Some(now);
    if latency.is_some() {
        path.latency = latency;
    }
    let better = match peer.best {
        None => true,
        Some(best) if best == from => false,
        Some(best) => {
            let current = peer.paths.get(&best);
            let stale = current.and_then(|path| path.heard).is_none_or(|heard| now >= heard + PATH_STALE_AFTER);
            let faster = match (latency, current.and_then(|path| path.latency)) {
                (Some(new), Some(old)) => new * 10 < old * 7,
                _ => false,
            };
            stale || faster
        }
    };
    if better && (latency.is_some() || peer.best.is_none()) {
        tracing::debug!(peer = %peer.address, path = %from, ?latency, "direct path selected");
        peer.best = Some(from);
        peer.probe_at = None;
        peer.attempts = 0;
    }
}

fn ipv4_destination(packet: &[u8]) -> Option<Ipv4Addr> {
    (packet.len() >= 20 && packet[0] >> 4 == 4).then(|| Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]))
}

fn ipv4_source(packet: &[u8]) -> Option<Ipv4Addr> {
    (packet.len() >= 20 && packet[0] >> 4 == 4).then(|| Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]))
}
