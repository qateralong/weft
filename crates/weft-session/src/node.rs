use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant, SystemTime};

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use snow::{HandshakeState, StatelessTransportState};
use weft_proto::padding::{BLOCK, ip_packet_len, padded_len};
use weft_proto::{HEADER_LEN, Header, ObfsKey, PacketError, PublicKey, TAG_LEN};

use crate::error::Error;
use crate::keys::StaticKeypair;
use crate::noise::{self, MAX_HANDSHAKE_LEN, MAX_HANDSHAKE_PADDING};
use crate::replay::{REJECT_AFTER_MESSAGES, ReplayWindow};
use crate::tai64n::{TAI64N_LEN, Tai64N};
use crate::timers::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerId(u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transmit {
    pub peer: PeerId,
    pub datagram: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
    pub peer: PeerId,
    pub packet: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Established(PeerId),
    Unreachable(PeerId),
}

pub struct Node {
    shared: Shared,
    peers: HashMap<PeerId, Peer>,
    by_key: HashMap<PublicKey, PeerId>,
    next_peer: u64,
}

struct Shared {
    keypair: StaticKeypair,
    obfs: ObfsKey,
    indices: HashMap<u32, PeerId>,
    rng: StdRng,
    epoch: Instant,
    epoch_wall: SystemTime,
    transmits: VecDeque<Transmit>,
    events: VecDeque<Event>,
}

struct Peer {
    key: PublicKey,
    obfs: ObfsKey,
    handshake: Option<Handshake>,
    current: Option<Session>,
    previous: Option<Session>,
    next: Option<Session>,
    last_timestamp: Option<Tai64N>,
    queue: VecDeque<Vec<u8>>,
    keepalive_at: Option<Instant>,
    trying_since: Option<Instant>,
    unreachable: bool,
}

struct Handshake {
    state: HandshakeState,
    local_index: u32,
    retry_at: Instant,
}

struct Session {
    transport: StatelessTransportState,
    local_index: u32,
    remote_index: u32,
    created: Instant,
    initiator: bool,
    send_counter: u64,
    replay: ReplayWindow,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Current,
    Previous,
    Next,
}

const SLOTS: [Slot; 3] = [Slot::Current, Slot::Previous, Slot::Next];

impl Node {
    pub fn new(keypair: StaticKeypair, now: Instant, wall: SystemTime) -> Result<Self, getrandom::Error> {
        let mut seed = [0; 32];
        getrandom::fill(&mut seed)?;
        Ok(Self::with_rng(keypair, now, wall, StdRng::from_seed(seed)))
    }

    pub fn with_seed(keypair: StaticKeypair, now: Instant, wall: SystemTime, seed: u64) -> Self {
        Self::with_rng(keypair, now, wall, StdRng::seed_from_u64(seed))
    }

    fn with_rng(keypair: StaticKeypair, now: Instant, wall: SystemTime, rng: StdRng) -> Self {
        let shared = Shared {
            obfs: ObfsKey::for_receiver(&keypair.public()),
            keypair,
            indices: HashMap::new(),
            rng,
            epoch: now,
            epoch_wall: wall,
            transmits: VecDeque::new(),
            events: VecDeque::new(),
        };
        Self { shared, peers: HashMap::new(), by_key: HashMap::new(), next_peer: 0 }
    }

    pub fn public_key(&self) -> PublicKey {
        self.shared.keypair.public()
    }

    pub fn add_peer(&mut self, now: Instant, key: PublicKey) -> PeerId {
        if let Some(&id) = self.by_key.get(&key) {
            return id;
        }
        let id = PeerId(self.next_peer);
        self.next_peer += 1;
        let mut peer = Peer {
            key,
            obfs: ObfsKey::for_receiver(&key),
            handshake: None,
            current: None,
            previous: None,
            next: None,
            last_timestamp: None,
            queue: VecDeque::new(),
            keepalive_at: None,
            trying_since: None,
            unreachable: false,
        };
        if let Err(error) = start_handshake(&mut self.shared, id, &mut peer, now) {
            tracing::warn!(?id, %error, "handshake start failed");
        }
        self.by_key.insert(key, id);
        self.peers.insert(id, peer);
        id
    }

    pub fn remove_peer(&mut self, id: PeerId) -> bool {
        let Some(mut peer) = self.peers.remove(&id) else {
            return false;
        };
        self.by_key.remove(&peer.key);
        if let Some(handshake) = peer.handshake.take() {
            self.shared.release_index(handshake.local_index);
        }
        for slot in SLOTS {
            if let Some(session) = peer.slot_mut(slot).take() {
                self.shared.release_index(session.local_index);
            }
        }
        true
    }

    pub fn peer_id(&self, key: &PublicKey) -> Option<PeerId> {
        self.by_key.get(key).copied()
    }

    pub fn peer_key(&self, id: PeerId) -> Option<PublicKey> {
        self.peers.get(&id).map(|peer| peer.key)
    }

    pub fn is_established(&self, id: PeerId) -> bool {
        self.peers.get(&id).is_some_and(|peer| peer.current.is_some())
    }

    pub fn reconnect(&mut self, now: Instant, id: PeerId) {
        if let Some(peer) = self.peers.get_mut(&id).filter(|peer| peer.current.is_none())
            && let Err(error) = start_handshake(&mut self.shared, id, peer, now)
        {
            tracing::warn!(?id, %error, "handshake start failed");
        }
    }

    pub fn send(&mut self, now: Instant, id: PeerId, packet: &[u8]) -> Result<(), Error> {
        if ip_packet_len(packet)? != Some(packet.len()) {
            return Err(PacketError::InvalidPayload.into());
        }
        let peer = self.peers.get_mut(&id).ok_or(Error::UnknownPeer)?;
        let shared = &mut self.shared;
        let rekey = match peer.current.as_mut().filter(|session| session.can_send(now)) {
            Some(session) => {
                let mut plaintext = packet.to_vec();
                plaintext.resize(padded_len(packet.len()), 0);
                encrypt(shared, id, &peer.obfs, session, &plaintext)?;
                peer.keepalive_at = Some(now + shared.keepalive_interval());
                session.needs_rekey(now)
            }
            None => {
                if peer.queue.len() >= MAX_QUEUED_PACKETS {
                    peer.queue.pop_front();
                }
                peer.queue.push_back(packet.to_vec());
                true
            }
        };
        if rekey && peer.handshake.is_none() {
            start_handshake(shared, id, peer, now)?;
        }
        Ok(())
    }

    pub fn receive(&mut self, now: Instant, datagram: &[u8]) -> Result<Received, Error> {
        let mut packet = datagram.to_vec();
        let header = self.shared.obfs.open(&mut packet)?;
        let body = &packet[HEADER_LEN..];
        match header {
            Header::HandshakeInit { sender } => self.on_init(now, sender, body),
            Header::HandshakeResp { sender, receiver } => self.on_resp(now, sender, receiver, body),
            Header::Data { receiver, counter } => self.on_data(now, receiver, counter, body),
            Header::Discover => Err(PacketError::InvalidHeader.into()),
        }
    }

    pub fn tick(&mut self, now: Instant) {
        for (&id, peer) in &mut self.peers {
            if let Err(error) = tick_peer(&mut self.shared, id, peer, now) {
                tracing::warn!(?id, %error, "peer timer failed");
            }
        }
    }

    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.shared.transmits.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.shared.events.pop_front()
    }

    pub fn next_timeout(&self) -> Option<Instant> {
        self.peers.values().filter_map(Peer::next_timeout).min()
    }

    fn on_init(&mut self, now: Instant, sender: u32, body: &[u8]) -> Result<Received, Error> {
        let shared = &mut self.shared;
        let mut state = noise::builder(&shared.keypair).build_responder()?;
        let mut payload = vec![0; body.len()];
        let len = state.read_message(body, &mut payload)?;
        let key =
            state.get_remote_static().and_then(|key| PublicKey::from_slice(key).ok()).ok_or(Error::UnknownPeer)?;
        let id = *self.by_key.get(&key).ok_or(Error::UnknownPeer)?;
        let peer = self.peers.get_mut(&id).ok_or(Error::UnknownPeer)?;
        let timestamp = payload[..len].get(..TAI64N_LEN).and_then(Tai64N::from_slice).ok_or(Error::InvalidPayload)?;
        if peer.last_timestamp.is_some_and(|last| timestamp <= last) {
            return Err(Error::StaleHandshake);
        }
        peer.last_timestamp = Some(timestamp);

        let padding = vec![0; shared.handshake_padding()];
        let mut out = vec![0; HEADER_LEN + MAX_HANDSHAKE_LEN];
        let len = state.write_message(&padding, &mut out[HEADER_LEN..])?;
        out.truncate(HEADER_LEN + len);
        let transport = state.into_stateless_transport_mode()?;
        let local_index = shared.alloc_index(id);
        out[..HEADER_LEN].copy_from_slice(&Header::HandshakeResp { sender: local_index, receiver: sender }.encode());
        peer.obfs.seal(&mut out)?;
        shared.transmits.push_back(Transmit { peer: id, datagram: out });

        let session = Session::new(transport, local_index, sender, now, false);
        if let Some(old) = peer.next.replace(session) {
            shared.release_index(old.local_index);
        }
        Ok(Received { peer: id, packet: None })
    }

    fn on_resp(&mut self, now: Instant, sender: u32, receiver: u32, body: &[u8]) -> Result<Received, Error> {
        let shared = &mut self.shared;
        let id = *shared.indices.get(&receiver).ok_or(Error::UnknownIndex)?;
        let peer = self.peers.get_mut(&id).ok_or(Error::UnknownIndex)?;
        let handshake =
            peer.handshake.as_mut().filter(|handshake| handshake.local_index == receiver).ok_or(Error::UnknownIndex)?;
        let mut payload = vec![0; body.len()];
        handshake.state.read_message(body, &mut payload)?;
        let handshake = peer.handshake.take().expect("handshake checked above");
        let transport = handshake.state.into_stateless_transport_mode()?;
        let session = Session::new(transport, receiver, sender, now, true);
        peer.install(shared, id, session, now);
        peer.flush(shared, id, now, true)?;
        Ok(Received { peer: id, packet: None })
    }

    fn on_data(&mut self, now: Instant, receiver: u32, counter: u64, body: &[u8]) -> Result<Received, Error> {
        let shared = &mut self.shared;
        let id = *shared.indices.get(&receiver).ok_or(Error::UnknownIndex)?;
        let peer = self.peers.get_mut(&id).ok_or(Error::UnknownIndex)?;
        let slot = SLOTS
            .into_iter()
            .find(|&slot| peer.slot(slot).as_ref().is_some_and(|session| session.local_index == receiver))
            .ok_or(Error::UnknownIndex)?;
        let session = peer.slot_mut(slot).as_mut().expect("slot checked above");
        if session.expired(now) {
            return Err(Error::Expired);
        }
        let mut plaintext = vec![0; body.len()];
        let len = session.transport.read_message(counter, body, &mut plaintext)?;
        if !session.replay.accept(counter) {
            return Err(Error::Replay);
        }
        plaintext.truncate(len);

        if slot == Slot::Next {
            let session = peer.next.take().expect("slot checked above");
            peer.install(shared, id, session, now);
            peer.flush(shared, id, now, false)?;
        }
        let packet = ip_packet_len(&plaintext)?.map(|len| {
            plaintext.truncate(len);
            plaintext
        });
        Ok(Received { peer: id, packet })
    }
}

