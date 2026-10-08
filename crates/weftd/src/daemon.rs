use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, sleep_until};
use weft_ipc::{
    Connection, DeviceInfo, Diagnostics, Failure, HostStatus, InviteInfo, MemberStatus, NetworkStatus, PeerDiagnostics,
    PeerLink, PortMapping, Request, Response, ServerDiagnostics, ServerStatus, Status,
};
use weft_mesh::{Mesh, Output, PeerConfig, ServerId, SocketId};
use weft_portmap::{Mapped, PortMapper};
use weft_proto::control::{
    Candidates, ClientKind, ClientMessage, DeviceList, Endpoint, ErrorCode, Hello, Invite, InviteCode, InviteRequest,
    MemberAction, NetworkCredentials, NetworkName, NetworkSettings, PROTOCOL_VERSION, PeerCandidates, PeerKey,
    RelayPacket, Role, RoleChange, ServerKind, ServerMessage, State, Welcome,
};
use weft_proto::{DNS_ADDRESS, Host, Link, PublicKey};
use weft_session::StaticKeypair;
use weft_tun::{DEFAULT_MTU, Dns, Route, Tun, TunConfig};

use crate::control::{Control, ControlEvent};
use crate::dns;
use crate::host::Hosted;
use crate::ipc::Command;
use crate::settings::{ServerEntry, Settings, SettingsFile, Transport, default_nickname, public_server};

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const CANDIDATES_INTERVAL: Duration = Duration::from_secs(60);
const IDLE_TICK: Duration = Duration::from_secs(3600);
const DEFAULT_TTL: u32 = 64;

pub struct Options {
    pub tun_name: String,
    pub port: Option<u16>,
    pub echo: bool,
    pub state_dir: std::path::PathBuf,
}

/// One coordination server and the connection to it.
struct Session {
    id: ServerId,
    link: Link,
    up: bool,
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
    backoff: Duration,
    reconnect_at: Option<Instant>,
}

impl Session {
    fn new(id: ServerId, link: Link, up: bool) -> Self {
        Self {
            id,
            link,
            up,
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
            backoff: MIN_BACKOFF,
            reconnect_at: None,
        }
    }

    fn host(&self) -> String {
        link_host(&self.link)
    }

    /// How well `selector` names this server: 2 for its link or `host:port`, 1 for the host alone.
    fn matches(&self, selector: &str) -> u8 {
        link_matches(&self.link, selector)
    }

    fn has_network(&self, name: &str) -> bool {
        let name = name.trim().to_lowercase();
        self.state.networks.iter().any(|network| network.name.to_lowercase() == name)
    }

    fn address(&self) -> Option<Ipv4Addr> {
        self.welcome.as_ref().map(|welcome| Ipv4Addr::from(welcome.address))
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
}

pub struct Daemon {
    keypair: Arc<StaticKeypair>,
    state_dir: std::path::PathBuf,
    hosted: Option<Hosted>,
    echo: bool,
    settings: Settings,
    settings_file: SettingsFile,
    tun_name: String,
    mesh: Mesh,
    udp: UdpSocket,
    udp_port: u16,
    /// IPv6 counterpart of the main socket; peers reach it without NAT.
    udp6: Option<UdpSocket>,
    udp6_port: u16,
    extra: HashMap<SocketId, ExtraSocket>,
    extra_tx: mpsc::Sender<(SocketId, SocketAddr, Vec<u8>)>,
    extra_rx: mpsc::Receiver<(SocketId, SocketAddr, Vec<u8>)>,
    mapper: PortMapper,
    mapped: watch::Receiver<Option<Mapped>>,
    tun: Option<Tun>,
    /// Our virtual addresses with prefix lengths; the first one created the TUN interface.
    addresses: Vec<(Ipv4Addr, u8)>,
    names: HashMap<String, Ipv4Addr>,
    sessions: Vec<Session>,
    next_server: ServerId,
    generation: u64,
    next_id: u32,
    control_tx: mpsc::UnboundedSender<(u64, ControlEvent)>,
    control_rx: mpsc::UnboundedReceiver<(u64, ControlEvent)>,
    commands: mpsc::Receiver<Command>,
}

/// A short-lived socket used while punching through a symmetric NAT.
struct ExtraSocket {
    socket: Arc<UdpSocket>,
    ttl: u32,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ExtraSocket {
    fn drop(&mut self) {
        self.task.abort();
    }
}

enum Wake {
    Command(Option<Command>),
    Control(Option<(u64, ControlEvent)>),
    Udp(std::io::Result<(usize, SocketAddr)>),
    Udp6(std::io::Result<(usize, SocketAddr)>),
    Extra(Option<(SocketId, SocketAddr, Vec<u8>)>),
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
        let udp6 = bind_v6(udp_port).or_else(|_| bind_v6(0));
        if let Err(error) = &udp6 {
            tracing::info!(%error, "no ipv6 socket");
        }
        let udp6 = udp6.ok();
        let udp6_port = udp6.as_ref().and_then(|socket| socket.local_addr().ok()).map_or(0, |addr| addr.port());
        tracing::info!(port = udp_port, port6 = udp6_port, key = %keypair.public(), "weftd started");
        let mut mesh = Mesh::new(
            StaticKeypair::from_secret(keypair.secret()),
            std::time::Instant::now(),
            std::time::SystemTime::now(),
        )
        .map_err(std::io::Error::other)?;
        mesh.set_udp_port(udp_port);
        let mapper = PortMapper::spawn(udp_port);
        let (extra_tx, extra_rx) = mpsc::channel(1024);
        let mapped = mapper.subscribe();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let mut daemon = Self {
            keypair: Arc::new(keypair),
            state_dir: options.state_dir,
            hosted: None,
            echo: options.echo,
            settings,
            settings_file,
            tun_name: options.tun_name,
            mesh,
            udp,
            udp_port,
            udp6,
            udp6_port,
            extra: HashMap::new(),
            extra_tx,
            extra_rx,
            mapper,
            mapped,
            tun: None,
            addresses: Vec::new(),
            names: HashMap::new(),
            sessions: Vec::new(),
            next_server: 1,
            generation: 0,
            next_id: 1,
            control_tx,
            control_rx,
            commands,
        };
        for entry in daemon.settings.servers.clone() {
            match entry.link.parse::<Link>() {
                Ok(link) => {
                    let host = Session::new(0, link.server(), false).host();
                    if let Some(index) = daemon.sessions.iter().position(|session| session.host() == host) {
                        daemon.sessions.remove(index);
                    }
                    daemon.add_session(link.server(), entry.up);
                }
                Err(error) => tracing::warn!(link = entry.link, %error, "skipping an invalid server link"),
            }
        }
        for index in 0..daemon.sessions.len() {
            if daemon.sessions[index].up {
                daemon.connect(index);
            }
        }
        if daemon.settings.host.enabled
            && let Err(error) = daemon.start_hosting().await
        {
            tracing::error!(%error, "cannot start the hosted server");
        }
        Ok(daemon)
    }

