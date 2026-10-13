use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use rand::RngExt;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use weft_proto::control::{
    self, Avatar, Candidates, ClientKind, ClientMessage, DecodeError, DeviceInfo, DeviceList, Empty, ErrorCode, Invite,
    InviteCode, InviteList, InviteRequest, MAX_AVATAR_LEN, MUTE_FOREVER, MemberAction, MuteRequest, NetworkCredentials,
    NetworkName, NetworkSettings, PROTOCOL_VERSION, PeerKey, RelayPacket, Role, RoleChange, ServerKind, ServerMessage,
    Welcome, avatar_id,
};
use weft_proto::{MIN_PACKET_LEN, ObfsKey, PublicKey};
use weft_session::StaticKeypair;
use weft_session::stream::{self, io};
use weft_session::tls::{RECORD_HANDSHAKE, TlsAcceptor};

use crate::db::{DbError, Device, InviteRow, MUTED_FOREVER, NetworkRow, name_key, unix_now};
use crate::hub::{Hub, MAX_CANDIDATES, SharedHub, lock};
use crate::panel::Panel;
use crate::tls::{http_reply, is_http};
use crate::udp;
use crate::validate;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_INVITES: usize = 100;
const MAX_INVITE_LIFETIME: u64 = 365 * 24 * 3600;
const MAX_INVITE_LEN: usize = 64;
const MAX_MUTE: u64 = 10 * 365 * 24 * 3600;
/// Control messages a client may send per second over time, relayed packets aside, and in a burst.
const MESSAGES_PER_SECOND: f64 = 50.0;
const MESSAGE_BURST: f64 = 300.0;
const REFRESH_EVERY: Duration = Duration::from_secs(1);
const AVATAR_EVERY: Duration = Duration::from_secs(10);
/// Networks one device may own.
const MAX_OWNED: usize = 32;
/// Open connections from one address, so one host cannot hold them all.
const MAX_CONNECTIONS_PER_IP: usize = 64;
const INVITE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

#[derive(Debug, thiserror::Error)]
enum ConnError {
    #[error(transparent)]
    Session(#[from] weft_session::Error),
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("timed out")]
    Timeout,
    #[error("protocol violation")]
    Protocol,
    #[error("stale handshake")]
    Stale,
}

pub async fn serve(
    listener: TcpListener,
    hub: SharedHub,
    keypair: Arc<StaticKeypair>,
    socket: Arc<UdpSocket>,
    tls: TlsAcceptor,
    panel: Arc<Panel>,
) {
    let mut next_conn = 0;
    let open: Arc<std::sync::Mutex<HashMap<IpAddr, usize>>> = Arc::default();
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                tracing::warn!(%error, "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        next_conn += 1;
        let shared = Shared {
            hub: hub.clone(),
            keypair: keypair.clone(),
            socket: socket.clone(),
            tls: tls.clone(),
            panel: panel.clone(),
        };
        let conn = next_conn;
        let Some(slot) = Slot::take(&open, addr.ip()) else {
            tracing::debug!(%addr, "too many connections from one address");
            continue;
        };
        tokio::spawn(async move {
            let _slot = slot;
            if let Err(error) = accept(stream, addr, shared, conn).await {
                tracing::debug!(%addr, %error, "connection closed");
            }
        });
    }
}

/// One open connection counted against its address until dropped.
struct Slot {
    open: Arc<std::sync::Mutex<HashMap<IpAddr, usize>>>,
    ip: IpAddr,
}

impl Slot {
    fn take(open: &Arc<std::sync::Mutex<HashMap<IpAddr, usize>>>, ip: IpAddr) -> Option<Slot> {
        let mut counts = open.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let count = counts.entry(ip).or_default();
        if *count >= MAX_CONNECTIONS_PER_IP {
            return None;
        }
        *count += 1;
        Some(Slot { open: open.clone(), ip })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut counts = self.open.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = counts.get_mut(&self.ip) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.ip);
            }
        }
    }
}

/// How fast one client sends requests: a token bucket for all of them, and spacing for the
/// ones that make the server do more work.
struct Pace {
    tokens: f64,
    at: Instant,
    refreshed: Option<Instant>,
    pictured: Option<Instant>,
}

impl Pace {
    fn new(now: Instant) -> Self {
        Self { tokens: MESSAGE_BURST, at: now, refreshed: None, pictured: None }
    }

    fn take(&mut self, now: Instant) -> bool {
        let elapsed = now.duration_since(self.at).as_secs_f64();
        self.at = now;
        self.tokens = (self.tokens + elapsed * MESSAGES_PER_SECOND).min(MESSAGE_BURST);
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }

    fn spaced(last: &mut Option<Instant>, every: Duration, now: Instant) -> bool {
        if last.is_some_and(|last| now.duration_since(last) < every) {
            return false;
        }
        *last = Some(now);
        true
    }
}

/// What every connection needs from the server.
struct Shared {
    hub: SharedHub,
    keypair: Arc<StaticKeypair>,
    socket: Arc<UdpSocket>,
    tls: TlsAcceptor,
    panel: Arc<Panel>,
}

/// Takes TLS or the plain stream of older clients. HTTP inside TLS goes to the panel, which
/// answers everything but its secret path like nginx would.
async fn accept(stream: TcpStream, addr: SocketAddr, shared: Shared, conn: u64) -> Result<(), ConnError> {
    let Shared { hub, keypair, socket, tls, panel } = shared;
    stream.set_nodelay(true)?;
    let mut start = [0; 4];
    let peeked = timeout(HANDSHAKE_TIMEOUT, stream.peek(&mut start)).await.map_err(|_| ConnError::Timeout)??;
    if peeked == 0 {
        return Ok(());
    }
    if start[0] != RECORD_HANDSHAKE {
        if is_http(&start[..peeked]) {
            let message = "<center>The plain HTTP request was sent to HTTPS port</center>\r\n";
            return decoy(stream, &http_reply("400 Bad Request", message)).await;
        }
        let (reader, writer) = stream.into_split();
        return connection(reader, writer, addr, hub, keypair, socket, conn).await;
    }
    let stream = timeout(HANDSHAKE_TIMEOUT, tls.accept(stream)).await.map_err(|_| ConnError::Timeout)??;
    let mut stream = stream;
    let mut start = [0; 4];
    timeout(HANDSHAKE_TIMEOUT, stream.read_exact(&mut start)).await.map_err(|_| ConnError::Timeout)??;
    if is_http(&start) {
        return Ok(crate::panel::serve(stream, start, addr.ip(), hub, panel).await?);
    }
    let (reader, writer) = tokio::io::split(stream);
    let reader = std::io::Cursor::new(start).chain(reader);
    connection(reader, writer, addr, hub, keypair, socket, conn).await
}

/// Reads the request, answers and closes gracefully so the reply is not lost to a reset.
async fn decoy<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, reply: &[u8]) -> Result<(), ConnError> {
    let mut buf = [0; 4096];
    let _ = timeout(Duration::from_millis(500), stream.read(&mut buf)).await;
    stream.write_all(reply).await?;
    stream.shutdown().await?;
    let _ = timeout(Duration::from_secs(1), async { while matches!(stream.read(&mut buf).await, Ok(1..)) {} }).await;
    Ok(())
}