impl Shared {
    fn alloc_index(&mut self, id: PeerId) -> u32 {
        loop {
            if let Entry::Vacant(entry) = self.indices.entry(self.rng.random()) {
                let index = *entry.key();
                entry.insert(id);
                return index;
            }
        }
    }

    fn release_index(&mut self, index: u32) {
        self.indices.remove(&index);
    }

    fn timestamp(&self, now: Instant) -> Tai64N {
        Tai64N::from_system_time(self.epoch_wall + now.saturating_duration_since(self.epoch))
    }

    fn handshake_padding(&mut self) -> usize {
        self.rng.random_range(0..=MAX_HANDSHAKE_PADDING)
    }

    fn keepalive_interval(&mut self) -> Duration {
        let jitter = self.rng.random_range(0..=2 * KEEPALIVE_JITTER_MS);
        KEEPALIVE_INTERVAL - Duration::from_millis(KEEPALIVE_JITTER_MS) + Duration::from_millis(jitter)
    }
}

impl Peer {
    fn slot(&self, slot: Slot) -> &Option<Session> {
        match slot {
            Slot::Current => &self.current,
            Slot::Previous => &self.previous,
            Slot::Next => &self.next,
        }
    }

    fn slot_mut(&mut self, slot: Slot) -> &mut Option<Session> {
        match slot {
            Slot::Current => &mut self.current,
            Slot::Previous => &mut self.previous,
            Slot::Next => &mut self.next,
        }
    }

