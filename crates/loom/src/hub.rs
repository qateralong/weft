use std::collections::{BTreeSet, HashMap};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rand::RngExt;
use tokio::sync::{mpsc, oneshot};
use weft_proto::control::{Endpoint, Network, Peer, PeerCandidates, ServerKind, ServerMessage, State};
use weft_proto::loom::Token;
use weft_proto::{ObfsKey, PublicKey};
use weft_session::Tai64N;

use crate::config::Config;
use crate::db::{Db, DbError};
use crate::limiter::Limiter;

pub const MAX_CANDIDATES: usize = 16;
const UDP_FRESH: Duration = Duration::from_secs(60);

pub type SharedHub = Arc<Mutex<Hub>>;

pub struct Hub {
    pub db: Db,
    pub config: Config,
    pub limiter: Limiter,
    sessions: HashMap<PublicKey, Session>,
    tokens: HashMap<Token, PublicKey>,
    addresses: HashMap<Ipv4Addr, PublicKey>,
    timestamps: HashMap<PublicKey, Tai64N>,
    relations: HashMap<(PublicKey, PublicKey), bool>,
}

struct Session {
    conn: u64,
    tx: mpsc::UnboundedSender<ServerMessage>,
    token: Token,
    address: Ipv4Addr,
    obfs: ObfsKey,
    endpoint: Option<SocketAddr>,
    discovered_at: Option<Instant>,
    candidates: Vec<SocketAddr>,
    _kill: oneshot::Sender<()>,
}

pub struct Route {
    pub source: Ipv4Addr,
    pub delivery: Delivery,
}

pub enum Delivery {
    Udp(SocketAddr, ObfsKey),
    Tcp(mpsc::UnboundedSender<ServerMessage>),
}

