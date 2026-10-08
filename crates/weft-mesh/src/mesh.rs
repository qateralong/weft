use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
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
const UDP_CHECK: Duration = Duration::from_secs(10);
const MAX_PATHS: usize = 32;
/// Opening packets from the symmetric side use a low TTL so they create a mapping in its own
/// NAT but expire before the peer's NAT. A Linux conntrack NAT on the peer's side would remember
/// such a flow, and once the peer's probe hits it, move all its later probes to another port.
/// The distance to our NAT is unknown, so rounds try larger TTLs with fresh sockets, the safe
/// low ones first and with more time before the next.
const SPRAY_TTLS: [u8; 5] = [2, 3, 4, 5, 6];
const SPRAY_OPEN_AT: [Duration; 5] =
    [Duration::ZERO, Duration::from_secs(3), Duration::from_secs(9), Duration::from_secs(12), Duration::from_secs(15)];
const SPRAY_ROUND_SOCKETS: SocketId = 96;
/// Extra sockets the side behind a symmetric NAT opens towards one peer.
pub const SPRAY_SOCKETS: SocketId = SPRAY_ROUND_SOCKETS * SPRAY_TTLS.len() as SocketId;
const SPRAY_PROBES: usize = 512;
const SPRAY_ROUNDS: u32 = 7;
const SPRAY_INTERVAL: Duration = Duration::from_secs(3);
const SPRAY_SCAN_DELAY: Duration = Duration::from_millis(500);
const SPRAY_COOLDOWN: Duration = Duration::from_secs(300);
const SPRAY_AFTER: u32 = 2;
const MIN_SPRAY_PORT: u16 = 1024;

/// Identifies one of the coordination servers the device is connected to.
pub type ServerId = u32;

/// Index of a local UDP socket; 0 is the main socket, the others only exist while spraying.
pub type SocketId = u16;

/// A local socket and a remote address: one end-to-end UDP path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Via {
    pub socket: SocketId,
    pub addr: SocketAddr,
}