async fn connection<R, W>(
    mut reader: R,
    mut writer: W,
    addr: SocketAddr,
    hub: SharedHub,
    keypair: Arc<StaticKeypair>,
    socket: Arc<UdpSocket>,
    conn: u64,
) -> Result<(), ConnError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let udp_port = socket.local_addr()?.port();
    let own = ObfsKey::for_receiver(&keypair.public());

    let (mut sender, mut receiver, hello) = timeout(HANDSHAKE_TIMEOUT, async {
        let packet = io::read_handshake(&mut reader, &own).await?;
        let accepted = stream::accept(&keypair, &packet)?;
        if !lock(&hub).accept_timestamp(accepted.channel.remote(), accepted.timestamp) {
            return Err(ConnError::Stale);
        }
        writer.write_all(&accepted.frame).await?;
        let (sender, mut receiver) = accepted.channel.split();
        let hello: ClientMessage = control::decode(&io::read_message(&mut reader, &mut receiver).await?)?;
        Ok((sender, receiver, hello))
    })
    .await
    .map_err(|_| ConnError::Timeout)??;

    let key = receiver.remote();
    let Some(ClientKind::Hello(greeting)) = hello.kind else {
        return Err(ConnError::Protocol);
    };
    let refuse = |code| ServerMessage::failure(hello.id, code);
    let nickname = validate::nickname(&greeting.nickname);
    let early_failure = if greeting.version != PROTOCOL_VERSION {
        Some(refuse(ErrorCode::UnsupportedVersion))
    } else if nickname.is_none() {
        Some(refuse(ErrorCode::InvalidNickname))
    } else {
        None
    };
    if let Some(failure) = early_failure {
        writer.write_all(&sender.seal(&control::encode(&failure))?).await?;
        return Ok(());
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (kill_tx, mut kill_rx) = oneshot::channel();
    let welcome = {
        let mut hub = lock(&hub);
        let pool = hub.config.pool;
        let blocked = hub.db.is_blocked(&key).unwrap_or(false);
        let known = hub.db.device_exists(&key).unwrap_or(true);
        let device = if blocked {
            Err(DbError::Blocked)
        } else if !known && !hub.limiter.registration_allowed(addr.ip(), Instant::now()) {
            Err(DbError::TooManyDevices)
        } else {
            let preferred = greeting.address.map(Ipv4Addr::from);
            hub.db.upsert_device_preferring(&key, nickname.as_deref().unwrap_or_default(), &pool, preferred)
        };
        match device {
            Ok(device) => {
                let token = hub.register(key, device.address, conn, tx.clone(), kill_tx);
                let avatar = hub.db.avatar_ids(&BTreeSet::from([key])).ok().and_then(|mut ids| ids.remove(&key));
                ServerMessage::reply(
                    hello.id,
                    ServerKind::Welcome(Welcome {
                        address: u32::from(device.address),
                        prefix_len: u32::from(pool.prefix()),
                        discovery_token: token.to_vec(),
                        udp_port: u32::from(udp_port),
                        avatar: avatar.unwrap_or_default(),
                    }),
                )
            }
            Err(DbError::PoolExhausted) => refuse(ErrorCode::PoolExhausted),
            Err(DbError::TooManyDevices) => refuse(ErrorCode::RateLimited),
            Err(DbError::Blocked) => refuse(ErrorCode::Banned),
            Err(error) => {
                tracing::warn!(%error, "cannot register device");
                refuse(ErrorCode::Internal)
            }
        }
    };
    let registered = matches!(welcome.kind, Some(ServerKind::Welcome(_)));
    let _ = tx.send(welcome);
    if registered {
        tracing::info!(?key, %addr, "device connected");
        lock(&hub).notify_related(&key);
    }

    let writer_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            let frame = sender.seal(&control::encode(&message))?;
            writer.write_all(&frame).await?;
        }
        Ok::<_, ConnError>(())
    });
    if !registered {
        drop(tx);
        let _ = writer_task.await;
        return Ok(());
    }

    let mut pace = Pace::new(Instant::now());
    let result = loop {
        let bytes = tokio::select! {
            _ = &mut kill_rx => break Ok(()),
            read = timeout(IDLE_TIMEOUT, io::read_message(&mut reader, &mut receiver)) => match read {
                Err(_) => break Err(ConnError::Timeout),
                Ok(Err(error)) => break Err(error.into()),
                Ok(Ok(bytes)) => bytes,
            },
        };
        let message: ClientMessage = match control::decode(&bytes) {
            Ok(message) => message,
            Err(error) => break Err(error.into()),
        };
        let relayed = matches!(message.kind, Some(ClientKind::Relay(_)));
        if !relayed && !pace.take(Instant::now()) {
            tracing::debug!(?key, %addr, "too many requests, closing");
            break Err(ConnError::Protocol);
        }
        let Some(reply) = handle(&hub, &socket, key, addr.ip(), message, &mut pace).await else { continue };
        if tx.send(reply).is_err() {
            break Ok(());
        }
    };

    {
        let mut hub = lock(&hub);
        if hub.unregister(&key, conn) {
            tracing::info!(?key, "device disconnected");
            hub.notify_related(&key);
        }
    }
    drop(tx);
    let _ = timeout(Duration::from_secs(1), writer_task).await;
    result
}