    pub async fn run(mut self) {
        let mut udp_buf = vec![0; 65_535];
        let mut udp6_buf = vec![0; 65_535];
        let mut tun_buf = vec![0; 65_535];
        loop {
            let deadline = self.deadline();
            let wake = tokio::select! {
                command = self.commands.recv() => Wake::Command(command),
                event = self.control_rx.recv() => Wake::Control(event),
                received = self.udp.recv_from(&mut udp_buf) => Wake::Udp(received),
                received = recv_v6(self.udp6.as_ref(), &mut udp6_buf) => Wake::Udp6(received),
                received = self.extra_rx.recv() => Wake::Extra(received),
                received = recv_tun(self.tun.as_ref(), &mut tun_buf) => Wake::Tun(received),
                Ok(()) = self.mapped.changed() => Wake::Mapped,
                _ = sleep_until(deadline) => Wake::Timer,
            };
            let now = std::time::Instant::now();
            match wake {
                Wake::Command(None) => return,
                Wake::Command(Some(command)) => self.on_command(command).await,
                Wake::Control(Some((generation, event))) => {
                    let found = self.sessions.iter().position(|session| session.generation == generation);
                    if let Some(index) = found {
                        self.on_control(index, event).await;
                    }
                }
                Wake::Control(None) => {}
                Wake::Udp(Ok((len, from))) => {
                    if let Some(delivered) = self.mesh.receive_udp(now, 0, from, &udp_buf[..len]) {
                        self.deliver(&delivered.packet).await;
                    }
                }
                Wake::Udp(Err(error)) => tracing::debug!(%error, "udp receive failed"),
                Wake::Udp6(Ok((len, from))) => {
                    if let Some(delivered) = self.mesh.receive_udp(now, 0, from, &udp6_buf[..len]) {
                        self.deliver(&delivered.packet).await;
                    }
                }
                Wake::Udp6(Err(error)) => tracing::debug!(%error, "udp6 receive failed"),
                Wake::Extra(Some((socket, from, datagram))) => {
                    if let Some(delivered) = self.mesh.receive_udp(now, socket, from, &datagram) {
                        self.deliver(&delivered.packet).await;
                    }
                }
                Wake::Extra(None) => {}
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
                Wake::Mapped => {
                    for session in &mut self.sessions {
                        if session.connection == Connection::Connected {
                            session.candidates_at = Some(Instant::now());
                        }
                    }
                }
                Wake::Timer => self.on_timer(),
            }
            self.flush().await;
        }
    }

    fn deadline(&self) -> Instant {
        let mesh = self.mesh.next_timeout().map(Instant::from_std);
        let sessions = self.sessions.iter().flat_map(|session| [session.reconnect_at, session.candidates_at]);
        [mesh].into_iter().chain(sessions).flatten().min().unwrap_or_else(|| Instant::now() + IDLE_TICK)
    }

