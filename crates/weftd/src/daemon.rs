use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, sleep_until};
use weft_ipc::{
    Connection, DeviceInfo, Diagnostics, Failure, InviteInfo, MemberStatus, NetworkStatus, PeerDiagnostics, PeerLink,
    PortMapping, Request, Response, Status,
};
use weft_mesh::{Mesh, Output, PeerConfig};
use weft_portmap::{Mapped, PortMapper};
use weft_proto::control::{
    Candidates, ClientKind, ClientMessage, DeviceList, Endpoint, ErrorCode, Invite, InviteCode, InviteRequest,
    MemberAction, NetworkCredentials, NetworkName, NetworkSettings, PeerCandidates, PeerKey, RelayPacket, Role,
    RoleChange, ServerKind, ServerMessage, State, Welcome,
};
use weft_proto::{DNS_ADDRESS, Link, PublicKey};
use weft_session::StaticKeypair;
use weft_tun::{DEFAULT_MTU, Dns, Route, Tun, TunConfig};

use crate::control::{Control, ControlEvent};
use crate::dns;
use crate::ipc::Command;
use crate::settings::{Settings, SettingsFile, default_nickname};

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const CANDIDATES_INTERVAL: Duration = Duration::from_secs(60);
const IDLE_TICK: Duration = Duration::from_secs(3600);

pub struct Options {
    pub tun_name: String,
    pub port: Option<u16>,
    pub echo: bool,
}

pub struct Daemon {
    keypair: Arc<StaticKeypair>,
    echo: bool,
    settings: Settings,
    settings_file: SettingsFile,
    tun_name: String,
    mesh: Mesh,
    udp: UdpSocket,
    udp_port: u16,
    mapper: PortMapper,
    mapped: watch::Receiver<Option<Mapped>>,
    tun: Option<Tun>,
    tun_address: Option<Ipv4Addr>,
    names: HashMap<String, Ipv4Addr>,
    control: Option<Control>,
    generation: u64,
    connection: Connection,
    welcome: Option<Welcome>,
    state: State,
    candidates: Vec<SocketAddr>,
    candidates_at: Option<Instant>,
    pending: HashMap<u32, oneshot::Sender<Response>>,
    up_waiters: Vec<oneshot::Sender<Response>>,
    redeem_waiters: Vec<(String, oneshot::Sender<Response>)>,
    next_id: u32,
    backoff: Duration,
    reconnect_at: Option<Instant>,
    control_tx: mpsc::UnboundedSender<(u64, ControlEvent)>,
    control_rx: mpsc::UnboundedReceiver<(u64, ControlEvent)>,
    commands: mpsc::Receiver<Command>,
}

enum Wake {
    Command(Option<Command>),
    Control(Option<(u64, ControlEvent)>),
    Udp(std::io::Result<(usize, SocketAddr)>),
    Tun(std::io::Result<usize>),
    Mapped,
    Timer,
}