async fn handle(
    hub: &SharedHub,
    socket: &UdpSocket,
    key: PublicKey,
    ip: IpAddr,
    message: ClientMessage,
    pace: &mut Pace,
) -> Option<ServerMessage> {
    let now = Instant::now();
    let ack = |result: Result<(), ErrorCode>| result.map(|()| ServerKind::Ack(Empty {}));
    let result = match message.kind {
        Some(ClientKind::CreateNetwork(request)) => ack(create(hub, key, request).await),
        Some(ClientKind::JoinNetwork(request)) => join(hub, key, ip, request).await,
        Some(ClientKind::LeaveNetwork(request)) => ack(leave(hub, key, request)),
        Some(ClientKind::Ping(_)) => ack(Ok(())),
        Some(ClientKind::CreateInvite(request)) => create_invite(&mut lock(hub), key, request),
        Some(ClientKind::ListInvites(request)) => list_invites(&mut lock(hub), key, request),
        Some(ClientKind::RevokeInvite(request)) => ack(revoke_invite(&mut lock(hub), key, request)),
        Some(ClientKind::RedeemInvite(request)) => redeem_invite(&mut lock(hub), key, ip, request),
        Some(ClientKind::Kick(request)) => ack(remove(&mut lock(hub), key, request, false)),
        Some(ClientKind::Ban(request)) => ack(remove(&mut lock(hub), key, request, true)),
        Some(ClientKind::Unban(request)) => ack(unban(&mut lock(hub), key, request)),
        Some(ClientKind::ListBans(request)) => list_bans(&lock(hub), key, request),
        Some(ClientKind::ListRequests(request)) => list_requests(&lock(hub), key, request),
        Some(ClientKind::Approve(request)) => ack(approve(&mut lock(hub), key, request)),
        Some(ClientKind::Deny(request)) => ack(deny(&mut lock(hub), key, request)),
        Some(ClientKind::SetRole(request)) => ack(set_role(&mut lock(hub), key, request)),
        Some(ClientKind::UpdateNetwork(request)) => ack(update_network(hub, key, request).await),
        Some(ClientKind::DeleteNetwork(request)) => ack(delete_network(&mut lock(hub), key, request)),
        Some(ClientKind::Mute(request)) => ack(mute(&mut lock(hub), key, request)),
        Some(ClientKind::Refresh(_)) if !Pace::spaced(&mut pace.refreshed, REFRESH_EVERY, now) => {
            Err(ErrorCode::RateLimited)
        }
        Some(ClientKind::Refresh(_)) => lock(hub).state_for(&key).map(ServerKind::State).map_err(internal),
        Some(ClientKind::SetAvatar(_)) if pace.pictured.is_some_and(|last| now.duration_since(last) < AVATAR_EVERY) => {
            Err(ErrorCode::RateLimited)
        }
        Some(ClientKind::SetAvatar(avatar)) => {
            let result = set_avatar(&mut lock(hub), key, avatar);
            if result.is_ok() {
                pace.pictured = Some(now);
            }
            ack(result)
        }
        Some(ClientKind::GetAvatar(target)) => get_avatar(&lock(hub), key, target),
        Some(ClientKind::Candidates(candidates)) => {
            set_candidates(hub, key, candidates);
            return None;
        }
        Some(ClientKind::CallMeMaybe(target)) => {
            call_me_maybe(hub, key, target);
            return None;
        }
        Some(ClientKind::Relay(packet)) => {
            relay(hub, socket, key, packet).await;
            return None;
        }
        Some(ClientKind::Hello(_)) | None => Err(ErrorCode::InvalidRequest),
    };
    Some(match result {
        Ok(kind) => ServerMessage::reply(message.id, kind),
        Err(code) => ServerMessage::failure(message.id, code),
    })
}

fn set_candidates(hub: &SharedHub, key: PublicKey, candidates: Candidates) {
    let candidates: Vec<SocketAddr> =
        candidates.endpoints.iter().filter_map(|endpoint| endpoint.to_socket_addr()).take(MAX_CANDIDATES).collect();
    let mut hub = lock(hub);
    if hub.set_candidates(&key, candidates) {
        hub.notify_related(&key);
    }
}

fn call_me_maybe(hub: &SharedHub, key: PublicKey, target: PeerKey) {
    if let Ok(target) = PublicKey::from_slice(&target.key) {
        lock(hub).call_me_maybe(&key, &target);
    }
}

async fn relay(hub: &SharedHub, socket: &UdpSocket, key: PublicKey, packet: RelayPacket) {
    if packet.packet.len() < MIN_PACKET_LEN {
        return;
    }
    let route = lock(hub).route(&key, Ipv4Addr::from(packet.address), packet.packet.len(), Instant::now());
    if let Some(route) = route {
        udp::forward(socket, route, &packet.packet).await;
    }
}

async fn create(hub: &SharedHub, key: PublicKey, request: NetworkCredentials) -> Result<(), ErrorCode> {
    let name = validate::network_name(&request.name).ok_or(ErrorCode::InvalidName)?;
    if !validate::password(&request.password) {
        return Err(ErrorCode::InvalidPassword);
    }
    {
        let hub = lock(hub);
        if hub.db.network_by_name(&name).map_err(internal)?.is_some() {
            return Err(ErrorCode::NetworkExists);
        }
        if hub.db.owned_count(&key).map_err(internal)? >= MAX_OWNED {
            return Err(ErrorCode::TooManyNetworks);
        }
    }
    let hash = crate::hashing::run(move || {
        Argon2::default().hash_password(request.password.as_bytes()).map(|hash| hash.to_string())
    })
    .await
    .ok_or(ErrorCode::Internal)?
    .map_err(internal)?;
    let mut hub = lock(hub);
    match hub.db.create_network(&name, &hash, &key) {
        Ok(_) => hub.memberships_changed(),
        Err(DbError::NetworkExists) => return Err(ErrorCode::NetworkExists),
        Err(error) => return Err(internal(error)),
    }
    hub.notify_related(&key);
    Ok(())
}