impl Via {
    pub fn main(addr: SocketAddr) -> Self {
        Self { socket: 0, addr }
    }
}
const MAX_DISCO_PADDING: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerConfig {
    pub key: PublicKey,
    pub address: Ipv4Addr,
    pub online: bool,
    pub candidates: Vec<SocketAddr>,
    /// The peer's address as the server sees it.
    pub endpoint: Option<SocketAddr>,
    /// The server that relays for this peer and passes call-me-maybe.
    pub server: ServerId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    /// `ttl` is set only for packets that must not travel far.
    Udp {
        socket: SocketId,
        to: SocketAddr,
        datagram: Vec<u8>,
        ttl: Option<u8>,
    },
    TcpRelay {
        server: ServerId,
        to: Ipv4Addr,
        packet: Vec<u8>,
    },
    CallMeMaybe {
        server: ServerId,
        peer: PublicKey,
    },
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// Our address as any server sees it.
    pub observed: Option<SocketAddr>,
    pub servers: Vec<ServerReport>,
    pub peers: Vec<PeerReport>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerReport {
    pub id: ServerId,
    /// Whether UDP to the server answered recently; `None` before the first answer is due.
    pub udp: Option<bool>,
    pub observed: Option<SocketAddr>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerReport {
    pub key: PublicKey,
    pub link: PeerLink,
    pub candidates: Vec<SocketAddr>,
    /// Our address as this peer sees it.
    pub observed: Option<SocketAddr>,
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
    servers: BTreeMap<ServerId, Server>,
    locals: Vec<Local>,
    flood: Bucket,
    outputs: VecDeque<Output>,
    rng: StdRng,
    udp_port: Option<u16>,
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
    since: Instant,
}

struct Peer {
    key: PublicKey,
    address: Ipv4Addr,
    online: bool,
    obfs: ObfsKey,
    disco: DiscoKey,
    candidates: Vec<SocketAddr>,
    endpoint: Option<SocketAddr>,
    server: ServerId,
    paths: HashMap<Via, Path>,
    best: Option<Via>,
    probe_at: Option<Instant>,
    attempts: u32,
    pings: HashMap<TxId, (Via, Instant)>,
    observed: Option<SocketAddr>,
    spray: Option<Spray>,
    sprayed_at: Option<Instant>,
}

/// Birthday-style punching for a cone NAT on one side and a symmetric NAT on the other.
#[derive(Clone, Copy, Debug)]
struct Spray {
    role: SprayRole,
    next: Instant,
    rounds: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SprayRole {
    /// We are behind the symmetric NAT: open many mappings towards the peer.
    Open(SocketAddr),
    /// The peer is: probe random ports on its public address.
    Scan(IpAddr),
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
            servers: BTreeMap::new(),
            locals: Vec::new(),
            flood: Bucket::default(),
            outputs: VecDeque::new(),
            rng,
            udp_port: None,
        }
    }

    /// The local port of the main socket, to tell whether our NAT changes ports.
    pub fn set_udp_port(&mut self, port: u16) {
        self.udp_port = Some(port);
    }

    /// Sockets that still carry a path or a probe; the others can be closed.
    pub fn sockets(&self) -> BTreeSet<SocketId> {
        let mut sockets = BTreeSet::from([0]);
        for peer in self.peers.values() {
            sockets.extend(peer.best.map(|via| via.socket));
            sockets.extend(peer.pings.values().map(|(via, _)| via.socket));
            if matches!(peer.spray, Some(Spray { role: SprayRole::Open(_), .. })) {
                sockets.extend(1..=SPRAY_SOCKETS);
            }
        }
        sockets
    }

    pub fn public_key(&self) -> PublicKey {
        self.public
    }

    pub fn observed(&self) -> Option<SocketAddr> {
        self.servers.values().find_map(|server| server.observed)
    }

    pub fn set_server(&mut self, now: Instant, id: ServerId, udp: SocketAddr, key: PublicKey, token: Token) {
        self.servers.insert(
            id,
            Server {
                udp,
                obfs: ObfsKey::for_receiver(&key),
                token,
                observed: None,
                udp_fresh_until: None,
                discover_at: now,
                since: now,
            },
        );
        self.tick(now);
    }

    /// Our virtual addresses with their prefix lengths, one per distinct server pool.
    pub fn set_locals(&mut self, addresses: &[(Ipv4Addr, u8)]) {
        self.locals = addresses
            .iter()
            .map(|&(address, prefix)| {
                let host_bits = u32::MAX.checked_shr(u32::from(prefix)).unwrap_or(0);
                Local { address, broadcast: Ipv4Addr::from(u32::from(address) | host_bits) }
            })
            .collect();
    }

    pub fn clear_server(&mut self, id: ServerId) {
        self.servers.remove(&id);
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
            peer.endpoint = config.endpoint;
            peer.server = config.server;
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
        let Some(peer) = self.peers.get_mut(&id) else { return };
        if !candidates.is_empty() {
            peer.candidates = candidates;
        }
        let retry = peer.attempts + 1 >= SPRAY_AFTER;
        self.burst(now, id);
        if retry {
            self.start_spray(now, id);
        }
        self.flush(now);
    }

    pub fn send(&mut self, now: Instant, packet: &[u8]) -> Result<(), SendError> {
        let destination = ipv4_destination(packet).ok_or(SendError::Invalid)?;
        if flood::is_flood(destination, None) || self.locals.iter().any(|local| destination == local.broadcast) {
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

    pub fn receive_udp(
        &mut self,
        now: Instant,
        socket: SocketId,
        from: SocketAddr,
        datagram: &[u8],
    ) -> Option<Delivered> {
        let mut packet = datagram.to_vec();
        let header = self.obfs.open(&mut packet).ok()?;
        let via = Via { socket, addr: from };
        let delivered = match header {
            Header::Observed if socket == 0 => {
                self.on_observed(now, from, &packet);
                None
            }
            Header::Observed => None,
            Header::Relayed => {
                let from_server = socket == 0 && self.servers.values().any(|server| server.udp == from);
                let (source, inner) = parse_relayed(&packet).filter(|_| from_server)?;
                self.on_relayed(now, source, inner)
            }
            Header::Disco => {
                self.on_disco(now, via, &packet);
                None
            }
            header => self.on_session(now, Some(via), header, &packet),
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
        for server in self.servers.values_mut() {
            if now < server.discover_at {
                continue;
            }
            let jitter = self.rng.random_range(0..=2 * DISCOVER_JITTER.as_millis() as u64);
            server.discover_at = now + DISCOVER_INTERVAL - DISCOVER_JITTER + Duration::from_millis(jitter);
            let mut packet = discover_packet(&server.token, self.rng.random_range(0..=64), self.rng.random());
            if server.obfs.seal(&mut packet).is_ok() {
                self.outputs.push_back(Output::Udp { socket: 0, to: server.udp, datagram: packet, ttl: None });
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
            [peer.probe_at, path_check, pings, peer.spray.map(|spray| spray.next)]
        });
        let servers = self.servers.values().map(|server| server.discover_at);
        peers.flatten().chain(servers).chain(self.node.next_timeout()).min()
    }

    pub fn poll_output(&mut self) -> Option<Output> {
        self.outputs.pop_front()
    }

    pub fn report(&self, now: Instant) -> Report {
        let servers = self
            .servers
            .iter()
            .map(|(&id, server)| {
                let fresh = server.udp_fresh_until.is_some_and(|until| now < until);
                let udp = (fresh || server.observed.is_some() || now >= server.since + UDP_CHECK).then_some(fresh);
                ServerReport { id, udp, observed: server.observed }
            })
            .collect();
        let mut peers: Vec<PeerReport> = self
            .peers
            .values()
            .filter_map(|peer| {
                Some(PeerReport {
                    key: peer.key,
                    link: self.link(&peer.key)?,
                    candidates: peer.candidates.clone(),
                    observed: peer.observed,
                })
            })
            .collect();
        peers.sort_by_key(|peer| peer.key);
        Report { observed: self.observed(), servers, peers }
    }

    pub fn link(&self, key: &PublicKey) -> Option<PeerLink> {
        let id = *self.by_key.get(key)?;
        let peer = self.peers.get(&id)?;
        if !self.node.is_established(id) {
            return Some(PeerLink::Connecting);
        }
        Some(match peer.best {
            Some(via) => {
                PeerLink::Direct { addr: via.addr, latency: peer.paths.get(&via).and_then(|path| path.latency) }
            }
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
                endpoint: config.endpoint,
                server: config.server,
                paths: HashMap::new(),
                best: None,
                probe_at: Some(now),
                attempts: 0,
                pings: HashMap::new(),
                observed: None,
                spray: None,
                sprayed_at: None,
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
                    tracing::debug!(peer = %peer.address, path = %best.addr, "direct path lost");
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
        if self.peers.get(&id).and_then(|peer| peer.spray).is_some_and(|spray| now >= spray.next) {
            self.spray_round(now, id);
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
        let (key, server) = (peer.key, peer.server);
        let spray = peer.attempts >= SPRAY_AFTER;
        if self.servers.contains_key(&server) {
            self.outputs.push_back(Output::CallMeMaybe { server, peer: key });
        } else {
            self.burst(now, id);
        }
        if spray {
            self.start_spray(now, id);
        }
    }

    /// Whether our NAT gave the main socket a different public port than the local one.
    fn port_changing(&self) -> bool {
        self.observed().zip(self.udp_port).is_some_and(|(observed, port)| observed.port() != port)
    }

    fn start_spray(&mut self, now: Instant, id: PeerId) {
        let own = self.port_changing();
        let Some(peer) = self.peers.get_mut(&id) else { return };
        let has_server = self.servers.contains_key(&peer.server);
        let cooling = peer.sprayed_at.is_some_and(|at| now < at + SPRAY_COOLDOWN);
        if !has_server || !peer.online || peer.best.is_some() || peer.spray.is_some() || cooling {
            return;
        }
        let Some(endpoint) = peer.endpoint else { return };
        let peer_changing = !peer.candidates.iter().any(|&c| c != endpoint && c.port() == endpoint.port());
        let (role, start) = match (own, peer_changing) {
            (true, false) => (SprayRole::Open(endpoint), now),
            (false, true) => (SprayRole::Scan(endpoint.ip()), now + SPRAY_SCAN_DELAY),
            _ => return,
        };
        tracing::debug!(peer = %peer.address, ?role, "spraying for a symmetric NAT");
        peer.spray = Some(Spray { role, next: start, rounds: 0 });
        peer.sprayed_at = Some(now);
    }

    fn spray_round(&mut self, now: Instant, id: PeerId) {
        let Some(peer) = self.peers.get_mut(&id) else { return };
        let Some(mut spray) = peer.spray.take() else { return };
        if peer.best.is_some() {
            return;
        }
        let round = spray.rounds as usize;
        spray.rounds += 1;
        spray.next = match spray.role {
            SprayRole::Open(_) => match SPRAY_OPEN_AT.get(round + 1) {
                Some(&at) => now + at - SPRAY_OPEN_AT[round],
                None => now,
            },
            SprayRole::Scan(_) => now + SPRAY_INTERVAL,
        };
        let last = match spray.role {
            SprayRole::Open(_) => SPRAY_TTLS.len() as u32,
            SprayRole::Scan(_) => SPRAY_ROUNDS,
        };
        if spray.rounds < last {
            peer.spray = Some(spray);
        }
        let (targets, ttl): (Vec<Via>, Option<u8>) = match spray.role {
            SprayRole::Open(target) => {
                let first = round as SocketId * SPRAY_ROUND_SOCKETS + 1;
                let sockets = first..first + SPRAY_ROUND_SOCKETS;
                (sockets.map(|socket| Via { socket, addr: target }).collect(), Some(SPRAY_TTLS[round]))
            }
            SprayRole::Scan(ip) => {
                let ports = (0..SPRAY_PROBES).map(|_| self.rng.random_range(MIN_SPRAY_PORT..=u16::MAX));
                (ports.map(|port| Via::main(SocketAddr::new(ip, port))).collect(), None)
            }
        };
        for via in targets {
            self.ping_via(now, id, via, false, ttl);
        }
    }

    fn burst(&mut self, now: Instant, id: PeerId) {
        let Some(peer) = self.peers.get_mut(&id) else { return };
        if !peer.online {
            return;
        }
        let mut targets: Vec<Via> = peer.candidates.iter().copied().map(Via::main).collect();
        let known: Vec<Via> = peer.paths.keys().copied().filter(|via| !targets.contains(via)).collect();
        targets.extend(known);
        tracing::trace!(peer = %peer.address, ?targets, "probing");
        for target in targets {
            self.ping(now, id, target);
        }
    }

    fn ping(&mut self, now: Instant, id: PeerId, to: Via) {
        self.ping_via(now, id, to, true, None);
    }

    /// Sends a disco ping; untracked pings do not create paths until they are answered.
    fn ping_via(&mut self, now: Instant, id: PeerId, to: Via, track: bool, ttl: Option<u8>) {
        let Some(peer) = self.peers.get_mut(&id) else { return };
        let tx: TxId = self.rng.random();
        let padding = self.rng.random_range(0..=MAX_DISCO_PADDING);
        let nonce = self.rng.random();
        let Ok(datagram) = disco::seal(&peer.disco, &self.public, &peer.obfs, &Message::Ping { tx }, padding, nonce)
        else {
            return;
        };
        peer.pings.insert(tx, (to, now));
        if track && (peer.paths.len() < MAX_PATHS || peer.paths.contains_key(&to)) {
            peer.paths.entry(to).or_default().pinged = Some(now);
        }
        self.outputs.push_back(Output::Udp { socket: to.socket, to: to.addr, datagram, ttl });
    }

    fn on_observed(&mut self, now: Instant, from: SocketAddr, packet: &[u8]) {
        let Some((token, observed)) = parse_observed(packet) else { return };
        let Some(server) = self.servers.values_mut().find(|server| server.udp == from && server.token == token) else {
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

    fn on_session(&mut self, now: Instant, from: Option<Via>, header: Header, packet: &[u8]) -> Option<Delivered> {
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
        let accepted = self.locals.iter().any(|local| destination == local.address || destination == local.broadcast)
            || self.locals.is_empty()
            || flood::is_flood(destination, None);
        if !accepted {
            tracing::debug!(peer = %peer.address, %destination, "dropped packet for a foreign destination");
            return None;
        }
        Some(Delivered { source: peer.address, packet })
    }

    fn on_disco(&mut self, now: Instant, from: Via, packet: &[u8]) {
        let Some(&id) = disco::sender(packet).and_then(|key| self.by_key.get(&key)) else { return };
        let Some(peer) = self.peers.get_mut(&id) else { return };
        let Some(message) = disco::open(&peer.disco, packet) else { return };
        tracing::trace!(peer = %peer.address, ?from, ?message, "disco");
        match message {
            Message::Ping { tx } => {
                let padding = self.rng.random_range(0..=MAX_DISCO_PADDING);
                let nonce = self.rng.random();
                let pong = Message::Pong { tx, observed: from.addr };
                if let Ok(datagram) = disco::seal(&peer.disco, &self.public, &peer.obfs, &pong, padding, nonce) {
                    self.outputs.push_back(Output::Udp { socket: from.socket, to: from.addr, datagram, ttl: None });
                }
                let known = peer.paths.contains_key(&from);
                if !known && peer.paths.len() < MAX_PATHS {
                    peer.paths.insert(from, Path::default());
                }
                let pinging =
                    peer.paths.get(&from).and_then(|path| path.pinged).is_some_and(|at| now < at + PING_TIMEOUT);
                if peer.best.is_none() && !pinging {
                    self.ping(now, id, from);
                }
            }
            Message::Pong { tx, observed } => {
                let Some((addr, sent)) = peer.pings.remove(&tx) else { return };
                if addr != from {
                    return;
                }
                peer.observed = Some(observed);
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
                self.outputs.push_back(Output::Udp {
                    socket: best.socket,
                    to: best.addr,
                    datagram: transmit.datagram,
                    ttl: None,
                });
                continue;
            }
            let Some(server) = self.servers.get(&peer.server) else { continue };
            if server.udp_fresh_until.is_some_and(|until| now < until) {
                let mut packet = relay_packet(&server.token, peer.address, &transmit.datagram);
                if server.obfs.seal(&mut packet).is_ok() {
                    self.outputs.push_back(Output::Udp { socket: 0, to: server.udp, datagram: packet, ttl: None });
                }
            } else {
                let server = peer.server;
                self.outputs.push_back(Output::TcpRelay { server, to: peer.address, packet: transmit.datagram });
            }
        }
    }
}

fn heard(peer: &mut Peer, from: Via, now: Instant, latency: Option<Duration>) {
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
        tracing::debug!(peer = %peer.address, path = %from.addr, socket = from.socket, ?latency, "direct path selected");
        peer.best = Some(from);
        peer.probe_at = None;
        peer.attempts = 0;
        peer.spray = None;
    }
}

fn ipv4_destination(packet: &[u8]) -> Option<Ipv4Addr> {
    (packet.len() >= 20 && packet[0] >> 4 == 4).then(|| Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]))
}

fn ipv4_source(packet: &[u8]) -> Option<Ipv4Addr> {
    (packet.len() >= 20 && packet[0] >> 4 == 4).then(|| Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]))
}