impl Daemon {
    pub async fn new(
        keypair: StaticKeypair,
        settings_file: SettingsFile,
        options: Options,
        commands: mpsc::Receiver<Command>,
    ) -> std::io::Result<Self> {
        let settings = settings_file.load()?;
        let port = options.port.or(settings.port).unwrap_or(0);
        let udp = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], port))).await?;
        let udp_port = udp.local_addr()?.port();
        tracing::info!(port = udp_port, key = %keypair.public(), "weftd started");
        let mesh = Mesh::new(
            StaticKeypair::from_secret(keypair.secret()),
            std::time::Instant::now(),
            std::time::SystemTime::now(),
        )
        .map_err(std::io::Error::other)?;
        let mapper = PortMapper::spawn(udp_port);
        let mapped = mapper.subscribe();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let mut daemon = Self {
            keypair: Arc::new(keypair),
            echo: options.echo,
            settings,
            settings_file,
            tun_name: options.tun_name,
            mesh,
            udp,
            udp_port,
            mapper,
            mapped,
            tun: None,
            tun_address: None,
            names: HashMap::new(),
            control: None,
            generation: 0,
            connection: Connection::Disconnected,
            welcome: None,
            state: State::default(),
            candidates: Vec::new(),
            candidates_at: None,
            pending: HashMap::new(),
            up_waiters: Vec::new(),
            redeem_waiters: Vec::new(),
            next_id: 1,
            backoff: MIN_BACKOFF,
            reconnect_at: None,
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
                Ok(()) = self.mapped.changed() => Wake::Mapped,
                _ = sleep_until(deadline) => Wake::Timer,
            };
            let now = std::time::Instant::now();
            match wake {
                Wake::Command(None) => return,
                Wake::Command(Some(command)) => self.on_command(command),
                Wake::Control(Some((generation, event))) if generation == self.generation => {
                    self.on_control(event).await
                }
                Wake::Control(_) => {}
                Wake::Udp(Ok((len, from))) => {
                    if let Some(delivered) = self.mesh.receive_udp(now, from, &udp_buf[..len]) {
                        self.deliver(&delivered.packet).await;
                    }
                }
                Wake::Udp(Err(error)) => tracing::debug!(%error, "udp receive failed"),
                Wake::Tun(Ok(len)) if dns::is_query(&tun_buf[..len]) => {
                    if let (Some(reply), Some(tun)) = (dns::respond(&tun_buf[..len], &self.names), &self.tun) {
                        let _ = tun.send(&reply).await;
                    }
                }
                Wake::Tun(Ok(len)) => {
                    if let Err(error) = self.mesh.send(now, &tun_buf[..len]) {
                        tracing::trace!(%error, "packet not sent");
                    }
                }
                Wake::Tun(Err(error)) => {
                    tracing::warn!(%error, "tun receive failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Wake::Mapped => self.candidates_at = Some(Instant::now()),
                Wake::Timer => self.on_timer(),
            }
            self.flush().await;
        }
    }

    fn deadline(&self) -> Instant {
        let mesh = self.mesh.next_timeout().map(Instant::from_std);
        [mesh, self.reconnect_at, self.candidates_at]
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
            Request::Redeem { link } => self.redeem(link, reply),
            Request::CreateInvite { network, uses, expires_in } => {
                let request =
                    InviteRequest { network, max_uses: uses.unwrap_or(0), expires_in: expires_in.unwrap_or(0) };
                self.request(ClientKind::CreateInvite(request), reply)
            }
            Request::Invites { network } => self.request(ClientKind::ListInvites(NetworkName { name: network }), reply),
            Request::RevokeInvite { code } => self.request(ClientKind::RevokeInvite(InviteCode { code }), reply),
            Request::Kick { network, member } => {
                self.request(ClientKind::Kick(MemberAction { network, member }), reply)
            }
            Request::Ban { network, member } => self.request(ClientKind::Ban(MemberAction { network, member }), reply),
            Request::Unban { network, member } => {
                self.request(ClientKind::Unban(MemberAction { network, member }), reply)
            }
            Request::Bans { network } => self.request(ClientKind::ListBans(NetworkName { name: network }), reply),
            Request::Requests { network } => {
                self.request(ClientKind::ListRequests(NetworkName { name: network }), reply)
            }
            Request::Approve { network, member } => {
                self.request(ClientKind::Approve(MemberAction { network, member }), reply)
            }
            Request::Deny { network, member } => {
                self.request(ClientKind::Deny(MemberAction { network, member }), reply)
            }
            Request::SetRole { network, member, role } => {
                let role = match role {
                    weft_ipc::Role::Owner => Role::Owner,
                    weft_ipc::Role::Admin => Role::Admin,
                    weft_ipc::Role::Member => Role::Member,
                };
                self.request(ClientKind::SetRole(RoleChange { network, member, role: role as i32 }), reply)
            }
            Request::Configure { network, locked, approval, password } => self.request(
                ClientKind::UpdateNetwork(NetworkSettings { name: network, locked, approval, password }),
                reply,
            ),
            Request::Diagnose { logs } => {
                let _ = reply.send(Response::Diagnostics(Box::new(self.diagnostics(logs))));
            }
            Request::Delete { network } => {
                self.request(ClientKind::DeleteNetwork(NetworkName { name: network }), reply)
            }
        }
    }

    fn redeem(&mut self, link: String, reply: oneshot::Sender<Response>) {
        let Some((link, code)) = link.parse::<Link>().ok().and_then(|link| Some((link.server(), link.invite?))) else {
            let _ = reply.send(Response::Error(Failure::InvalidLink));
            return;
        };
        let server = link.to_string();
        if self.settings.server.as_ref() != Some(&server) || self.control.is_none() {
            self.settings.server = Some(server);
            self.settings.up = true;
            self.save_settings();
            self.disconnect();
            self.connect();
        } else if !self.settings.up {
            self.settings.up = true;
            self.save_settings();
        }
        if self.connection == Connection::Connected {
            self.request(ClientKind::RedeemInvite(InviteCode { code }), reply);
        } else {
            self.redeem_waiters.push((code, reply));
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
        if self.connection != Connection::Connected {
            let _ = reply.send(Response::Error(Failure::NotConnected));
            return;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        if self.send_control(ClientMessage { id, kind: Some(kind) }) {
            self.pending.insert(id, reply);
        } else {
            let _ = reply.send(Response::Error(Failure::NotConnected));
        }
    }

    fn send_control(&self, message: ClientMessage) -> bool {
        self.control.as_ref().is_some_and(|control| control.send(message))
    }

    fn connect(&mut self) {
        let Some(link) = self.server_link() else { return };
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
        self.welcome = None;
        self.reconnect_at = None;
        self.candidates_at = None;
        self.candidates.clear();
        self.state = State::default();
        self.mesh.clear_server();
        self.mesh.update_peers(std::time::Instant::now(), Vec::new());
        self.tun = None;
        self.tun_address = None;
        self.fail_pending(Failure::NotConnected);
    }

    fn server_link(&self) -> Option<Link> {
        self.settings.server.as_deref().and_then(|link| link.parse().ok())
    }

    fn fail_pending(&mut self, failure: Failure) {
        for (_, reply) in self.pending.drain() {
            let _ = reply.send(Response::Error(failure));
        }
        for reply in self.up_waiters.drain(..) {
            let _ = reply.send(Response::Error(failure));
        }
        for (_, reply) in self.redeem_waiters.drain(..) {
            let _ = reply.send(Response::Error(failure));
        }
    }

    async fn on_control(&mut self, event: ControlEvent) {
        match event {
            ControlEvent::Connected { welcome, server } => self.on_connected(welcome, server),
            ControlEvent::Refused(code) => {
                tracing::warn!(?code, "server refused the connection");
                self.control = None;
                self.connection = Connection::Disconnected;
                self.fail_pending(failure(code));
            }
            ControlEvent::Message(message) => self.on_message(message).await,
            ControlEvent::Closed(reason) => {
                tracing::warn!(%reason, "server connection closed");
                self.control = None;
                self.welcome = None;
                self.candidates_at = None;
                self.candidates.clear();
                self.mesh.clear_server();
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
        let prefix = welcome.prefix_len.min(32) as u8;
        self.mesh.set_local(address, prefix);
        if !self.echo && (self.tun.is_none() || self.tun_address != Some(address)) {
            self.tun = None;
            self.tun_address = None;
            let mut routes: Vec<Route> = self.settings.multicast_groups.iter().copied().map(Route::host).collect();
            if self.settings.broadcast {
                routes.insert(0, Route::host(Ipv4Addr::BROADCAST));
            }
            let dns = self.settings.dns.then(|| {
                routes.push(Route::host(DNS_ADDRESS));
                Dns { server: DNS_ADDRESS, domain: dns::ZONE.to_string() }
            });
            let config = TunConfig { name: self.tun_name.clone(), address, prefix, mtu: DEFAULT_MTU, routes, dns };
            match Tun::create(&config) {
                Ok(tun) => {
                    tracing::info!(name = tun.name(), %address, "tun interface is up");
                    self.tun = Some(tun);
                    self.tun_address = Some(address);
                }
                Err(error) => tracing::error!(%error, "cannot create tun interface"),
            }
        }
        self.update_names();
        if let (Some(link), Ok(token)) = (self.server_link(), welcome.discovery_token.as_slice().try_into()) {
            let udp = SocketAddr::new(server.ip(), welcome.udp_port as u16);
            self.mesh.set_server(std::time::Instant::now(), udp, link.server_key, token);
        }
        self.welcome = Some(welcome);
        self.candidates.clear();
        self.candidates_at = Some(Instant::now());
        for reply in self.up_waiters.drain(..) {
            let _ = reply.send(Response::Ok);
        }
        for (code, reply) in std::mem::take(&mut self.redeem_waiters) {
            self.request(ClientKind::RedeemInvite(InviteCode { code }), reply);
        }
    }

    async fn on_message(&mut self, message: ServerMessage) {
        if message.reply_to != 0
            && let Some(reply) = self.pending.remove(&message.reply_to)
        {
            let response = match message.kind {
                Some(ServerKind::Ack(_)) => Response::Ok,
                Some(ServerKind::Failure(f)) => Response::Error(failure(f.code())),
                Some(ServerKind::Joined(network)) => Response::Joined(network.name),
                Some(ServerKind::Invite(invite)) => Response::Invite(self.invite_info(invite)),
                Some(ServerKind::Invites(list)) => {
                    Response::Invites(list.invites.into_iter().map(|invite| self.invite_info(invite)).collect())
                }
                Some(ServerKind::Pending(network)) => Response::Pending(network.name),
                Some(ServerKind::Bans(list)) => Response::Bans(device_infos(list)),
                Some(ServerKind::Requests(list)) => Response::Requests(device_infos(list)),
                _ => Response::Error(Failure::Internal),
            };
            let _ = reply.send(response);
            return;
        }
        let now = std::time::Instant::now();
        match message.kind {
            Some(ServerKind::State(state)) => {
                self.state = state;
                self.update_names();
                let configs = peer_configs(&self.state);
                self.mesh.update_peers(now, configs);
            }
            Some(ServerKind::CallMeMaybe(PeerCandidates { key, endpoints })) => {
                if let Ok(key) = PublicKey::from_slice(&key) {
                    let candidates = endpoints.iter().filter_map(|e| e.to_socket_addr()).collect();
                    self.mesh.call_me_maybe(now, &key, candidates);
                }
            }
            Some(ServerKind::Relay(RelayPacket { address, packet })) => {
                if let Some(delivered) = self.mesh.receive_tcp_relay(now, Ipv4Addr::from(address), &packet) {
                    self.deliver(&delivered.packet).await;
                }
            }
            _ => {}
        }
    }

    fn on_timer(&mut self) {
        let now = Instant::now();
        self.mesh.tick(now.into_std());
        if self.reconnect_at.is_some_and(|at| now >= at) {
            self.connect();
        }
        if self.candidates_at.is_some_and(|at| now >= at) {
            self.candidates_at = Some(now + CANDIDATES_INTERVAL);
            self.publish_candidates();
        }
    }

    fn publish_candidates(&mut self) {
        if self.connection != Connection::Connected {
            return;
        }
        let tun = self.tun.as_ref().map(|tun| tun.name().to_string()).unwrap_or_else(|| self.tun_name.clone());
        let mut candidates: Vec<SocketAddr> = weft_portmap::local_addresses(Some(&tun))
            .into_iter()
            .filter(|ip| Some(*ip) != self.tun_address.map(IpAddr::V4))
            .map(|ip| SocketAddr::new(ip, self.udp_port))
            .collect();
        let observed = self.mesh.observed().map(|addr| addr.ip());
        if let Some(mapped) = self.mapper.current().and_then(|mapped| mapped.endpoint(observed)) {
            candidates.insert(0, mapped);
        }
        candidates.dedup();
        if candidates != self.candidates {
            tracing::debug!(?candidates, "publishing candidates");
            let endpoints = candidates.iter().map(|&addr| Endpoint::from(addr)).collect();
            let message = ClientMessage { id: 0, kind: Some(ClientKind::Candidates(Candidates { endpoints })) };
            if self.send_control(message) {
                self.candidates = candidates;
            }
        }
    }

    async fn flush(&mut self) {
        while let Some(output) = self.mesh.poll_output() {
            match output {
                Output::Udp { to, datagram } => {
                    if let Err(error) = self.udp.send_to(&datagram, to).await {
                        tracing::trace!(%to, %error, "udp send failed");
                    }
                }
                Output::TcpRelay { to, packet } => {
                    let relay = RelayPacket { address: u32::from(to), packet };
                    self.send_control(ClientMessage { id: 0, kind: Some(ClientKind::Relay(relay)) });
                }
                Output::CallMeMaybe { peer } => {
                    let key = PeerKey { key: peer.as_bytes().to_vec() };
                    self.send_control(ClientMessage { id: 0, kind: Some(ClientKind::CallMeMaybe(key)) });
                }
            }
        }
    }

    async fn deliver(&mut self, packet: &[u8]) {
        if self.echo {
            let own = self.welcome.as_ref().map(|welcome| Ipv4Addr::from(welcome.address));
            if let Some(reply) = own.and_then(|own| crate::echo::reply(packet, own)) {
                let _ = self.mesh.send(std::time::Instant::now(), &reply);
            }
            return;
        }
        if let Some(tun) = &self.tun
            && let Err(error) = tun.send(packet).await
        {
            tracing::debug!(%error, "tun send failed");
        }
    }

    fn status(&self) -> Status {
        let own = self.keypair.public();
        let peers: HashMap<&[u8], &weft_proto::control::Peer> =
            self.state.peers.iter().map(|peer| (peer.key.as_slice(), peer)).collect();
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
                locked: network.locked,
                approval: network.approval,
                requests: network.requests,
                members: network
                    .members
                    .iter()
                    .filter(|key| key.as_slice() != own.as_bytes())
                    .filter_map(|key| peers.get(key.as_slice()))
                    .map(|peer| self.member_status(peer))
                    .collect(),
            })
            .collect();
        Status {
            connection: self.connection,
            server: self.settings.server.clone(),
            nickname: self.settings.nickname.clone().unwrap_or_else(default_nickname),
            public_key: own.to_string(),
            address: self.welcome.as_ref().map(|welcome| Ipv4Addr::from(welcome.address)),
            networks,
        }
    }

    fn update_names(&mut self) {
        let own = self.keypair.public();
        let nickname = self.settings.nickname.clone().unwrap_or_else(default_nickname);
        let address = self.welcome.as_ref().map(|welcome| Ipv4Addr::from(welcome.address));
        let peers = self
            .state
            .peers
            .iter()
            .filter(|peer| peer.key.as_slice() != own.as_bytes())
            .map(|peer| (peer.nickname.as_str(), Ipv4Addr::from(peer.address)));
        self.names = dns::names(address.map(|address| (nickname.as_str(), address)).into_iter().chain(peers));
    }

    fn dns_name(&self, nickname: &str, address: Ipv4Addr) -> Option<String> {
        let label = dns::label(nickname)?;
        (self.names.get(&label) == Some(&address)).then(|| format!("{label}.{}", dns::ZONE))
    }

    fn diagnostics(&self, logs: bool) -> Diagnostics {
        let report = self.mesh.report(std::time::Instant::now());
        let tun = self.tun.as_ref().map(|tun| tun.name().to_string()).unwrap_or_else(|| self.tun_name.clone());
        let local_addresses: Vec<IpAddr> = weft_portmap::local_addresses(Some(&tun))
            .into_iter()
            .filter(|ip| Some(*ip) != self.tun_address.map(IpAddr::V4))
            .collect();
        let mapped = self.mapper.current();
        let port_mapping = mapped.map(|mapped| PortMapping {
            protocol: mapped.protocol.name().to_string(),
            external: mapped.endpoint(report.observed.map(|addr| addr.ip())),
        });
        let seen: Vec<SocketAddr> = report.peers.iter().filter_map(|peer| peer.observed).collect();
        let nat = weft_ipc::report::classify_nat(report.observed, self.udp_port, &local_addresses, &seen);
        let reports: HashMap<PublicKey, &weft_mesh::PeerReport> =
            report.peers.iter().map(|peer| (peer.key, peer)).collect();
        let own = self.keypair.public();
        let peers = self
            .state
            .peers
            .iter()
            .filter(|peer| peer.key.as_slice() != own.as_bytes())
            .map(|peer| {
                let status = self.member_status(peer);
                let report = PublicKey::from_slice(&peer.key).ok().and_then(|key| reports.get(&key).copied());
                let endpoint = match report.map(|report| report.link) {
                    Some(weft_mesh::PeerLink::Direct { addr, .. }) => Some(addr),
                    _ => None,
                };
                PeerDiagnostics {
                    nickname: status.nickname,
                    address: status.address,
                    link: status.link,
                    endpoint,
                    latency_ms: status.latency_ms,
                    candidates: peer.candidates.iter().filter_map(|endpoint| endpoint.to_socket_addr()).collect(),
                    observed: report.and_then(|report| report.observed),
                }
            })
            .collect();
        Diagnostics {
            version: env!("CARGO_PKG_VERSION").to_string(),
            os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            connection: self.connection,
            server: self.settings.server.clone(),
            server_udp: report.server_udp,
            observed: report.observed,
            local_port: self.udp_port,
            local_addresses,
            dns: self.settings.dns.then(|| self.tun.as_ref().map(Tun::dns_configured)).flatten(),
            port_mapping,
            nat,
            peers,
            logs: if logs { crate::logs::recent() } else { Vec::new() },
        }
    }

    fn member_status(&self, peer: &weft_proto::control::Peer) -> MemberStatus {
        let link = PublicKey::from_slice(&peer.key).ok().and_then(|key| self.mesh.link(&key));
        let (link, latency) = match link {
            _ if !peer.online => (PeerLink::Offline, None),
            None | Some(weft_mesh::PeerLink::Connecting) => (PeerLink::Connecting, None),
            Some(weft_mesh::PeerLink::Relay) => (PeerLink::Relay, None),
            Some(weft_mesh::PeerLink::Direct { latency, .. }) => (PeerLink::Direct, latency),
        };
        MemberStatus {
            dns: self.dns_name(&peer.nickname, Ipv4Addr::from(peer.address)),
            nickname: peer.nickname.clone(),
            address: Ipv4Addr::from(peer.address),
            link,
            latency_ms: latency.map(|latency| latency.as_millis().min(u128::from(u32::MAX)) as u32),
        }
    }

    fn invite_info(&self, invite: Invite) -> InviteInfo {
        let link = self.server_link().map(|link| Link { invite: Some(invite.code.clone()), ..link }.to_string());
        InviteInfo {
            code: invite.code,
            link,
            network: invite.network,
            max_uses: (invite.max_uses > 0).then_some(invite.max_uses),
            uses: invite.uses,
            expires: (invite.expires > 0).then_some(invite.expires),
            creator: invite.creator,
        }
    }

    fn save_settings(&self) {
        if let Err(error) = self.settings_file.save(&self.settings) {
            tracing::error!(path = %self.settings_file.path().display(), %error, "cannot save settings");
        }
    }
}

fn peer_configs(state: &State) -> Vec<PeerConfig> {
    state
        .peers
        .iter()
        .filter_map(|peer| {
            let key = PublicKey::from_slice(&peer.key).ok()?;
            let candidates = peer.endpoint.iter().chain(&peer.candidates).filter_map(|e| e.to_socket_addr()).collect();
            Some(PeerConfig { key, address: Ipv4Addr::from(peer.address), online: peer.online, candidates })
        })
        .collect()
}

async fn recv_tun(tun: Option<&Tun>, buf: &mut [u8]) -> std::io::Result<usize> {
    match tun {
        Some(tun) => tun.recv(buf).await,
        None => std::future::pending().await,
    }
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
        ErrorCode::Forbidden => Failure::Forbidden,
        ErrorCode::InviteNotFound => Failure::InviteNotFound,
        ErrorCode::Banned => Failure::Banned,
        ErrorCode::MemberNotFound => Failure::MemberNotFound,
        ErrorCode::AmbiguousMember => Failure::AmbiguousMember,
        ErrorCode::TooManyInvites => Failure::TooManyInvites,
        ErrorCode::NetworkLocked => Failure::NetworkLocked,
        ErrorCode::Unspecified | ErrorCode::Internal => Failure::Internal,
    }
}

fn device_infos(list: DeviceList) -> Vec<DeviceInfo> {
    list.devices
        .into_iter()
        .map(|device| DeviceInfo {
            nickname: device.nickname,
            address: Ipv4Addr::from(device.address),
            public_key: PublicKey::from_slice(&device.key).map(|key| key.to_string()).unwrap_or_default(),
        })
        .collect()
}