async fn join(
    hub: &SharedHub,
    key: PublicKey,
    ip: IpAddr,
    request: NetworkCredentials,
) -> Result<ServerKind, ErrorCode> {
    let name = validate::network_name(&request.name).ok_or(ErrorCode::InvalidName)?;
    if !validate::password(&request.password) {
        return Err(ErrorCode::WrongPassword);
    }
    let limit_key = name_key(&name);
    let network = {
        let mut hub = lock(hub);
        let now = Instant::now();
        if !hub.limiter.allowed(ip, &limit_key, now) {
            return Err(ErrorCode::RateLimited);
        }
        let Some(network) = hub.db.network_by_name(&name).map_err(internal)? else {
            hub.limiter.failed(ip, &limit_key, now);
            return Err(ErrorCode::NetworkNotFound);
        };
        admissible(&hub, &network, &key)?;
        network
    };
    let hash = network.password_hash.clone();
    let valid = crate::hashing::run(move || {
        Argon2::default().verify_password(request.password.as_bytes(), hash.as_str()).is_ok()
    })
    .await
    .ok_or(ErrorCode::Internal)?;

    let mut hub = lock(hub);
    if !valid {
        hub.limiter.failed(ip, &limit_key, Instant::now());
        return Err(ErrorCode::WrongPassword);
    }
    let network = hub
        .db
        .network_by_name(&name)
        .map_err(internal)?
        .filter(|current| current.id == network.id)
        .ok_or(ErrorCode::NetworkNotFound)?;
    Ok(if admit(&mut hub, &network, &key)? {
        ServerKind::Pending(NetworkName { name: network.name })
    } else {
        ServerKind::Ack(Empty {})
    })
}

fn admissible(hub: &Hub, network: &NetworkRow, key: &PublicKey) -> Result<(), ErrorCode> {
    if hub.db.is_member(network.id, key).map_err(internal)? {
        return Err(ErrorCode::AlreadyMember);
    }
    if hub.db.is_banned(network.id, key).map_err(internal)? {
        return Err(ErrorCode::Banned);
    }
    if network.locked {
        return Err(ErrorCode::NetworkLocked);
    }
    if !network.approval && hub.db.member_count(network.id).map_err(internal)? >= hub.config.max_members {
        return Err(ErrorCode::NetworkFull);
    }
    Ok(())
}

/// Returns true when the device has to wait for approval.
fn admit(hub: &mut Hub, network: &NetworkRow, key: &PublicKey) -> Result<bool, ErrorCode> {
    admissible(hub, network, key)?;
    if network.approval {
        hub.db.add_request(network.id, key).map_err(internal)?;
        let admins: BTreeSet<PublicKey> = hub
            .db
            .members(network.id)
            .map_err(internal)?
            .into_iter()
            .filter(|(_, role)| *role >= Role::Admin)
            .map(|(device, _)| device.key)
            .collect();
        hub.notify(&admins);
        return Ok(true);
    }
    hub.db.add_member(network.id, key, Role::Member).map_err(internal)?;
    hub.memberships_changed();
    hub.notify_related(key);
    Ok(false)
}

fn leave(hub: &SharedHub, key: PublicKey, request: NetworkName) -> Result<(), ErrorCode> {
    let mut hub = lock(hub);
    let network = hub.db.network_by_name(&request.name).map_err(internal)?.ok_or(ErrorCode::NetworkNotFound)?;
    let before = hub.db.related(&key).map_err(internal)?;
    if !hub.db.remove_member(network.id, &key).map_err(internal)? {
        return Err(ErrorCode::NotMember);
    }
    hub.memberships_changed();
    hub.notify(&before);
    Ok(())
}