    async fn on_command(&mut self, command: Command) {
        let Command { request, server, reply } = command;
        let server = server.as_deref();
        match request {
            Request::Up { link, nickname } => self.up(link, nickname, server, reply),
            Request::Down => self.down(server, reply),
            Request::Remove => self.remove(server, reply),
            Request::Host { enabled, port, address } => self.host(enabled, port, address, reply).await,
            Request::Status => {
                let _ = reply.send(Response::Status(self.status()));
            }
            Request::Diagnose { logs } => {
                let _ = reply.send(Response::Diagnostics(Box::new(self.diagnostics(logs))));
            }
            Request::Redeem { link } => self.redeem(link, reply),
            Request::Create { name, password } => {
                let target = self.target(server, None);
                self.route(target, ClientKind::CreateNetwork(NetworkCredentials { name, password }), reply)
            }
            Request::Join { name, password } => {
                let target = self.target(server, Some(&name));
                self.route(target, ClientKind::JoinNetwork(NetworkCredentials { name, password }), reply)
            }
            Request::Leave { name } => {
                let target = self.target(server, Some(&name));
                self.route(target, ClientKind::LeaveNetwork(NetworkName { name }), reply)
            }
            Request::RevokeInvite { code } => {
                let target = self.target(server, None);
                self.route(target, ClientKind::RevokeInvite(InviteCode { code }), reply)
            }
            Request::CreateInvite { network, uses, expires_in } => {
                let target = self.target(server, Some(&network));
                let request =
                    InviteRequest { network, max_uses: uses.unwrap_or(0), expires_in: expires_in.unwrap_or(0) };
                self.route(target, ClientKind::CreateInvite(request), reply)
            }
            Request::Invites { network } => {
                let target = self.target(server, Some(&network));
                self.route(target, ClientKind::ListInvites(NetworkName { name: network }), reply)
            }
            Request::Bans { network } => {
                let target = self.target(server, Some(&network));
                self.route(target, ClientKind::ListBans(NetworkName { name: network }), reply)
            }
            Request::Requests { network } => {
                let target = self.target(server, Some(&network));
                self.route(target, ClientKind::ListRequests(NetworkName { name: network }), reply)
            }
            Request::Delete { network } => {
                let target = self.target(server, Some(&network));
                self.route(target, ClientKind::DeleteNetwork(NetworkName { name: network }), reply)
            }
            Request::Kick { network, member } => {
                let target = self.target(server, Some(&network));
                self.route(target, ClientKind::Kick(MemberAction { network, member }), reply)
            }
            Request::Ban { network, member } => {
                let target = self.target(server, Some(&network));
                self.route(target, ClientKind::Ban(MemberAction { network, member }), reply)
            }
            Request::Unban { network, member } => {
                let target = self.target(server, Some(&network));
                self.route(target, ClientKind::Unban(MemberAction { network, member }), reply)
            }
            Request::Approve { network, member } => {
                let target = self.target(server, Some(&network));
                self.route(target, ClientKind::Approve(MemberAction { network, member }), reply)
            }
            Request::Deny { network, member } => {
                let target = self.target(server, Some(&network));
                self.route(target, ClientKind::Deny(MemberAction { network, member }), reply)
            }
            Request::SetRole { network, member, role } => {
                let target = self.target(server, Some(&network));
                let role = match role {
                    weft_ipc::Role::Owner => Role::Owner,
                    weft_ipc::Role::Admin => Role::Admin,
                    weft_ipc::Role::Member => Role::Member,
                };
                self.route(target, ClientKind::SetRole(RoleChange { network, member, role: role as i32 }), reply)
            }
            Request::Configure { network, locked, approval, password } => {
                let target = self.target(server, Some(&network));
                let settings = NetworkSettings { name: network, locked, approval, password };
                self.route(target, ClientKind::UpdateNetwork(settings), reply)
            }
        }
    }

    /// Picks the server for a request: the selected one, the one that has the network, or the
    /// only one there is.
    fn target(&self, server: Option<&str>, network: Option<&str>) -> Result<usize, Failure> {
        if let Some(selector) = server {
            let score = |i: usize| {
                let public = if self.is_hosted(i) { link_matches(&self.public_link(i), selector) } else { 0 };
                self.sessions[i].matches(selector).max(public)
            };
            let best = (0..self.sessions.len()).map(score).max().unwrap_or(0);
            let found: Vec<usize> = (0..self.sessions.len()).filter(|&i| best > 0 && score(i) == best).collect();
            return match found[..] {
                [index] => Ok(index),
                [] => Err(Failure::ServerNotFound),
                _ => Err(Failure::AmbiguousServer),
            };
        }
        if let Some(name) = network {
            let found: Vec<usize> = (0..self.sessions.len()).filter(|&i| self.sessions[i].has_network(name)).collect();
            match found[..] {
                [index] => return Ok(index),
                [] => {}
                _ => return Err(Failure::AmbiguousNetwork),
            }
        }
        if self.sessions.len() == 1 {
            return Ok(0);
        }
        let connected: Vec<usize> =
            (0..self.sessions.len()).filter(|&i| self.sessions[i].connection == Connection::Connected).collect();
        match connected[..] {
            [index] => Ok(index),
            _ if self.sessions.is_empty() => Err(Failure::NoServer),
            _ => Err(Failure::AmbiguousServer),
        }
    }

    fn route(&mut self, target: Result<usize, Failure>, kind: ClientKind, reply: oneshot::Sender<Response>) {
        match target {
            Ok(index) => self.request(index, kind, reply),
            Err(failure) => {
                let _ = reply.send(Response::Error(failure));
            }
        }
    }

    /// The session for this server; a new link for the same host and port replaces the old one,
    /// as the server may have a new key.
    fn session_for(&mut self, link: &Link) -> usize {
        let link = link.server();
        if let Some(index) = self.sessions.iter().position(|session| session.link == link) {
            return index;
        }
        let host = Session::new(0, link.clone(), false).host();
        match self.sessions.iter().position(|session| session.host().eq_ignore_ascii_case(&host)) {
            Some(index) => {
                self.disconnect_session(index);
                self.sessions[index].link = link;
                index
            }
            None => self.add_session(link, true),
        }
    }

    fn is_public(&self, index: usize) -> bool {
        public_server().is_some_and(|link| self.sessions[index].link == link)
    }

    fn add_session(&mut self, link: Link, up: bool) -> usize {
        let id = self.next_server;
        self.next_server += 1;
        self.sessions.push(Session::new(id, link, up));
        self.sessions.len() - 1
    }