pub fn lock(hub: &SharedHub) -> MutexGuard<'_, Hub> {
    hub.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Hub {
    pub fn new(db: Db, config: Config) -> Self {
        Self {
            db,
            config,
            limiter: Limiter::default(),
            sessions: HashMap::new(),
            tokens: HashMap::new(),
            addresses: HashMap::new(),
            timestamps: HashMap::new(),
            relations: HashMap::new(),
        }
    }

    pub fn accept_timestamp(&mut self, key: PublicKey, timestamp: Tai64N) -> bool {
        match self.timestamps.get(&key) {
            Some(&last) if timestamp <= last => false,
            _ => {
                self.timestamps.insert(key, timestamp);
                true
            }
        }
    }

    pub fn register(
        &mut self,
        key: PublicKey,
        address: Ipv4Addr,
        conn: u64,
        tx: mpsc::UnboundedSender<ServerMessage>,
        kill: oneshot::Sender<()>,
    ) -> Token {
        let token: Token = rand::rng().random();
        let session = Session {
            conn,
            tx,
            token,
            address,
            obfs: ObfsKey::for_receiver(&key),
            endpoint: None,
            discovered_at: None,
            candidates: Vec::new(),
            _kill: kill,
        };
        if let Some(old) = self.sessions.insert(key, session) {
            self.tokens.remove(&old.token);
            self.addresses.remove(&old.address);
        }
        self.tokens.insert(token, key);
        self.addresses.insert(address, key);
        token
    }

    pub fn unregister(&mut self, key: &PublicKey, conn: u64) -> bool {
        if !self.sessions.get(key).is_some_and(|session| session.conn == conn) {
            return false;
        }
        if let Some(session) = self.sessions.remove(key) {
            self.tokens.remove(&session.token);
            self.addresses.remove(&session.address);
        }
        true
    }

    pub fn token_owner(&self, token: &Token) -> Option<PublicKey> {
        self.tokens.get(token).copied()
    }

    pub fn discovered(&mut self, token: &Token, addr: SocketAddr, now: Instant) -> Option<PublicKey> {
        let key = *self.tokens.get(token)?;
        let session = self.sessions.get_mut(&key)?;
        session.discovered_at = Some(now);
        (session.endpoint.replace(addr) != Some(addr)).then_some(key)
    }

    pub fn set_candidates(&mut self, key: &PublicKey, candidates: Vec<SocketAddr>) -> bool {
        let Some(session) = self.sessions.get_mut(key) else { return false };
        let changed = session.candidates != candidates;
        session.candidates = candidates;
        changed
    }

    pub fn memberships_changed(&mut self) {
        self.relations.clear();
    }

    pub fn related(&mut self, a: &PublicKey, b: &PublicKey) -> bool {
        if let Some(&related) = self.relations.get(&(*a, *b)) {
            return related;
        }
        let related = a != b && self.db.related(a).is_ok_and(|set| set.contains(b));
        if self.relations.len() > 100_000 {
            self.relations.clear();
        }
        self.relations.insert((*a, *b), related);
        related
    }

    pub fn call_me_maybe(&mut self, from: &PublicKey, to: &PublicKey) {
        if !self.related(from, to) {
            return;
        }
        let (Some(a), Some(b)) = (self.sessions.get(from), self.sessions.get(to)) else { return };
        let candidates = |session: &Session| -> Vec<Endpoint> {
            session.endpoint.iter().chain(&session.candidates).map(|&addr| addr.into()).collect()
        };
        let message = |key: &PublicKey, session: &Session| {
            ServerMessage::push(ServerKind::CallMeMaybe(PeerCandidates {
                key: key.as_bytes().to_vec(),
                endpoints: candidates(session),
            }))
        };
        let _ = b.tx.send(message(from, a));
        let _ = a.tx.send(message(to, b));
    }

    pub fn route(&mut self, source: &PublicKey, destination: Ipv4Addr, now: Instant) -> Option<Route> {
        let source_address = self.sessions.get(source)?.address;
        let destination_key = *self.addresses.get(&destination)?;
        if !self.related(source, &destination_key) {
            return None;
        }
        let session = self.sessions.get(&destination_key)?;
        let fresh = session.discovered_at.is_some_and(|at| now.duration_since(at) < UDP_FRESH);
        let delivery = match session.endpoint {
            Some(endpoint) if fresh => Delivery::Udp(endpoint, session.obfs.clone()),
            _ => Delivery::Tcp(session.tx.clone()),
        };
        Some(Route { source: source_address, delivery })
    }

    pub fn state_for(&self, key: &PublicKey) -> Result<State, DbError> {
        let memberships = self.db.memberships(key)?;
        let others: BTreeSet<PublicKey> =
            memberships.iter().flat_map(|m| m.members.iter().copied()).filter(|other| other != key).collect();
        let devices = self.db.devices(&others)?;
        let networks = memberships
            .into_iter()
            .map(|m| Network {
                name: m.name,
                role: m.role as i32,
                members: m.members.iter().map(|k| k.as_bytes().to_vec()).collect(),
            })
            .collect();
        let mut peers: Vec<Peer> = devices
            .into_values()
            .map(|device| {
                let session = self.sessions.get(&device.key);
                Peer {
                    key: device.key.as_bytes().to_vec(),
                    nickname: device.nickname,
                    address: u32::from(device.address),
                    online: session.is_some(),
                    endpoint: session.and_then(|s| s.endpoint).map(Endpoint::from),
                    candidates: session.map(|s| s.candidates.iter().map(|&c| c.into()).collect()).unwrap_or_default(),
                }
            })
            .collect();
        peers.sort_by_key(|peer| peer.address);
        Ok(State { networks, peers })
    }

    pub fn notify(&self, keys: &BTreeSet<PublicKey>) {
        for key in keys {
            let Some(session) = self.sessions.get(key) else { continue };
            match self.state_for(key) {
                Ok(state) => {
                    let _ = session.tx.send(ServerMessage::push(ServerKind::State(state)));
                }
                Err(error) => tracing::warn!(%error, "cannot build state"),
            }
        }
    }

    pub fn notify_related(&self, key: &PublicKey) {
        match self.db.related(key) {
            Ok(related) => self.notify(&related),
            Err(error) => tracing::warn!(%error, "cannot load related devices"),
        }
    }
}