fn create_invite(hub: &mut Hub, key: PublicKey, request: InviteRequest) -> Result<ServerKind, ErrorCode> {
    if request.expires_in > MAX_INVITE_LIFETIME {
        return Err(ErrorCode::InvalidRequest);
    }
    let network = manage(hub, &request.network, &key)?.0;
    if hub.db.invites(network.id).map_err(internal)?.len() >= MAX_INVITES {
        return Err(ErrorCode::TooManyInvites);
    }
    let expires = (request.expires_in > 0).then(|| unix_now() + request.expires_in as i64);
    for _ in 0..4 {
        let code = invite_code();
        match hub.db.create_invite(&code, network.id, &key, request.max_uses, expires) {
            Ok(()) => {
                let invite = hub.db.invite(&code).map_err(internal)?.ok_or(ErrorCode::Internal)?;
                return Ok(ServerKind::Invite(invite_message(invite)));
            }
            Err(DbError::InviteExists) => continue,
            Err(error) => return Err(internal(error)),
        }
    }
    Err(ErrorCode::Internal)
}

fn list_invites(hub: &mut Hub, key: PublicKey, request: NetworkName) -> Result<ServerKind, ErrorCode> {
    let network = manage(hub, &request.name, &key)?.0;
    let invites = hub.db.invites(network.id).map_err(internal)?.into_iter().map(invite_message).collect();
    Ok(ServerKind::Invites(InviteList { invites }))
}

fn revoke_invite(hub: &mut Hub, key: PublicKey, request: InviteCode) -> Result<(), ErrorCode> {
    let invite = find_invite(hub, &request.code)?.ok_or(ErrorCode::InviteNotFound)?;
    manage(hub, &invite.network_name, &key)?;
    hub.db.revoke_invite(&invite.code).map_err(internal)?;
    Ok(())
}

fn redeem_invite(hub: &mut Hub, key: PublicKey, ip: IpAddr, request: InviteCode) -> Result<ServerKind, ErrorCode> {
    let now = Instant::now();
    if !hub.limiter.ip_allowed(ip, now) {
        return Err(ErrorCode::RateLimited);
    }
    let Some(invite) = find_invite(hub, &request.code)? else {
        hub.limiter.ip_failed(ip, now);
        return Err(ErrorCode::InviteNotFound);
    };
    let network = hub.db.network_by_name(&invite.network_name).map_err(internal)?.ok_or(ErrorCode::InviteNotFound)?;
    let pending = admit(hub, &network, &key)?;
    hub.db.use_invite(&invite.code).map_err(internal)?;
    let name = NetworkName { name: network.name };
    Ok(if pending { ServerKind::Pending(name) } else { ServerKind::Joined(name) })
}

fn remove(hub: &mut Hub, key: PublicKey, request: MemberAction, ban: bool) -> Result<(), ErrorCode> {
    let (network, role) = manage(hub, &request.network, &key)?;
    let members = hub.db.members(network.id).map_err(internal)?;
    let index = resolve(members.iter().map(|(device, _)| device), &request.member)?;
    let (target, target_role) = &members[index];
    if *target_role >= role {
        return Err(ErrorCode::Forbidden);
    }
    let before = hub.db.related(&target.key).map_err(internal)?;
    if ban {
        hub.db.ban(network.id, &target.key).map_err(internal)?;
    } else {
        hub.db.remove_member(network.id, &target.key).map_err(internal)?;
    }
    tracing::info!(network = network.name, target = ?target.key, by = ?key, ban, "member removed");
    hub.memberships_changed();
    hub.notify(&before);
    Ok(())
}

fn unban(hub: &mut Hub, key: PublicKey, request: MemberAction) -> Result<(), ErrorCode> {
    let network = manage(hub, &request.network, &key)?.0;
    let bans = hub.db.bans(network.id).map_err(internal)?;
    let index = resolve(bans.iter(), &request.member)?;
    hub.db.unban(network.id, &bans[index].key).map_err(internal)?;
    Ok(())
}

fn list_bans(hub: &Hub, key: PublicKey, request: NetworkName) -> Result<ServerKind, ErrorCode> {
    let network = manage(hub, &request.name, &key)?.0;
    let bans = hub.db.bans(network.id).map_err(internal)?.into_iter().map(device_info).collect();
    Ok(ServerKind::Bans(DeviceList { devices: bans }))
}

fn list_requests(hub: &Hub, key: PublicKey, request: NetworkName) -> Result<ServerKind, ErrorCode> {
    let network = manage(hub, &request.name, &key)?.0;
    let devices = hub.db.requests(network.id).map_err(internal)?.into_iter().map(device_info).collect();
    Ok(ServerKind::Requests(DeviceList { devices }))
}