    fn install(&mut self, shared: &mut Shared, id: PeerId, session: Session, now: Instant) {
        if let Some(old) = self.previous.take() {
            shared.release_index(old.local_index);
        }
        self.previous = self.current.replace(session);
        self.keepalive_at = Some(now + shared.keepalive_interval());
        self.trying_since = None;
        self.unreachable = false;
        shared.events.push_back(Event::Established(id));
    }

    fn flush(&mut self, shared: &mut Shared, id: PeerId, now: Instant, confirm: bool) -> Result<(), Error> {
        let Some(session) = self.current.as_mut().filter(|session| session.can_send(now)) else {
            return Ok(());
        };
        let mut sent = false;
        while let Some(mut packet) = self.queue.pop_front() {
            packet.resize(padded_len(packet.len()), 0);
            encrypt(shared, id, &self.obfs, session, &packet)?;
            sent = true;
        }
        if !sent && confirm {
            send_keepalive(shared, id, &self.obfs, session)?;
        }
        Ok(())
    }

    fn needs_handshake(&self, now: Instant) -> bool {
        match (&self.current, &self.next) {
            (Some(current), _) => current.needs_rekey(now),
            (None, Some(next)) => now >= next.created + REKEY_TIMEOUT,
            (None, None) => true,
        }
    }