    fn up(
        &mut self,
        link: Option<String>,
        nickname: Option<String>,
        server: Option<&str>,
        reply: oneshot::Sender<Response>,
    ) {
        let mut renamed = false;
        if let Some(nickname) = nickname {
            let nickname = nickname.trim().to_string();
            if !(1..=32).contains(&nickname.chars().count()) {
                let _ = reply.send(Response::Error(Failure::InvalidNickname));
                return;
            }
            renamed = self.settings.nickname.as_ref() != Some(&nickname);
            self.settings.nickname = Some(nickname);
        }
        let chosen = match (link, server) {
            (Some(link), _) => {
                let Ok(link) = link.parse::<Link>() else {
                    let _ = reply.send(Response::Error(Failure::InvalidLink));
                    return;
                };
                if public_server().is_some_and(|public| public == link.server()) {
                    self.settings.public_server = Some(true);
                }
                Some(self.session_for(&link))
            }
            (None, Some(selector)) => match self.target(Some(selector), None) {
                Ok(index) => Some(index),
                Err(failure) => {
                    let _ = reply.send(Response::Error(failure));
                    return;
                }
            },
            (None, None) => None,
        };
        if self.sessions.is_empty() {
            let response = if renamed {
                self.save_settings();
                Response::Ok
            } else {
                Response::Error(Failure::NoServer)
            };
            let _ = reply.send(response);
            return;
        }
        let chosen: Vec<usize> = match chosen {
            Some(index) => vec![index],
            None => (0..self.sessions.len()).collect(),
        };
        for &index in &chosen {
            self.sessions[index].up = true;
        }
        self.save_settings();
        for index in 0..self.sessions.len() {
            let session = &self.sessions[index];
            let restart = renamed && session.up;
            if restart || (chosen.contains(&index) && session.control.is_none()) {
                self.disconnect_session(index);
                self.connect(index);
            }
        }
        let waiting = chosen.into_iter().find(|&index| self.sessions[index].connection != Connection::Connected);
        match waiting {
            Some(index) => self.sessions[index].up_waiters.push(reply),
            None => {
                let _ = reply.send(Response::Ok);
            }
        }
    }

    fn down(&mut self, server: Option<&str>, reply: oneshot::Sender<Response>) {
        let chosen: Vec<usize> = match server {
            Some(selector) => match self.target(Some(selector), None) {
                Ok(index) => vec![index],
                Err(failure) => {
                    let _ = reply.send(Response::Error(failure));
                    return;
                }
            },
            None => (0..self.sessions.len()).collect(),
        };
        for index in chosen {
            self.sessions[index].up = false;
            self.disconnect_session(index);
        }
        self.save_settings();
        self.refresh();
        let _ = reply.send(Response::Ok);
    }

    fn remove(&mut self, server: Option<&str>, reply: oneshot::Sender<Response>) {
        let index = match self.target(server, None) {
            Ok(index) => index,
            Err(failure) => {
                let _ = reply.send(Response::Error(failure));
                return;
            }
        };
        if self.is_hosted(index) {
            self.stop_hosting();
            let _ = reply.send(Response::Ok);
            return;
        }
        if self.is_public(index) {
            self.settings.public_server = Some(false);
        }
        self.disconnect_session(index);
        self.sessions.remove(index);
        self.save_settings();
        self.refresh();
        let _ = reply.send(Response::Ok);
    }

    async fn host(
        &mut self,
        enabled: bool,
        port: Option<u16>,
        address: Option<String>,
        reply: oneshot::Sender<Response>,
    ) {
        if let Some(address) = address {
            let address = address.trim().to_string();
            self.settings.host.address = (!address.is_empty()).then_some(address);
        }
        if !enabled {
            self.stop_hosting();
            let _ = reply.send(Response::Ok);
            return;
        }
        if let Some(port) = port
            && port != self.settings.host.port
        {
            self.settings.host.port = port;
            if self.hosted.is_some() {
                self.stop_hosting();
            }
        }
        if self.hosted.is_none()
            && let Err(error) = self.start_hosting().await
        {
            tracing::error!(%error, "cannot host a server");
            self.save_settings();
            let _ = reply.send(Response::Error(Failure::CannotHost));
            return;
        }
        self.save_settings();
        let _ = reply.send(Response::Ok);
    }

    /// Starts the hosted server and connects this device to it.
    async fn start_hosting(&mut self) -> std::io::Result<()> {
        let hosted = Hosted::start(&self.state_dir.join("loom"), self.settings.host.port).await?;
        self.settings.host.enabled = true;
        self.settings.host.port = hosted.port;
        let link = hosted.local_link();
        self.hosted = Some(hosted);
        let index = self.session_for(&link);
        self.sessions[index].up = true;
        if self.sessions[index].control.is_none() {
            self.connect(index);
        }
        self.save_settings();
        Ok(())
    }

    fn stop_hosting(&mut self) {
        if let Some(hosted) = self.hosted.take() {
            let link = hosted.local_link();
            if let Some(index) = self.sessions.iter().position(|session| session.link == link) {
                self.disconnect_session(index);
                self.sessions.remove(index);
            }
        }
        self.settings.host.enabled = false;
        self.save_settings();
        self.refresh();
    }

    fn is_hosted(&self, index: usize) -> bool {
        self.hosted.as_ref().is_some_and(|hosted| self.sessions[index].link == hosted.local_link())
    }

    /// The link others use: the shared one for the hosted server.
    fn public_link(&self, index: usize) -> Link {
        match &self.hosted {
            Some(hosted) if self.is_hosted(index) => hosted
                .share(self.settings.host.address.as_deref())
                .0
                .unwrap_or_else(|| self.sessions[index].link.clone()),
            _ => self.sessions[index].link.clone(),
        }
    }