fn approve(hub: &mut Hub, key: PublicKey, request: MemberAction) -> Result<(), ErrorCode> {
    let network = manage(hub, &request.network, &key)?.0;
    let requests = hub.db.requests(network.id).map_err(internal)?;
    let target = &requests[resolve(requests.iter(), &request.member)?];
    if hub.db.member_count(network.id).map_err(internal)? >= hub.config.max_members {
        return Err(ErrorCode::NetworkFull);
    }
    hub.db.add_member(network.id, &target.key, Role::Member).map_err(internal)?;
    tracing::info!(network = network.name, target = ?target.key, by = ?key, "request approved");
    hub.memberships_changed();
    hub.notify_related(&target.key);
    Ok(())
}

fn deny(hub: &mut Hub, key: PublicKey, request: MemberAction) -> Result<(), ErrorCode> {
    let network = manage(hub, &request.network, &key)?.0;
    let requests = hub.db.requests(network.id).map_err(internal)?;
    let target = &requests[resolve(requests.iter(), &request.member)?];
    hub.db.remove_request(network.id, &target.key).map_err(internal)?;
    hub.notify_related(&key);
    Ok(())
}

fn set_role(hub: &mut Hub, key: PublicKey, request: RoleChange) -> Result<(), ErrorCode> {
    let role = Role::try_from(request.role).map_err(|_| ErrorCode::InvalidRequest)?;
    if role == Role::Owner {
        return Err(ErrorCode::InvalidRequest);
    }
    let (network, own) = manage(hub, &request.network, &key)?;
    if own != Role::Owner {
        return Err(ErrorCode::Forbidden);
    }
    let members = hub.db.members(network.id).map_err(internal)?;
    let (target, target_role) = &members[resolve(members.iter().map(|(device, _)| device), &request.member)?];
    if *target_role == Role::Owner {
        return Err(ErrorCode::Forbidden);
    }
    hub.db.set_role(network.id, &target.key, role).map_err(internal)?;
    hub.notify(&BTreeSet::from([target.key]));
    Ok(())
}

async fn update_network(hub: &SharedHub, key: PublicKey, request: NetworkSettings) -> Result<(), ErrorCode> {
    if request.locked.is_none() && request.approval.is_none() && request.password.is_none() && request.rename.is_none()
    {
        return Err(ErrorCode::InvalidRequest);
    }
    let rename = match &request.rename {
        Some(name) => Some(validate::network_name(name).ok_or(ErrorCode::InvalidName)?),
        None => None,
    };
    let network = {
        let hub = lock(hub);
        let (network, role) = manage(&hub, &request.name, &key)?;
        if (request.password.is_some() || rename.is_some()) && role != Role::Owner {
            return Err(ErrorCode::Forbidden);
        }
        network
    };
    let hash = match request.password {
        Some(password) if !validate::password(&password) => return Err(ErrorCode::InvalidPassword),
        Some(password) => Some(
            crate::hashing::run(move || {
                Argon2::default().hash_password(password.as_bytes()).map(|hash| hash.to_string())
            })
            .await
            .ok_or(ErrorCode::Internal)?
            .map_err(internal)?,
        ),
        None => None,
    };
    let mut hub = lock(hub);
    if let Some(name) = &rename {
        hub.db.rename_network(network.id, name).map_err(|error| match error {
            DbError::NetworkExists => ErrorCode::NetworkExists,
            error => internal(error),
        })?;
        tracing::info!(network = network.name, to = name, by = ?key, "network renamed");
    }
    hub.db.update_network(network.id, request.locked, request.approval, hash.as_deref()).map_err(internal)?;
    tracing::info!(network = network.name, by = ?key, "settings changed");
    hub.notify_related(&key);
    Ok(())
}

fn mute(hub: &mut Hub, key: PublicKey, request: MuteRequest) -> Result<(), ErrorCode> {
    let (network, role) = manage(hub, &request.network, &key)?;
    let members = hub.db.members(network.id).map_err(internal)?;
    let (target, target_role) = &members[resolve(members.iter().map(|(device, _)| device), &request.member)?];
    if *target_role >= role {
        return Err(ErrorCode::Forbidden);
    }
    let until = match request.seconds {
        0 => 0,
        MUTE_FOREVER => MUTED_FOREVER,
        seconds => unix_now().saturating_add(seconds.min(MAX_MUTE) as i64),
    };
    hub.db.set_muted(network.id, &target.key, until).map_err(internal)?;
    tracing::info!(network = network.name, target = ?target.key, by = ?key, until, "member muted");
    hub.memberships_changed();
    let members = hub.db.member_keys(network.id).map_err(internal)?;
    hub.notify(&members);
    Ok(())
}