    fn next_timeout(&self) -> Option<Instant> {
        let expiries = SLOTS.into_iter().filter_map(|slot| self.slot(slot).as_ref()).map(Session::expires_at);
        let handshake = match (&self.handshake, &self.current, &self.next) {
            (Some(handshake), _, _) => Some(handshake.retry_at),
            (None, Some(current), _) => Some(current.rekey_at()),
            (None, None, Some(next)) => Some(next.created + REKEY_TIMEOUT),
            (None, None, None) => None,
        };
        expiries.chain(handshake).chain(self.keepalive_at).min()
    }
}

impl Session {
    fn new(
        transport: StatelessTransportState,
        local_index: u32,
        remote_index: u32,
        now: Instant,
        initiator: bool,
    ) -> Self {
        Self {
            transport,
            local_index,
            remote_index,
            created: now,
            initiator,
            send_counter: 0,
            replay: ReplayWindow::default(),
        }
    }

    fn expires_at(&self) -> Instant {
        self.created + REJECT_AFTER_TIME
    }

    fn expired(&self, now: Instant) -> bool {
        now >= self.expires_at()
    }

    fn can_send(&self, now: Instant) -> bool {
        !self.expired(now) && self.send_counter < REJECT_AFTER_MESSAGES
    }

    fn rekey_at(&self) -> Instant {
        self.created + if self.initiator { REKEY_AFTER_TIME } else { RESPONDER_REKEY_TIME }
    }

