use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use rand::RngExt;
use tokio::sync::{mpsc, oneshot};
use weft_proto::PublicKey;
use weft_proto::control::{DISCOVERY_TOKEN_LEN, Network, Peer, ServerKind, ServerMessage, State};
use weft_session::Tai64N;

use crate::config::Config;
use crate::db::{Db, DbError};
use crate::limiter::Limiter;

pub type Token = [u8; DISCOVERY_TOKEN_LEN];
pub type SharedHub = Arc<Mutex<Hub>>;

pub struct Hub {
    pub db: Db,
    pub config: Config,
    pub limiter: Limiter,
    sessions: HashMap<PublicKey, Session>,
    tokens: HashMap<Token, PublicKey>,
    endpoints: HashMap<PublicKey, SocketAddr>,
    timestamps: HashMap<PublicKey, Tai64N>,
}

struct Session {
    conn: u64,
    tx: mpsc::UnboundedSender<ServerMessage>,
    token: Token,
    _kill: oneshot::Sender<()>,
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
            endpoints: HashMap::new(),
            timestamps: HashMap::new(),
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
        conn: u64,
        tx: mpsc::UnboundedSender<ServerMessage>,
        kill: oneshot::Sender<()>,
    ) -> Token {
        let token: Token = rand::rng().random();
        if let Some(old) = self.sessions.insert(key, Session { conn, tx, token, _kill: kill }) {
            self.tokens.remove(&old.token);
        }
        self.tokens.insert(token, key);
        self.endpoints.remove(&key);
        token
    }

    pub fn unregister(&mut self, key: &PublicKey, conn: u64) -> bool {
        if !self.sessions.get(key).is_some_and(|session| session.conn == conn) {
            return false;
        }
        if let Some(session) = self.sessions.remove(key) {
            self.tokens.remove(&session.token);
        }
        self.endpoints.remove(key);
        true
    }

    pub fn discovered(&mut self, token: &Token, addr: SocketAddr) -> Option<PublicKey> {
        let key = *self.tokens.get(token)?;
        (self.endpoints.insert(key, addr) != Some(addr)).then_some(key)
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
                let online = self.sessions.contains_key(&device.key);
                Peer {
                    key: device.key.as_bytes().to_vec(),
                    nickname: device.nickname,
                    address: u32::from(device.address),
                    online,
                    endpoint: online.then(|| self.endpoints.get(&device.key)).flatten().map(|&addr| addr.into()),
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
