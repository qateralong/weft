use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use weft_proto::control::{
    self, ClientKind, ClientMessage, DecodeError, Empty, ErrorCode, NetworkCredentials, NetworkName, PROTOCOL_VERSION,
    Role, ServerKind, ServerMessage, Welcome,
};
use weft_proto::{ObfsKey, PublicKey};
use weft_session::StaticKeypair;
use weft_session::stream::{self, io};

use crate::db::{DbError, name_key};
use crate::hub::{SharedHub, lock};
use crate::validate;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

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

pub async fn serve(listener: TcpListener, hub: SharedHub, keypair: Arc<StaticKeypair>, udp_port: u16) {
    let mut next_conn = 0;
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
        let (hub, keypair, conn) = (hub.clone(), keypair.clone(), next_conn);
        tokio::spawn(async move {
            if let Err(error) = connection(stream, addr, hub, keypair, udp_port, conn).await {
                tracing::debug!(%addr, %error, "connection closed");
            }
        });
    }
}

async fn connection(
    stream: TcpStream,
    addr: SocketAddr,
    hub: SharedHub,
    keypair: Arc<StaticKeypair>,
    udp_port: u16,
    conn: u64,
) -> Result<(), ConnError> {
    stream.set_nodelay(true)?;
    let (mut reader, mut writer) = stream.into_split();
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
        match hub.db.upsert_device(&key, nickname.as_deref().unwrap_or_default(), &pool) {
            Ok(device) => {
                let token = hub.register(key, conn, tx.clone(), kill_tx);
                ServerMessage::reply(
                    hello.id,
                    ServerKind::Welcome(Welcome {
                        address: u32::from(device.address),
                        prefix_len: u32::from(pool.prefix()),
                        discovery_token: token.to_vec(),
                        udp_port: u32::from(udp_port),
                    }),
                )
            }
            Err(DbError::PoolExhausted) => refuse(ErrorCode::PoolExhausted),
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
        let reply = handle(&hub, key, addr.ip(), message).await;
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

async fn handle(hub: &SharedHub, key: PublicKey, ip: IpAddr, message: ClientMessage) -> ServerMessage {
    let result = match message.kind {
        Some(ClientKind::CreateNetwork(request)) => create(hub, key, request).await,
        Some(ClientKind::JoinNetwork(request)) => join(hub, key, ip, request).await,
        Some(ClientKind::LeaveNetwork(request)) => leave(hub, key, request),
        Some(ClientKind::Ping(_)) => Ok(()),
        Some(ClientKind::Hello(_)) | None => Err(ErrorCode::InvalidRequest),
    };
    match result {
        Ok(()) => ServerMessage::reply(message.id, ServerKind::Ack(Empty {})),
        Err(code) => ServerMessage::failure(message.id, code),
    }
}

async fn create(hub: &SharedHub, key: PublicKey, request: NetworkCredentials) -> Result<(), ErrorCode> {
    let name = validate::network_name(&request.name).ok_or(ErrorCode::InvalidName)?;
    if !validate::password(&request.password) {
        return Err(ErrorCode::InvalidPassword);
    }
    if lock(hub).db.network_by_name(&name).map_err(internal)?.is_some() {
        return Err(ErrorCode::NetworkExists);
    }
    let hash = tokio::task::spawn_blocking(move || {
        Argon2::default().hash_password(request.password.as_bytes()).map(|hash| hash.to_string())
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    let mut hub = lock(hub);
    match hub.db.create_network(&name, &hash, &key) {
        Ok(_) => {}
        Err(DbError::NetworkExists) => return Err(ErrorCode::NetworkExists),
        Err(error) => return Err(internal(error)),
    }
    hub.notify_related(&key);
    Ok(())
}

async fn join(hub: &SharedHub, key: PublicKey, ip: IpAddr, request: NetworkCredentials) -> Result<(), ErrorCode> {
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
        if hub.db.is_member(network.id, &key).map_err(internal)? {
            return Err(ErrorCode::AlreadyMember);
        }
        if hub.db.member_count(network.id).map_err(internal)? >= hub.config.max_members {
            return Err(ErrorCode::NetworkFull);
        }
        network
    };
    let hash = network.password_hash.clone();
    let valid = tokio::task::spawn_blocking(move || {
        Argon2::default().verify_password(request.password.as_bytes(), hash.as_str()).is_ok()
    })
    .await
    .map_err(internal)?;

    let mut hub = lock(hub);
    if !valid {
        hub.limiter.failed(ip, &limit_key, Instant::now());
        return Err(ErrorCode::WrongPassword);
    }
    if hub.db.member_count(network.id).map_err(internal)? >= hub.config.max_members {
        return Err(ErrorCode::NetworkFull);
    }
    hub.db.add_member(network.id, &key, Role::Member).map_err(internal)?;
    hub.notify_related(&key);
    Ok(())
}

fn leave(hub: &SharedHub, key: PublicKey, request: NetworkName) -> Result<(), ErrorCode> {
    let mut hub = lock(hub);
    let network = hub.db.network_by_name(&request.name).map_err(internal)?.ok_or(ErrorCode::NetworkNotFound)?;
    let before = hub.db.related(&key).map_err(internal)?;
    if !hub.db.remove_member(network.id, &key).map_err(internal)? {
        return Err(ErrorCode::NotMember);
    }
    hub.notify(&before);
    Ok(())
}

fn internal(error: impl std::fmt::Display) -> ErrorCode {
    tracing::warn!(%error, "request failed");
    ErrorCode::Internal
}
