use std::collections::{BTreeSet, HashMap};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rand::RngExt;
use tokio::sync::oneshot;
use weft_proto::control::{
    Endpoint, MUTE_FOREVER, MemberDetails, Network, Peer, PeerCandidates, Role, ServerKind, ServerMessage, State,
};
use weft_proto::loom::Token;
use weft_proto::{ObfsKey, PublicKey};
use weft_session::Tai64N;

use crate::config::Config;
use crate::db::{Db, DbError, MUTED_FOREVER, unix_now};
use crate::limiter::Limiter;
use crate::outbox::Outbox;
use crate::relay::{Bucket, Traffic};

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
    relay: Traffic,
    relay_bucket: Option<Bucket>,
    started: Instant,
}

struct Session {
    conn: u64,
    tx: Outbox,
    token: Token,
    address: Ipv4Addr,
    obfs: ObfsKey,
    endpoint: Option<SocketAddr>,
    discovered_at: Option<Instant>,
    candidates: Vec<SocketAddr>,
    relay: Traffic,
    relay_bucket: Option<Bucket>,
    _kill: oneshot::Sender<()>,
}

pub struct Route {
    pub source: Ipv4Addr,
    pub delivery: Delivery,
}

pub enum Delivery {
    Udp(SocketAddr, ObfsKey),
    Tcp(Outbox),
}

/// Devices that belong to nothing are forgotten after this long without connecting.
const STALE_DEVICE: Duration = Duration::from_secs(90 * 24 * 3600);
const PRUNE_EVERY: u32 = 720;

/// Lifts ended mutes every few seconds and forgets stale devices every hour.
pub async fn housekeeping(hub: SharedHub) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    let mut ticks = 0u32;
    loop {
        interval.tick().await;
        let mut hub = lock(&hub);
        hub.expire_mutes();
        if ticks.is_multiple_of(PRUNE_EVERY) {
            hub.prune_devices();
        }
        ticks = ticks.wrapping_add(1);
    }
}

pub fn lock(hub: &SharedHub) -> MutexGuard<'_, Hub> {
    hub.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Hub {
    pub fn new(db: Db, config: Config) -> Self {
        let now = Instant::now();
        Self {
            relay: Traffic::default(),
            relay_bucket: Bucket::mbit(config.relay_total_mbit, now),
            started: now,
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
        tx: Outbox,
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
            relay: Traffic::default(),
            relay_bucket: Bucket::mbit(self.config.relay_mbit, Instant::now()),
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
        let related = a != b && self.db.reachable(a, unix_now()).is_ok_and(|set| set.contains(b));
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
        b.tx.send(message(from, a));
        a.tx.send(message(to, b));
    }

    pub fn route(&mut self, source: &PublicKey, destination: Ipv4Addr, len: usize, now: Instant) -> Option<Route> {
        let source_address = self.sessions.get(source)?.address;
        let destination_key = *self.addresses.get(&destination)?;
        if !self.related(source, &destination_key) || !self.sessions.contains_key(&destination_key) {
            return None;
        }
        let sender = self.sessions.get_mut(source)?;
        let allowed = sender.relay_bucket.as_mut().is_none_or(|bucket| bucket.take(len, now))
            && self.relay_bucket.as_mut().is_none_or(|bucket| bucket.take(len, now));
        if !allowed {
            sender.relay.dropped += 1;
            self.relay.dropped += 1;
            return None;
        }
        sender.relay.add(len);
        self.relay.add(len);
        let session = self.sessions.get(&destination_key)?;
        let fresh = session.discovered_at.is_some_and(|at| now.duration_since(at) < UDP_FRESH);
        let delivery = match session.endpoint {
            Some(endpoint) if fresh => Delivery::Udp(endpoint, session.obfs.clone()),
            _ => Delivery::Tcp(session.tx.clone()),
        };
        Some(Route { source: source_address, delivery })
    }

    pub fn online_count(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_online(&self, key: &PublicKey) -> bool {
        self.sessions.contains_key(key)
    }

    pub fn relay_traffic(&self, key: &PublicKey) -> Traffic {
        self.sessions.get(key).map(|session| session.relay).unwrap_or_default()
    }

    pub fn relay_total(&self) -> Traffic {
        self.relay
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// Closes the device's control connection.
    pub fn disconnect(&mut self, key: &PublicKey) -> bool {
        let Some(session) = self.sessions.remove(key) else { return false };
        self.tokens.remove(&session.token);
        self.addresses.remove(&session.address);
        true
    }

    /// What `key` sees: its networks and every member, where muted ones show as offline so the
    /// client does not try to reach them.
    pub fn state_for(&self, key: &PublicKey) -> Result<State, DbError> {
        let memberships = self.db.memberships(key)?;
        let others: BTreeSet<PublicKey> =
            memberships.iter().flat_map(|m| m.members.iter().copied()).filter(|other| other != key).collect();
        let devices = self.db.devices(&others)?;
        let avatars = self.db.avatar_ids(&others)?;
        let reachable = self.db.reachable(key, unix_now())?;
        let networks = memberships
            .into_iter()
            .map(|m| Network {
                name: m.name,
                role: m.role as i32,
                members: m.members.iter().map(|k| k.as_bytes().to_vec()).collect(),
                locked: m.locked,
                approval: m.approval,
                requests: if m.role >= Role::Admin { m.requests as u32 } else { 0 },
                details: m
                    .details
                    .iter()
                    .map(|detail| MemberDetails {
                        key: detail.key.as_bytes().to_vec(),
                        role: detail.role as i32,
                        muted_until: match detail.muted_until {
                            MUTED_FOREVER => MUTE_FOREVER,
                            until => until.max(0) as u64,
                        },
                    })
                    .collect(),
            })
            .collect();
        let mut peers: Vec<Peer> = devices
            .into_values()
            .map(|device| {
                let session = self.sessions.get(&device.key).filter(|_| reachable.contains(&device.key));
                Peer {
                    key: device.key.as_bytes().to_vec(),
                    avatar: avatars.get(&device.key).cloned().unwrap_or_default(),
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
                    session.tx.send(ServerMessage::push(ServerKind::State(state)));
                }
                Err(error) => tracing::warn!(%error, "cannot build state"),
            }
        }
    }

    fn prune_devices(&mut self) {
        let online: BTreeSet<PublicKey> = self.sessions.keys().copied().collect();
        match self.db.prune_devices(unix_now() - STALE_DEVICE.as_secs() as i64, &online) {
            Ok(0) => {}
            Ok(removed) => tracing::info!(removed, "forgot stale devices"),
            Err(error) => tracing::warn!(%error, "cannot prune devices"),
        }
    }

    /// Lifts mutes whose time is up and tells the members of their networks.
    pub fn expire_mutes(&mut self) {
        let networks = match self.db.expire_mutes(unix_now()) {
            Ok(networks) => networks,
            Err(error) => return tracing::warn!(%error, "cannot expire mutes"),
        };
        if networks.is_empty() {
            return;
        }
        self.memberships_changed();
        let keys: BTreeSet<PublicKey> =
            networks.iter().filter_map(|&network| self.db.member_keys(network).ok()).flatten().collect();
        self.notify(&keys);
    }

    pub fn notify_related(&self, key: &PublicKey) {
        match self.db.related(key) {
            Ok(related) => self.notify(&related),
            Err(error) => tracing::warn!(%error, "cannot load related devices"),
        }
    }
}