    fn redeem(&mut self, link: String, reply: oneshot::Sender<Response>) {
        let Some((link, code)) = link.parse::<Link>().ok().and_then(|link| Some((link.server(), link.invite?))) else {
            let _ = reply.send(Response::Error(Failure::InvalidLink));
            return;
        };
        let index = self.session_for(&link);
        self.sessions[index].up = true;
        self.save_settings();
        if self.sessions[index].control.is_none() {
            self.connect(index);
        }
        if self.sessions[index].connection == Connection::Connected {
            self.request(index, ClientKind::RedeemInvite(InviteCode { code }), reply);
        } else {
            self.sessions[index].redeem_waiters.push((code, reply));
        }
    }

    fn request(&mut self, index: usize, kind: ClientKind, reply: oneshot::Sender<Response>) {
        if self.sessions[index].connection != Connection::Connected {
            let _ = reply.send(Response::Error(Failure::NotConnected));
            return;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        if self.send_control(index, ClientMessage { id, kind: Some(kind) }) {
            self.sessions[index].pending.insert(id, reply);
        } else {
            let _ = reply.send(Response::Error(Failure::NotConnected));
        }
    }

    fn send_control(&self, index: usize, message: ClientMessage) -> bool {
        self.sessions[index].control.as_ref().is_some_and(|control| control.send(message))
    }

    fn connect(&mut self, index: usize) {
        let nickname = self.settings.nickname.clone().unwrap_or_else(default_nickname);
        let address = self.addresses.first().map(|&(address, _)| address).or(self.settings.address);
        self.generation += 1;
        let session = &mut self.sessions[index];
        session.generation = self.generation;
        session.control = Some(Control::spawn(
            session.link.clone(),
            self.settings.transport,
            self.keypair.clone(),
            Hello { version: PROTOCOL_VERSION, nickname, address: address.map(u32::from) },
            self.generation,
            self.control_tx.clone(),
        ));
        session.connection = Connection::Connecting;
        session.reconnect_at = None;
    }

    /// Closes the connection and forgets what the server said; the caller refreshes shared state.
    fn disconnect_session(&mut self, index: usize) {
        self.generation += 1;
        let session = &mut self.sessions[index];
        session.generation = self.generation;
        session.control = None;
        session.connection = Connection::Disconnected;
        session.welcome = None;
        session.reconnect_at = None;
        session.candidates_at = None;
        session.candidates.clear();
        session.state = State::default();
        session.fail_pending(Failure::NotConnected);
        self.mesh.clear_server(session.id);
    }

    /// Rebuilds what depends on all servers: mesh peers, names, and the TUN interface.
    fn refresh(&mut self) {
        if self.sessions.iter().all(|session| !session.up) {
            self.tun = None;
            self.addresses.clear();
            self.mesh.set_locals(&[]);
        }
        self.mesh.update_peers(std::time::Instant::now(), self.peer_configs());
        self.update_names();
    }

    async fn on_control(&mut self, index: usize, event: ControlEvent) {
        match event {
            ControlEvent::Connected { welcome, server } => self.on_connected(index, welcome, server),
            ControlEvent::Refused(code) => {
                let session = &mut self.sessions[index];
                tracing::warn!(server = %session.host(), ?code, "server refused the connection");
                session.control = None;
                session.connection = Connection::Disconnected;
                session.fail_pending(failure(code));
            }
            ControlEvent::Message(message) => self.on_message(index, message).await,
            ControlEvent::Closed(reason) => {
                let session = &mut self.sessions[index];
                tracing::warn!(server = %session.host(), %reason, "server connection closed");
                session.control = None;
                session.welcome = None;
                session.candidates_at = None;
                session.candidates.clear();
                session.fail_pending(Failure::NotConnected);
                if session.up {
                    session.connection = Connection::Connecting;
                    session.reconnect_at = Some(Instant::now() + session.backoff);
                    session.backoff = (session.backoff * 2).min(MAX_BACKOFF);
                } else {
                    session.connection = Connection::Disconnected;
                }
                let id = session.id;
                self.mesh.clear_server(id);
            }
        }
    }

    fn on_connected(&mut self, index: usize, welcome: Welcome, server: SocketAddr) {
        let address = Ipv4Addr::from(welcome.address);
        let prefix = welcome.prefix_len.min(32) as u8;
        tracing::info!(%server, %address, "connected to the server");
        self.attach_address(address, prefix);
        if self.settings.address.is_none() {
            self.settings.address = Some(address);
            self.save_settings();
        }
        let session = &mut self.sessions[index];
        session.connection = Connection::Connected;
        session.backoff = MIN_BACKOFF;
        if let Ok(token) = welcome.discovery_token.as_slice().try_into() {
            let udp = SocketAddr::new(server.ip(), welcome.udp_port as u16);
            let (id, key) = (session.id, session.link.server_key);
            self.mesh.set_server(std::time::Instant::now(), id, udp, key, token);
        }
        let session = &mut self.sessions[index];
        session.welcome = Some(welcome);
        session.candidates.clear();
        session.candidates_at = Some(Instant::now());
        for reply in session.up_waiters.drain(..) {
            let _ = reply.send(Response::Ok);
        }
        for (code, reply) in std::mem::take(&mut session.redeem_waiters) {
            self.request(index, ClientKind::RedeemInvite(InviteCode { code }), reply);
        }
        self.update_names();
    }

    /// Puts the address on the TUN interface, creating it for the first server.
    fn attach_address(&mut self, address: Ipv4Addr, prefix: u8) {
        if self.addresses.iter().any(|&(known, _)| known == address) {
            return;
        }
        let subnet = |address: Ipv4Addr, prefix: u8| {
            u32::from(address) & u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0)
        };
        let clash = self.addresses.iter().find(|&&(known, known_prefix)| {
            let prefix = prefix.min(known_prefix);
            subnet(known, prefix) == subnet(address, prefix)
        });
        if let Some((known, _)) = clash {
            tracing::warn!(%address, %known, "another server gave a different address in the same range; its peers will not work");
            return;
        }
        if !self.echo {
            match &self.tun {
                None => match self.create_tun(address, prefix) {
                    Ok(tun) => {
                        tracing::info!(name = tun.name(), %address, "tun interface is up");
                        self.tun = Some(tun);
                    }
                    Err(error) => {
                        tracing::error!(%error, "cannot create tun interface");
                        return;
                    }
                },
                Some(tun) => {
                    if let Err(error) = tun.add_address(address, prefix) {
                        tracing::warn!(%address, %error, "cannot add an address to the tun interface");
                        return;
                    }
                    tracing::info!(%address, "address added for another server");
                }
            }
        }
        self.addresses.push((address, prefix));
        self.mesh.set_locals(&self.addresses);
    }

