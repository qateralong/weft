use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use rand::RngExt;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until};
use weft_ipc::{Connection, Failure, MemberStatus, NetworkStatus, PeerLink, Request, Response, Status};
use weft_proto::control::{
    ClientKind, ClientMessage, ErrorCode, NetworkCredentials, NetworkName, Role, ServerKind, ServerMessage, State,
    Welcome,
};
use weft_proto::obfs::discover_packet;
use weft_proto::{Link, ObfsKey, PublicKey};
use weft_session::{Event, Node, PeerId, StaticKeypair};
use weft_tun::{DEFAULT_MTU, Tun, TunConfig};

use crate::control::{Control, ControlEvent};
use crate::ipc::Command;
use crate::settings::{Settings, SettingsFile, default_nickname};

const DISCOVER_INTERVAL_MS: u64 = 20_000;
const DISCOVER_JITTER_MS: u64 = 5_000;
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const IDLE_TICK: Duration = Duration::from_secs(3600);

pub struct Daemon {
    keypair: Arc<StaticKeypair>,
    settings: Settings,
    settings_file: SettingsFile,
    tun_name: String,
    node: Node,
    udp: UdpSocket,
    tun: Option<Tun>,
    tun_address: Option<Ipv4Addr>,
    control: Option<Control>,
    generation: u64,
    connection: Connection,
    session: Option<ServerSession>,
    state: State,
    peers: HashMap<PublicKey, PeerInfo>,
    by_id: HashMap<PeerId, PublicKey>,
    by_address: HashMap<Ipv4Addr, PublicKey>,
    pending: HashMap<u32, oneshot::Sender<Response>>,
    up_waiters: Vec<oneshot::Sender<Response>>,
    next_id: u32,
    backoff: Duration,
    reconnect_at: Option<Instant>,
    discover_at: Option<Instant>,
    control_tx: mpsc::UnboundedSender<(u64, ControlEvent)>,
    control_rx: mpsc::UnboundedReceiver<(u64, ControlEvent)>,
    commands: mpsc::Receiver<Command>,
}

struct ServerSession {
    welcome: Welcome,
    udp: SocketAddr,
    obfs: ObfsKey,
}

struct PeerInfo {
    id: PeerId,
    online: bool,
    address: Ipv4Addr,
    endpoint: Option<SocketAddr>,
    announced: Option<SocketAddr>,
}

enum Wake {
    Command(Option<Command>),
    Control(Option<(u64, ControlEvent)>),
    Udp(std::io::Result<(usize, SocketAddr)>),
    Tun(std::io::Result<usize>),
    Timer,
}