fn set_avatar(hub: &mut Hub, key: PublicKey, avatar: Avatar) -> Result<(), ErrorCode> {
    let image = avatar.image;
    let picture = image.starts_with(b"\x89PNG\r\n\x1a\n") || image.starts_with(&[0xff, 0xd8, 0xff]);
    if image.len() > MAX_AVATAR_LEN || !(image.is_empty() || picture) {
        return Err(ErrorCode::InvalidRequest);
    }
    let id = avatar_id(&image);
    let stored = (!image.is_empty()).then_some((image.as_slice(), id.as_str()));
    hub.db.set_avatar(&key, stored).map_err(internal)?;
    hub.notify_related(&key);
    Ok(())
}

fn get_avatar(hub: &Hub, key: PublicKey, target: PeerKey) -> Result<ServerKind, ErrorCode> {
    let target = PublicKey::from_slice(&target.key).map_err(|_| ErrorCode::InvalidRequest)?;
    if !hub.db.related(&key).map_err(internal)?.contains(&target) {
        return Err(ErrorCode::MemberNotFound);
    }
    let image = hub.db.avatar(&target).map_err(internal)?.ok_or(ErrorCode::MemberNotFound)?;
    Ok(ServerKind::Avatar(Avatar { key: target.as_bytes().to_vec(), image }))
}

fn delete_network(hub: &mut Hub, key: PublicKey, request: NetworkName) -> Result<(), ErrorCode> {
    let (network, role) = manage(hub, &request.name, &key)?;
    if role != Role::Owner {
        return Err(ErrorCode::Forbidden);
    }
    let before = hub.db.related(&key).map_err(internal)?;
    hub.db.delete_network(network.id).map_err(internal)?;
    tracing::info!(network = network.name, by = ?key, "network deleted");
    hub.memberships_changed();
    hub.notify(&before);
    Ok(())
}

fn device_info(device: Device) -> DeviceInfo {
    DeviceInfo { key: device.key.as_bytes().to_vec(), nickname: device.nickname, address: u32::from(device.address) }
}

fn manage(hub: &Hub, name: &str, key: &PublicKey) -> Result<(NetworkRow, Role), ErrorCode> {
    let network = hub.db.network_by_name(name).map_err(internal)?.ok_or(ErrorCode::NetworkNotFound)?;
    match hub.db.role(network.id, key).map_err(internal)? {
        None => Err(ErrorCode::NotMember),
        Some(Role::Member) => Err(ErrorCode::Forbidden),
        Some(role) => Ok((network, role)),
    }
}

fn find_invite(hub: &mut Hub, code: &str) -> Result<Option<InviteRow>, ErrorCode> {
    let code = code.trim().to_ascii_uppercase();
    if code.is_empty() || code.len() > MAX_INVITE_LEN {
        return Ok(None);
    }
    hub.db.invite(&code).map_err(internal)
}

pub(crate) fn resolve<'a>(devices: impl Iterator<Item = &'a Device>, query: &str) -> Result<usize, ErrorCode> {
    let query = query.trim();
    let matches: Vec<usize> = if let Ok(key) = query.parse::<PublicKey>() {
        devices.enumerate().filter(|(_, device)| device.key == key).map(|(index, _)| index).collect()
    } else if let Ok(address) = query.parse::<Ipv4Addr>() {
        devices.enumerate().filter(|(_, device)| device.address == address).map(|(index, _)| index).collect()
    } else {
        let query = query.to_lowercase();
        devices
            .enumerate()
            .filter(|(_, device)| device.nickname.to_lowercase() == query)
            .map(|(index, _)| index)
            .collect()
    };
    match matches[..] {
        [] => Err(ErrorCode::MemberNotFound),
        [index] => Ok(index),
        _ => Err(ErrorCode::AmbiguousMember),
    }
}

fn invite_code() -> String {
    let mut rng = rand::rng();
    let chars: Vec<char> =
        (0..12).map(|_| char::from(INVITE_ALPHABET[rng.random_range(0..INVITE_ALPHABET.len())])).collect();
    chars.chunks(4).map(|chunk| chunk.iter().collect::<String>()).collect::<Vec<_>>().join("-")
}

fn invite_message(invite: InviteRow) -> Invite {
    Invite {
        code: invite.code,
        network: invite.network_name,
        max_uses: invite.max_uses,
        uses: invite.uses,
        expires: invite.expires.map_or(0, |expires| expires.max(0) as u64),
        creator: invite.creator,
    }
}

fn internal(error: impl std::fmt::Display) -> ErrorCode {
    tracing::warn!(%error, "request failed");
    ErrorCode::Internal
}
