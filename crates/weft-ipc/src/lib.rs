use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const SOCKET_ENV: &str = "WEFT_SOCKET";
pub const DEFAULT_SOCKET: &str = "/run/weft/weftd.sock";
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
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "result", content = "data", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Status(Status),
    Error(Failure),
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
    Internal,
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
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerLink {
    Offline,
    Connecting,
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

#[cfg(unix)]
pub async fn request(path: &Path, request: &Request) -> io::Result<Response> {
    let stream = tokio::net::UnixStream::connect(path).await?;
    let (reader, mut writer) = stream.into_split();
    send(&mut writer, request).await?;
    receive(&mut tokio::io::BufReader::new(reader)).await?.ok_or_else(|| io::ErrorKind::UnexpectedEof.into())
}

#[cfg(not(unix))]
pub async fn request(_path: &Path, _request: &Request) -> io::Result<Response> {
    Err(io::ErrorKind::Unsupported.into())
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