    fn create_tun(&self, address: Ipv4Addr, prefix: u8) -> std::io::Result<Tun> {
        let mut routes: Vec<Route> = self.settings.multicast_groups.iter().copied().map(Route::host).collect();
        if self.settings.broadcast {
            routes.insert(0, Route::host(Ipv4Addr::BROADCAST));
        }
        let dns = self.settings.dns.then(|| {
            routes.push(Route::host(DNS_ADDRESS));
            Dns { server: DNS_ADDRESS, domain: dns::ZONE.to_string() }
        });
        Tun::create(&TunConfig { name: self.tun_name.clone(), address, prefix, mtu: DEFAULT_MTU, routes, dns })
    }

    async fn on_message(&mut self, index: usize, message: ServerMessage) {
        if message.reply_to != 0
            && let Some(reply) = self.sessions[index].pending.remove(&message.reply_to)
        {
            let response = match message.kind {
                Some(ServerKind::Ack(_)) => Response::Ok,
                Some(ServerKind::Failure(f)) => Response::Error(failure(f.code())),
                Some(ServerKind::Joined(network)) => Response::Joined(network.name),
                Some(ServerKind::Invite(invite)) => Response::Invite(self.invite_info(index, invite)),
                Some(ServerKind::Invites(list)) => {
                    Response::Invites(list.invites.into_iter().map(|invite| self.invite_info(index, invite)).collect())
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
                self.sessions[index].state = state;
                self.refresh();
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
        if !self.extra.is_empty() {
            let used = self.mesh.sockets();
            self.extra.retain(|id, _| used.contains(id));
        }
        for index in 0..self.sessions.len() {
            if self.sessions[index].reconnect_at.is_some_and(|at| now >= at) {
                self.connect(index);
            }
            if self.sessions[index].candidates_at.is_some_and(|at| now >= at) {
                self.sessions[index].candidates_at = Some(now + CANDIDATES_INTERVAL);
                self.publish_candidates(index);
            }
        }
    }

    async fn extra_socket(&mut self, id: SocketId, to: SocketAddr, ttl: u32) -> Option<Arc<UdpSocket>> {
        if let Some(extra) = self.extra.get_mut(&id) {
            if extra.ttl != ttl && extra.socket.set_ttl(ttl).is_ok() {
                extra.ttl = ttl;
            }
            return Some(extra.socket.clone());
        }
        let bind = if to.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
        let socket = match UdpSocket::bind(bind).await {
            Ok(socket) => Arc::new(socket),
            Err(error) => {
                tracing::debug!(%error, "cannot open a punching socket");
                return None;
            }
        };
        if let Err(error) = socket.set_ttl(ttl) {
            tracing::debug!(%error, "cannot set the ttl");
        }
        let (receiver, tx) = (socket.clone(), self.extra_tx.clone());
        let task = tokio::spawn(async move {
            let mut buf = vec![0; 65_535];
            while let Ok((len, from)) = receiver.recv_from(&mut buf).await {
                if tx.send((id, from, buf[..len].to_vec())).await.is_err() {
                    return;
                }
            }
        });
        self.extra.insert(id, ExtraSocket { socket: socket.clone(), ttl, task });
        Some(socket)
    }

    fn local_candidates(&self) -> Vec<SocketAddr> {
        let tun = self.tun.as_ref().map(|tun| tun.name().to_string()).unwrap_or_else(|| self.tun_name.clone());
        let own: Vec<IpAddr> = self.addresses.iter().map(|&(address, _)| IpAddr::V4(address)).collect();
        let mut candidates: Vec<SocketAddr> = weft_portmap::local_addresses(Some(&tun))
            .into_iter()
            .filter(|ip| !own.contains(ip))
            .filter(|ip| ip.is_ipv4() || self.udp6.is_some())
            .map(|ip| SocketAddr::new(ip, if ip.is_ipv4() { self.udp_port } else { self.udp6_port }))
            .collect();
        let observed = self.mesh.observed().map(|addr| addr.ip());
        if let Some(mapped) = self.mapper.current().and_then(|mapped| mapped.endpoint(observed)) {
            candidates.insert(0, mapped);
        }
        candidates.dedup();
        candidates
    }

    fn publish_candidates(&mut self, index: usize) {
        if self.sessions[index].connection != Connection::Connected {
            return;
        }
        let candidates = self.local_candidates();
        if candidates != self.sessions[index].candidates {
            tracing::debug!(?candidates, "publishing candidates");
            let endpoints = candidates.iter().map(|&addr| Endpoint::from(addr)).collect();
            let message = ClientMessage { id: 0, kind: Some(ClientKind::Candidates(Candidates { endpoints })) };
            if self.send_control(index, message) {
                self.sessions[index].candidates = candidates;
            }
        }
    }

    fn session_by_id(&self, id: ServerId) -> Option<usize> {
        self.sessions.iter().position(|session| session.id == id)
    }

    async fn flush(&mut self) {
        while let Some(output) = self.mesh.poll_output() {
            match output {
                Output::Udp { socket: 0, to, datagram, .. } => {
                    let socket = match to {
                        SocketAddr::V4(_) => &self.udp,
                        SocketAddr::V6(_) => match &self.udp6 {
                            Some(socket) => socket,
                            None => continue,
                        },
                    };
                    if let Err(error) = socket.send_to(&datagram, to).await {
                        tracing::trace!(%to, %error, "udp send failed");
                    }
                }
                Output::Udp { socket, to, datagram, ttl } => {
                    let Some(extra) = self.extra_socket(socket, to, ttl.map_or(DEFAULT_TTL, u32::from)).await else {
                        continue;
                    };
                    if let Err(error) = extra.send_to(&datagram, to).await {
                        tracing::trace!(%to, socket, %error, "udp send failed");
                    }
                }
                Output::TcpRelay { server, to, packet } => {
                    if let Some(index) = self.session_by_id(server) {
                        let relay = RelayPacket { address: u32::from(to), packet };
                        self.send_control(index, ClientMessage { id: 0, kind: Some(ClientKind::Relay(relay)) });
                    }
                }
                Output::CallMeMaybe { server, peer } => {
                    if let Some(index) = self.session_by_id(server) {
                        let key = PeerKey { key: peer.as_bytes().to_vec() };
                        self.send_control(index, ClientMessage { id: 0, kind: Some(ClientKind::CallMeMaybe(key)) });
                    }
                }
            }
        }
    }

    async fn deliver(&mut self, packet: &[u8]) {
        if self.echo {
            let destination =
                (packet.len() >= 20).then(|| Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]));
            let own =
                destination.filter(|destination| self.addresses.iter().any(|&(address, _)| address == *destination));
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

    /// Peers of all servers; a device met on several servers is kept once, preferring an online entry.
    fn peer_configs(&self) -> Vec<PeerConfig> {
        let mut configs: HashMap<PublicKey, PeerConfig> = HashMap::new();
        let mut owners: HashMap<Ipv4Addr, PublicKey> = HashMap::new();
        for session in &self.sessions {
            for peer in &session.state.peers {
                let Ok(key) = PublicKey::from_slice(&peer.key) else { continue };
                let address = Ipv4Addr::from(peer.address);
                if owners.get(&address).is_some_and(|owner| *owner != key) {
                    tracing::warn!(%address, "two peers on different servers share an address; ignoring the second");
                    continue;
                }
                owners.insert(address, key);
                let candidates =
                    peer.endpoint.iter().chain(&peer.candidates).filter_map(|e| e.to_socket_addr()).collect();
                let endpoint = peer.endpoint.as_ref().and_then(|endpoint| endpoint.to_socket_addr());
                let config = PeerConfig { key, address, online: peer.online, candidates, endpoint, server: session.id };
                match configs.entry(key) {
                    Entry::Vacant(entry) => {
                        entry.insert(config);
                    }
                    Entry::Occupied(mut entry) if !entry.get().online && config.online => {
                        entry.insert(config);
                    }
                    Entry::Occupied(_) => {}
                }
            }
        }
        configs.into_values().collect()
    }

    fn status(&self) -> Status {
        let servers = (0..self.sessions.len())
            .map(|index| {
                let session = &self.sessions[index];
                let link = self.public_link(index);
                ServerStatus {
                    server: link.to_string(),
                    host: link_host(&link),
                    connection: session.connection,
                    address: session.address(),
                    networks: self.networks(session),
                    hosted: self.is_hosted(index),
                    public: self.is_public(index),
                }
            })
            .collect();
        let host = self.hosted.as_ref().map(|hosted| {
            let address = self.settings.host.address.clone();
            let (link, reach) = hosted.share(address.as_deref());
            HostStatus {
                link: link.map(|link| link.to_string()),
                port: hosted.port,
                reach,
                mapped: hosted.mapped(),
                address,
            }
        });
        Status {
            nickname: self.settings.nickname.clone().unwrap_or_else(default_nickname),
            public_key: self.keypair.public().to_string(),
            servers,
            host,
            public_link: public_server().map(|link| link.to_string()),
        }
    }

    fn networks(&self, session: &Session) -> Vec<NetworkStatus> {
        let own = self.keypair.public();
        let peers: HashMap<&[u8], &weft_proto::control::Peer> =
            session.state.peers.iter().map(|peer| (peer.key.as_slice(), peer)).collect();
        session
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
            .collect()
    }

    fn update_names(&mut self) {
        let own = self.keypair.public();
        let nickname = self.settings.nickname.clone().unwrap_or_else(default_nickname);
        let mine = self.addresses.iter().map(|&(address, _)| (nickname.as_str(), address));
        let peers = self
            .sessions
            .iter()
            .flat_map(|session| &session.state.peers)
            .filter(|peer| peer.key.as_slice() != own.as_bytes())
            .map(|peer| (peer.nickname.as_str(), Ipv4Addr::from(peer.address)));
        self.names = dns::names(mine.chain(peers));
    }

    fn dns_name(&self, nickname: &str, address: Ipv4Addr) -> Option<String> {
        let label = dns::label(nickname)?;
        (self.names.get(&label) == Some(&address)).then(|| format!("{label}.{}", dns::ZONE))
    }

    fn diagnostics(&self, logs: bool) -> Diagnostics {
        let report = self.mesh.report(std::time::Instant::now());
        let tun = self.tun.as_ref().map(|tun| tun.name().to_string()).unwrap_or_else(|| self.tun_name.clone());
        let own: Vec<IpAddr> = self.addresses.iter().map(|&(address, _)| IpAddr::V4(address)).collect();
        let local_addresses: Vec<IpAddr> =
            weft_portmap::local_addresses(Some(&tun)).into_iter().filter(|ip| !own.contains(ip)).collect();
        let mapped = self.mapper.current();
        let port_mapping = mapped.map(|mapped| PortMapping {
            protocol: mapped.protocol.name().to_string(),
            external: mapped.endpoint(report.observed.map(|addr| addr.ip())),
        });
        let seen: Vec<SocketAddr> = report.peers.iter().filter_map(|peer| peer.observed).collect();
        let nat = weft_ipc::report::classify_nat(report.observed, self.udp_port, &local_addresses, &seen);
        let reports: HashMap<PublicKey, &weft_mesh::PeerReport> =
            report.peers.iter().map(|peer| (peer.key, peer)).collect();
        let own_key = self.keypair.public();
        let mut listed = std::collections::HashSet::new();
        let peers = self
            .sessions
            .iter()
            .flat_map(|session| &session.state.peers)
            .filter(|peer| peer.key.as_slice() != own_key.as_bytes() && listed.insert(peer.key.clone()))
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
        let servers = self
            .sessions
            .iter()
            .map(|session| ServerDiagnostics {
                server: session.link.to_string(),
                connection: session.connection,
                udp: report.servers.iter().find(|server| server.id == session.id).and_then(|server| server.udp),
            })
            .collect();
        Diagnostics {
            version: env!("CARGO_PKG_VERSION").to_string(),
            os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            servers,
            transport: match self.settings.transport {
                Transport::Tls => "tls",
                Transport::Raw => "raw",
            }
            .to_string(),
            observed: report.observed,
            local_port: self.udp_port,
            ipv6: self.udp6.is_some() && local_addresses.iter().any(IpAddr::is_ipv6),
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

    fn invite_info(&self, index: usize, invite: Invite) -> InviteInfo {
        let link = Link { invite: Some(invite.code.clone()), ..self.public_link(index) }.to_string();
        InviteInfo {
            code: invite.code,
            link: Some(link),
            network: invite.network,
            max_uses: (invite.max_uses > 0).then_some(invite.max_uses),
            uses: invite.uses,
            expires: (invite.expires > 0).then_some(invite.expires),
            creator: invite.creator,
        }
    }

    fn save_settings(&mut self) {
        self.settings.servers = self
            .sessions
            .iter()
            .map(|session| ServerEntry { link: session.link.to_string(), up: session.up })
            .collect();
        if let Err(error) = self.settings_file.save(&self.settings) {
            tracing::error!(path = %self.settings_file.path().display(), %error, "cannot save settings");
        }
    }
}

/// How well `selector` names the server: 2 for its link or `host:port`, 1 for the host alone.
fn link_matches(link: &Link, selector: &str) -> u8 {
    let selector = selector.trim();
    let host = link_host(link);
    let bare = host.rsplit_once(':').map_or(host.as_str(), |(bare, _)| bare);
    if selector.eq_ignore_ascii_case(&host) || selector.parse::<Link>().is_ok_and(|other| other.server() == *link) {
        2
    } else if selector.eq_ignore_ascii_case(bare) {
        1
    } else {
        0
    }
}

fn link_host(link: &Link) -> String {
    let host = match &link.host {
        Host::Domain(name) => name.clone(),
        Host::Ipv4(ip) => ip.to_string(),
        Host::Ipv6(ip) => format!("[{ip}]"),
    };
    format!("{host}:{}", link.port)
}

fn bind_v6(port: u16) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_only_v6(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)).into())?;
    UdpSocket::from_std(socket.into())
}

async fn recv_v6(socket: Option<&UdpSocket>, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
    match socket {
        Some(socket) => socket.recv_from(buf).await,
        None => std::future::pending().await,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn session(link: &str) -> Session {
        Session::new(1, link.parse::<Link>().unwrap().server(), true)
    }

    #[test]
    fn selects_servers() {
        let key = "ci2rw2hrlkx25sxk5gbtj3vhkyaphbnb6xol2j7giyupfoz7vmfq";
        let a = session(&format!("weft://vpn.example.com:443#k={key}"));
        assert_eq!(a.host(), "vpn.example.com:443");
        assert_eq!(a.matches("vpn.example.com"), 1);
        assert_eq!(a.matches("VPN.example.com:443"), 2);
        assert_eq!(a.matches(&format!("weft://vpn.example.com:443/ABCD#k={key}")), 2);
        assert_eq!(a.matches("other.example.com"), 0);
        let b = session(&format!("weft://[2001:db8::1]:7443#k={key}"));
        assert_eq!(b.host(), "[2001:db8::1]:7443");
        assert_eq!(b.matches("[2001:db8::1]"), 1);
    }
}