impl Daemon {
    pub async fn new(
        keypair: StaticKeypair,
        settings_file: SettingsFile,
        tun_name: String,
        port: Option<u16>,
        commands: mpsc::Receiver<Command>,
    ) -> std::io::Result<Self> {
        let settings = settings_file.load()?;
        let port = port.or(settings.port).unwrap_or(0);
        let udp = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], port))).await?;
        tracing::info!(addr = %udp.local_addr()?, key = %keypair.public(), "weftd started");
        let now = std::time::Instant::now();
        let node = Node::new(StaticKeypair::from_secret(keypair.secret()), now, std::time::SystemTime::now())
            .map_err(std::io::Error::other)?;
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let mut daemon = Self {
            keypair: Arc::new(keypair),
            settings,
            settings_file,
            tun_name,
            node,
            udp,
            tun: None,
            tun_address: None,
            control: None,
            generation: 0,
            connection: Connection::Disconnected,
            session: None,
            state: State::default(),
            peers: HashMap::new(),
            by_id: HashMap::new(),
            by_address: HashMap::new(),
            pending: HashMap::new(),
            up_waiters: Vec::new(),
            next_id: 1,
            backoff: MIN_BACKOFF,
            reconnect_at: None,
            discover_at: None,
            control_tx,
            control_rx,
            commands,
        };
        if daemon.settings.up {
            daemon.connect();
        }
        Ok(daemon)
    }

    pub async fn run(mut self) {
        let mut udp_buf = vec![0; 65_535];
        let mut tun_buf = vec![0; 65_535];
        loop {
            let deadline = self.deadline();
            let wake = tokio::select! {
                command = self.commands.recv() => Wake::Command(command),
                event = self.control_rx.recv() => Wake::Control(event),
                received = self.udp.recv_from(&mut udp_buf) => Wake::Udp(received),
                received = recv_tun(self.tun.as_ref(), &mut tun_buf) => Wake::Tun(received),
                _ = sleep_until(deadline) => Wake::Timer,
            };
            match wake {
                Wake::Command(None) => return,
                Wake::Command(Some(command)) => self.on_command(command),
                Wake::Control(Some((generation, event))) if generation == self.generation => self.on_control(event),
                Wake::Control(_) => {}
                Wake::Udp(Ok((len, from))) => self.on_udp(&udp_buf[..len], from).await,
                Wake::Udp(Err(error)) => tracing::debug!(%error, "udp receive failed"),
                Wake::Tun(Ok(len)) => self.on_tun(&tun_buf[..len]),
                Wake::Tun(Err(error)) => {
                    tracing::warn!(%error, "tun receive failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Wake::Timer => self.on_timer().await,
            }
            self.flush().await;
        }
    }

    fn deadline(&self) -> Instant {
        let node = self.node.next_timeout().map(Instant::from_std);
        [node, self.reconnect_at, self.discover_at]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or_else(|| Instant::now() + IDLE_TICK)
    }

    fn on_command(&mut self, command: Command) {
        let Command { request, reply } = command;
        match request {
            Request::Up { link, nickname } => self.up(link, nickname, reply),
            Request::Down => {
                self.settings.up = false;
                self.save_settings();
                self.disconnect();
                let _ = reply.send(Response::Ok);
            }
            Request::Status => {
                let _ = reply.send(Response::Status(self.status()));
            }
            Request::Create { name, password } => {
                self.request(ClientKind::CreateNetwork(NetworkCredentials { name, password }), reply)
            }
            Request::Join { name, password } => {
                self.request(ClientKind::JoinNetwork(NetworkCredentials { name, password }), reply)
            }
            Request::Leave { name } => self.request(ClientKind::LeaveNetwork(NetworkName { name }), reply),
        }
    }

    fn up(&mut self, link: Option<String>, nickname: Option<String>, reply: oneshot::Sender<Response>) {
        let mut changed = false;
        if let Some(link) = link {
            let Ok(link) = link.parse::<Link>() else {
                let _ = reply.send(Response::Error(Failure::InvalidLink));
                return;
            };
            let server = link.server().to_string();
            changed |= self.settings.server.as_ref() != Some(&server);
            self.settings.server = Some(server);
        }
        if let Some(nickname) = nickname {
            let nickname = nickname.trim().to_string();
            if !(1..=32).contains(&nickname.chars().count()) {
                let _ = reply.send(Response::Error(Failure::InvalidNickname));
                return;
            }
            changed |= self.settings.nickname.as_ref() != Some(&nickname);
            self.settings.nickname = Some(nickname);
        }
        if self.settings.server.is_none() {
            let _ = reply.send(Response::Error(Failure::NoServer));
            return;
        }
        self.settings.up = true;
        self.save_settings();
        if self.connection == Connection::Connected && !changed {
            let _ = reply.send(Response::Ok);
            return;
        }
        if changed || self.control.is_none() {
            self.disconnect();
            self.connect();
        }
        self.up_waiters.push(reply);
    }

    fn request(&mut self, kind: ClientKind, reply: oneshot::Sender<Response>) {
        let Some(control) = self.control.as_ref().filter(|_| self.connection == Connection::Connected) else {
            let _ = reply.send(Response::Error(Failure::NotConnected));
            return;
        };
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        if control.send(ClientMessage { id, kind: Some(kind) }) {
            self.pending.insert(id, reply);
        } else {
            let _ = reply.send(Response::Error(Failure::NotConnected));
        }
    }

    fn connect(&mut self) {
        let Some(link) = self.settings.server.as_deref().and_then(|link| link.parse::<Link>().ok()) else {
            return;
        };
        let nickname = self.settings.nickname.clone().unwrap_or_else(default_nickname);
        self.generation += 1;
        self.control =
            Some(Control::spawn(link, self.keypair.clone(), nickname, self.generation, self.control_tx.clone()));
        self.connection = Connection::Connecting;
        self.reconnect_at = None;
    }

    fn disconnect(&mut self) {
        self.generation += 1;
        self.control = None;
        self.connection = Connection::Disconnected;
        self.session = None;
        self.reconnect_at = None;
        self.discover_at = None;
        self.state = State::default();
        self.sync_peers();
        self.tun = None;
        self.tun_address = None;
        self.fail_pending(Failure::NotConnected);
    }

    fn fail_pending(&mut self, failure: Failure) {
        for (_, reply) in self.pending.drain() {
            let _ = reply.send(Response::Error(failure));
        }
        for reply in self.up_waiters.drain(..) {
            let _ = reply.send(Response::Error(failure));
        }
    }

    fn on_control(&mut self, event: ControlEvent) {
        match event {
            ControlEvent::Connected { welcome, server } => self.on_connected(welcome, server),
            ControlEvent::Refused(code) => {
                tracing::warn!(?code, "server refused the connection");
                self.control = None;
                self.connection = Connection::Disconnected;
                self.fail_pending(failure(code));
            }
            ControlEvent::Message(message) => self.on_message(message),
            ControlEvent::Closed(reason) => {
                tracing::warn!(%reason, "server connection closed");
                self.control = None;
                self.session = None;
                self.discover_at = None;
                self.fail_pending(Failure::NotConnected);
                if self.settings.up {
                    self.connection = Connection::Connecting;
                    self.reconnect_at = Some(Instant::now() + self.backoff);
                    self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
                } else {
                    self.connection = Connection::Disconnected;
                }
            }
        }
    }

    fn on_connected(&mut self, welcome: Welcome, server: SocketAddr) {
        let address = Ipv4Addr::from(welcome.address);
        tracing::info!(%server, %address, "connected to the server");
        self.connection = Connection::Connected;
        self.backoff = MIN_BACKOFF;
        let config = TunConfig {
            name: self.tun_name.clone(),
            address,
            prefix: welcome.prefix_len.min(32) as u8,
            mtu: DEFAULT_MTU,
        };
        if self.tun.is_none() || self.tun_address != Some(address) {
            self.tun = None;
            self.tun_address = None;
            match Tun::create(&config) {
                Ok(tun) => {
                    tracing::info!(name = tun.name(), %address, "tun interface is up");
                    self.tun = Some(tun);
                    self.tun_address = Some(address);
                }
                Err(error) => tracing::error!(%error, "cannot create tun interface"),
            }
        }
        let server_key =
            self.settings.server.as_deref().and_then(|link| link.parse::<Link>().ok()).map(|l| l.server_key);
        self.session = server_key.map(|key| ServerSession {
            udp: SocketAddr::new(server.ip(), welcome.udp_port as u16),
            obfs: ObfsKey::for_receiver(&key),
            welcome,
        });
        self.discover_at = Some(Instant::now());
        for reply in self.up_waiters.drain(..) {
            let _ = reply.send(Response::Ok);
        }
    }

    fn on_message(&mut self, message: ServerMessage) {
        if message.reply_to != 0
            && let Some(reply) = self.pending.remove(&message.reply_to)
        {
            let response = match message.kind {
                Some(ServerKind::Ack(_)) => Response::Ok,
                Some(ServerKind::Failure(f)) => Response::Error(failure(f.code())),
                _ => Response::Error(Failure::Internal),
            };
            let _ = reply.send(response);
            return;
        }
        if let Some(ServerKind::State(state)) = message.kind {
            self.state = state;
            self.sync_peers();
        }
    }

    fn sync_peers(&mut self) {
        let now = std::time::Instant::now();
        let own = self.keypair.public();
        let listed: HashMap<PublicKey, (bool, Ipv4Addr, Option<SocketAddr>)> = self
            .state
            .peers
            .iter()
            .filter_map(|peer| {
                let key = PublicKey::from_slice(&peer.key).ok().filter(|key| *key != own)?;
                let endpoint = peer.endpoint.as_ref().and_then(|e| e.to_socket_addr());
                Some((key, (peer.online, Ipv4Addr::from(peer.address), endpoint)))
            })
            .collect();

        let gone: Vec<PublicKey> = self
            .peers
            .iter()
            .filter(|(key, peer)| match listed.get(key) {
                None => true,
                Some(&(online, _, _)) => !online && !self.node.is_established(peer.id),
            })
            .map(|(key, _)| *key)
            .collect();
        for key in gone {
            self.remove_peer(&key);
        }

        for (key, (online, address, announced)) in listed {
            if !online && !self.peers.contains_key(&key) {
                continue;
            }
            let peer = self.peers.entry(key).or_insert_with(|| {
                let id = self.node.add_peer(now, key);
                self.by_id.insert(id, key);
                PeerInfo { id, online, address, endpoint: None, announced: None }
            });
            peer.online = online;
            if peer.address != address {
                self.by_address.remove(&peer.address);
                peer.address = address;
            }
            self.by_address.insert(address, key);
            if announced.is_some() && peer.announced != announced {
                let first = peer.endpoint.is_none();
                peer.announced = announced;
                peer.endpoint = announced;
                if first {
                    self.node.reconnect(now, peer.id);
                }
            }
        }
    }

    fn remove_peer(&mut self, key: &PublicKey) {
        if let Some(peer) = self.peers.remove(key) {
            self.node.remove_peer(peer.id);
            self.by_id.remove(&peer.id);
            if self.by_address.get(&peer.address) == Some(key) {
                self.by_address.remove(&peer.address);
            }
        }
    }

    async fn on_udp(&mut self, datagram: &[u8], from: SocketAddr) {
        if self.session.as_ref().is_some_and(|session| session.udp == from) {
            return;
        }
        let received = match self.node.receive(std::time::Instant::now(), datagram) {
            Ok(received) => received,
            Err(error) => {
                tracing::trace!(%from, %error, "dropped datagram");
                return;
            }
        };
        let Some(key) = self.by_id.get(&received.peer) else { return };
        let Some(peer) = self.peers.get_mut(key) else { return };
        peer.endpoint = Some(from);
        let Some(packet) = received.packet else { return };
        if ipv4_source(&packet) != Some(peer.address) {
            tracing::debug!(peer = %peer.address, "dropped packet with a foreign source address");
            return;
        }
        if let Some(tun) = &self.tun
            && let Err(error) = tun.send(&packet).await
        {
            tracing::debug!(%error, "tun send failed");
        }
    }

    fn on_tun(&mut self, packet: &[u8]) {
        let Some(destination) = ipv4_destination(packet) else { return };
        let Some(peer) = self.by_address.get(&destination).and_then(|key| self.peers.get(key)) else { return };
        if let Err(error) = self.node.send(std::time::Instant::now(), peer.id, packet) {
            tracing::debug!(%error, "cannot send packet");
        }
    }

    async fn on_timer(&mut self) {
        let now = Instant::now();
        self.node.tick(now.into_std());
        if self.reconnect_at.is_some_and(|at| now >= at) {
            self.connect();
        }
        if self.discover_at.is_some_and(|at| now >= at) {
            self.discover().await;
            let jitter = rand::rng().random_range(0..=2 * DISCOVER_JITTER_MS);
            self.discover_at = Some(now + Duration::from_millis(DISCOVER_INTERVAL_MS - DISCOVER_JITTER_MS + jitter));
        }
    }

    async fn discover(&self) {
        let Some(session) = &self.session else { return };
        let Ok(token) = <[u8; 16]>::try_from(session.welcome.discovery_token.as_slice()) else { return };
        let mut rng = rand::rng();
        let mut packet = discover_packet(&token, rng.random_range(0..=64), rng.random());
        if session.obfs.seal(&mut packet).is_ok()
            && let Err(error) = self.udp.send_to(&packet, session.udp).await
        {
            tracing::debug!(%error, "discover send failed");
        }
    }

    async fn flush(&mut self) {
        while let Some(event) = self.node.poll_event() {
            match event {
                Event::Established(id) => tracing::debug!(?id, "session established"),
                Event::Unreachable(id) => {
                    tracing::info!(?id, "peer is unreachable");
                    let offline = self.by_id.get(&id).filter(|key| self.peers.get(key).is_some_and(|p| !p.online));
                    if let Some(key) = offline.copied() {
                        self.remove_peer(&key);
                    }
                }
            }
        }
        while let Some(transmit) = self.node.poll_transmit() {
            let endpoint = self.by_id.get(&transmit.peer).and_then(|key| self.peers.get(key)).and_then(|p| p.endpoint);
            if let Some(endpoint) = endpoint
                && let Err(error) = self.udp.send_to(&transmit.datagram, endpoint).await
            {
                tracing::debug!(%endpoint, %error, "udp send failed");
            }
        }
    }

    fn status(&self) -> Status {
        let own = self.keypair.public();
        let peers: HashMap<Vec<u8>, &weft_proto::control::Peer> =
            self.state.peers.iter().map(|peer| (peer.key.clone(), peer)).collect();
        let networks = self
            .state
            .networks
            .iter()
            .map(|network| NetworkStatus {
                name: network.name.clone(),
                role: match network.role() {
                    Role::Owner => weft_ipc::Role::Owner,
                    Role::Admin => weft_ipc::Role::Admin,
                    Role::Member => weft_ipc::Role::Member,
                },
                members: network
                    .members
                    .iter()
                    .filter(|key| key.as_slice() != own.as_bytes())
                    .filter_map(|key| peers.get(key))
                    .map(|peer| MemberStatus {
                        nickname: peer.nickname.clone(),
                        address: Ipv4Addr::from(peer.address),
                        link: self.link_state(peer),
                    })
                    .collect(),
            })
            .collect();
        Status {
            connection: self.connection,
            server: self.settings.server.clone(),
            nickname: self.settings.nickname.clone().unwrap_or_else(default_nickname),
            public_key: own.to_string(),
            address: self.session.as_ref().map(|s| Ipv4Addr::from(s.welcome.address)),
            networks,
        }
    }

    fn link_state(&self, peer: &weft_proto::control::Peer) -> PeerLink {
        if !peer.online {
            return PeerLink::Offline;
        }
        let established = PublicKey::from_slice(&peer.key)
            .ok()
            .and_then(|key| self.peers.get(&key))
            .is_some_and(|info| self.node.is_established(info.id));
        if established { PeerLink::Direct } else { PeerLink::Connecting }
    }

    fn save_settings(&self) {
        if let Err(error) = self.settings_file.save(&self.settings) {
            tracing::error!(path = %self.settings_file.path().display(), %error, "cannot save settings");
        }
    }
}

async fn recv_tun(tun: Option<&Tun>, buf: &mut [u8]) -> std::io::Result<usize> {
    match tun {
        Some(tun) => tun.recv(buf).await,
        None => std::future::pending().await,
    }
}

fn ipv4_destination(packet: &[u8]) -> Option<Ipv4Addr> {
    (packet.len() >= 20 && packet[0] >> 4 == 4).then(|| Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]))
}

fn ipv4_source(packet: &[u8]) -> Option<Ipv4Addr> {
    (packet.len() >= 20 && packet[0] >> 4 == 4).then(|| Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]))
}

fn failure(code: ErrorCode) -> Failure {
    match code {
        ErrorCode::UnsupportedVersion => Failure::UnsupportedVersion,
        ErrorCode::InvalidName => Failure::InvalidName,
        ErrorCode::InvalidNickname => Failure::InvalidNickname,
        ErrorCode::InvalidPassword => Failure::InvalidPassword,
        ErrorCode::NetworkExists => Failure::NetworkExists,
        ErrorCode::NetworkNotFound => Failure::NetworkNotFound,
        ErrorCode::WrongPassword => Failure::WrongPassword,
        ErrorCode::NetworkFull => Failure::NetworkFull,
        ErrorCode::RateLimited => Failure::RateLimited,
        ErrorCode::AlreadyMember => Failure::AlreadyMember,
        ErrorCode::NotMember => Failure::NotMember,
        ErrorCode::PoolExhausted => Failure::PoolExhausted,
        ErrorCode::InvalidRequest => Failure::InvalidRequest,
        ErrorCode::Unspecified | ErrorCode::Internal => Failure::Internal,
    }
}