    fn needs_rekey(&self, now: Instant) -> bool {
        now >= self.rekey_at() || self.send_counter >= REKEY_AFTER_MESSAGES
    }
}

fn start_handshake(shared: &mut Shared, id: PeerId, peer: &mut Peer, now: Instant) -> Result<(), Error> {
    if let Some(old) = peer.handshake.take() {
        shared.release_index(old.local_index);
    }
    let mut state = noise::builder(&shared.keypair).remote_public_key(peer.key.as_bytes())?.build_initiator()?;
    let mut payload = vec![0; TAI64N_LEN + shared.handshake_padding()];
    payload[..TAI64N_LEN].copy_from_slice(shared.timestamp(now).as_bytes());
    let mut out = vec![0; HEADER_LEN + MAX_HANDSHAKE_LEN];
    let len = state.write_message(&payload, &mut out[HEADER_LEN..])?;
    out.truncate(HEADER_LEN + len);
    let local_index = shared.alloc_index(id);
    out[..HEADER_LEN].copy_from_slice(&Header::HandshakeInit { sender: local_index }.encode());
    peer.obfs.seal(&mut out)?;
    shared.transmits.push_back(Transmit { peer: id, datagram: out });

    let jitter = Duration::from_millis(shared.rng.random_range(0..=REKEY_TIMEOUT_JITTER_MS));
    peer.handshake = Some(Handshake { state, local_index, retry_at: now + REKEY_TIMEOUT + jitter });
    peer.trying_since.get_or_insert(now);
    Ok(())
}

fn tick_peer(shared: &mut Shared, id: PeerId, peer: &mut Peer, now: Instant) -> Result<(), Error> {
    for slot in SLOTS {
        if peer.slot(slot).as_ref().is_some_and(|session| session.expired(now)) {
            let session = peer.slot_mut(slot).take().expect("slot checked above");
            shared.release_index(session.local_index);
        }
    }
    if peer.current.is_none() {
        peer.keepalive_at = None;
    }

    match &peer.handshake {
        Some(handshake) if now >= handshake.retry_at => {
            let unreachable = peer.trying_since.is_some_and(|since| now >= since + UNREACHABLE_AFTER);
            if unreachable && !peer.unreachable {
                peer.unreachable = true;
                peer.queue.clear();
                shared.events.push_back(Event::Unreachable(id));
            }
            start_handshake(shared, id, peer, now)?;
        }
        Some(_) => {}
        None if peer.needs_handshake(now) => start_handshake(shared, id, peer, now)?,
        None => {}
    }

    if peer.keepalive_at.is_some_and(|at| now >= at)
        && let Some(session) = peer.current.as_mut().filter(|session| session.can_send(now))
    {
        send_keepalive(shared, id, &peer.obfs, session)?;
        peer.keepalive_at = Some(now + shared.keepalive_interval());
    }
    Ok(())
}

fn send_keepalive(shared: &mut Shared, id: PeerId, obfs: &ObfsKey, session: &mut Session) -> Result<(), Error> {
    let blocks = shared.rng.random_range(0..=KEEPALIVE_MAX_BLOCKS);
    encrypt(shared, id, obfs, session, &vec![0; blocks * BLOCK])
}

fn encrypt(
    shared: &mut Shared,
    id: PeerId,
    obfs: &ObfsKey,
    session: &mut Session,
    plaintext: &[u8],
) -> Result<(), Error> {
    let counter = session.send_counter;
    let mut out = vec![0; HEADER_LEN + plaintext.len() + TAG_LEN];
    out[..HEADER_LEN].copy_from_slice(&Header::Data { receiver: session.remote_index, counter }.encode());
    let len = session.transport.write_message(counter, plaintext, &mut out[HEADER_LEN..])?;
    out.truncate(HEADER_LEN + len);
    session.send_counter += 1;
    obfs.seal(&mut out)?;
    shared.transmits.push_back(Transmit { peer: id, datagram: out });
    Ok(())
}
