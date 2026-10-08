use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const SOCKET_ENV: &str = "WEFT_SOCKET";
#[cfg(target_os = "macos")]
pub const DEFAULT_SOCKET: &str = "/var/run/weft/weftd.sock";
#[cfg(all(unix, not(target_os = "macos")))]
pub const DEFAULT_SOCKET: &str = "/run/weft/weftd.sock";
#[cfg(windows)]
pub const DEFAULT_SOCKET: &str = r"\\.\pipe\weftd";
const MAX_LINE: u64 = 1 << 20;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    Up { link: Option<String>, nickname: Option<String> },
    Down,
    Create { name: String, password: String },
    Join { name: String, password: String },
    Leave { name: String },
    Status,
    Redeem { link: String },
    CreateInvite { network: String, uses: Option<u32>, expires_in: Option<u64> },
    Invites { network: String },
    RevokeInvite { code: String },
    Kick { network: String, member: String },
    Ban { network: String, member: String },
    Unban { network: String, member: String },
    Bans { network: String },
    Requests { network: String },
    Approve { network: String, member: String },
    Deny { network: String, member: String },
    SetRole { network: String, member: String, role: Role },
    Configure { network: String, locked: Option<bool>, approval: Option<bool>, password: Option<String> },
    Delete { network: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "result", content = "data", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Status(Status),
    Invite(InviteInfo),
    Invites(Vec<InviteInfo>),
    Joined(String),
    Pending(String),
    Bans(Vec<DeviceInfo>),
    Requests(Vec<DeviceInfo>),
    Error(Failure),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct InviteInfo {
    pub code: String,
    pub link: Option<String>,
    pub network: String,
    pub max_uses: Option<u32>,
    pub uses: u32,
    pub expires: Option<u64>,
    pub creator: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub nickname: String,
    pub address: Ipv4Addr,
    pub public_key: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    NoServer,
    InvalidLink,
    NotConnected,
    Timeout,
    UnsupportedVersion,
    InvalidRequest,
    InvalidName,
    InvalidNickname,
    InvalidPassword,
    NetworkExists,
    NetworkNotFound,
    WrongPassword,
    NetworkFull,
    RateLimited,
    AlreadyMember,
    NotMember,
    PoolExhausted,
    Forbidden,
    InviteNotFound,
    Banned,
    MemberNotFound,
    AmbiguousMember,
    TooManyInvites,
    NetworkLocked,
    Internal,
}

impl Failure {
    pub fn message_id(self) -> &'static str {
        match self {
            Failure::NoServer => "error-no-server",
            Failure::InvalidLink => "error-invalid-link",
            Failure::NotConnected => "error-not-connected",
            Failure::Timeout => "error-timeout",
            Failure::UnsupportedVersion => "error-unsupported-version",
            Failure::InvalidRequest => "error-invalid-request",
            Failure::InvalidName => "error-invalid-name",
            Failure::InvalidNickname => "error-invalid-nickname",
            Failure::InvalidPassword => "error-invalid-password",
            Failure::NetworkExists => "error-network-exists",
            Failure::NetworkNotFound => "error-network-not-found",
            Failure::WrongPassword => "error-wrong-password",
            Failure::NetworkFull => "error-network-full",
            Failure::RateLimited => "error-rate-limited",
            Failure::AlreadyMember => "error-already-member",
            Failure::NotMember => "error-not-member",
            Failure::PoolExhausted => "error-pool-exhausted",
            Failure::Forbidden => "error-forbidden",
            Failure::InviteNotFound => "error-invite-not-found",
            Failure::Banned => "error-banned",
            Failure::MemberNotFound => "error-member-not-found",
            Failure::AmbiguousMember => "error-ambiguous-member",
            Failure::TooManyInvites => "error-too-many-invites",
            Failure::NetworkLocked => "error-network-locked",
            Failure::Internal => "error-internal",
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub connection: Connection,
    pub server: Option<String>,
    pub nickname: String,
    pub public_key: String,
    pub address: Option<Ipv4Addr>,
    pub networks: Vec<NetworkStatus>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Connection {
    Disconnected,
    Connecting,
    Connected,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct NetworkStatus {
    pub name: String,
    pub role: Role,
    pub locked: bool,
    pub approval: bool,
    pub requests: u32,
    pub members: Vec<MemberStatus>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Admin,
    Member,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MemberStatus {
    pub nickname: String,
    pub address: Ipv4Addr,
    pub link: PeerLink,
    pub latency_ms: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerLink {
    Offline,
    Connecting,
    Relay,
    Direct,
}

pub fn socket_path() -> PathBuf {
    std::env::var_os(SOCKET_ENV).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET))
}

pub async fn send<T: Serialize, W: AsyncWrite + Unpin>(writer: &mut W, message: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(message).map_err(io::Error::other)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

pub async fn receive<T: DeserializeOwned, R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<T>> {
    let mut line = String::new();
    if reader.take(MAX_LINE).read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    serde_json::from_str(&line).map(Some).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub async fn request(path: &Path, request: &Request) -> io::Result<Response> {
    let (reader, mut writer) = tokio::io::split(connect(path).await?);
    send(&mut writer, request).await?;
    receive(&mut tokio::io::BufReader::new(reader)).await?.ok_or_else(|| io::ErrorKind::UnexpectedEof.into())
}

#[cfg(unix)]
async fn connect(path: &Path) -> io::Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(path).await
}

#[cfg(windows)]
async fn connect(path: &Path) -> io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    const ERROR_PIPE_BUSY: i32 = 231;
    for _ in 0..50 {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(path) {
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            result => return result,
        }
    }
    Err(io::ErrorKind::TimedOut.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        let request = Request::Join { name: "lan".into(), password: "pw".into() };
        let text = serde_json::to_string(&request).unwrap();
        assert_eq!(text, r#"{"command":"join","name":"lan","password":"pw"}"#);
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), request);
        assert_eq!(serde_json::to_string(&Request::Status).unwrap(), r#"{"command":"status"}"#);
        assert_eq!(
            serde_json::to_string(&Response::Error(Failure::WrongPassword)).unwrap(),
            r#"{"result":"error","data":"wrong_password"}"#
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn request_over_socket() {
        use tokio::io::BufReader;

        let dir = std::env::temp_dir().join(format!("weft-ipc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.sock");
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let request: Request = receive(&mut BufReader::new(reader)).await.unwrap().unwrap();
            assert_eq!(request, Request::Down);
            send(&mut writer, &Response::Ok).await.unwrap();
        });
        assert_eq!(super::request(&path, &Request::Down).await.unwrap(), Response::Ok);
        server.await.unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
