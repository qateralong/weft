//! A web panel for the server's administrator, served over the same TLS port at a secret path.
//! Everything else on that port keeps looking like nginx.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;
use weft_i18n::{Language, Localizer};

use crate::admin::{self, AdminRequest, AdminResponse};
use crate::db::unix_now;
use crate::hub::{SharedHub, lock};
use crate::tls::http_reply;

const PAGE: &str = include_str!("panel.html");
const SECRET_FILE: &str = "panel-path";
const HASH_FILE: &str = "panel.hash";
const COOKIE: &str = "weft_panel";
const SESSION_LIFETIME: Duration = Duration::from_secs(12 * 3600);
const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 64 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const LOGIN_FAILURES: u32 = 5;
const LOGIN_WINDOW: Duration = Duration::from_secs(60);
pub const SAMPLE_EVERY: Duration = Duration::from_secs(10);
const SAMPLES: usize = 360;
const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";

/// One point of the traffic history.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Sample {
    pub time: i64,
    pub bytes: u64,
    pub online: usize,
}

pub struct Panel {
    dir: PathBuf,
    sessions: Mutex<HashMap<String, Instant>>,
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
    history: Mutex<VecDeque<Sample>>,
}

fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn random(len: usize) -> String {
    let mut rng = rand::rng();
    (0..len).map(|_| char::from(ALPHABET[rng.random_range(0..ALPHABET.len())])).collect()
}

/// The secret first path segment of the panel, made on first use.
pub fn secret(dir: &Path) -> io::Result<String> {
    let path = dir.join(SECRET_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => Ok(text.trim().to_string()),
        _ => {
            let secret = random(20);
            write_private(&path, &secret)?;
            Ok(secret)
        }
    }
}

/// Stores the administrator password as an Argon2 hash.
pub fn set_password(dir: &Path, password: &str) -> io::Result<()> {
    let hash = Argon2::default().hash_password(password.as_bytes()).map_err(io::Error::other)?.to_string();
    write_private(&dir.join(HASH_FILE), &hash)
}

pub fn generate_password() -> String {
    random(16)
}

pub fn has_password(dir: &Path) -> bool {
    dir.join(HASH_FILE).exists()
}

/// Where the panel opens, like `https://example.com:443/secret/`.
pub fn url(host: &str, port: u16, secret: &str) -> String {
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
    format!("https://{host}:{port}/{secret}/")
}

fn write_private(path: &Path, text: &str) -> io::Result<()> {
    std::fs::write(path, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

impl Panel {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            sessions: Mutex::default(),
            failures: Mutex::default(),
            history: Mutex::new(VecDeque::with_capacity(SAMPLES)),
        }
    }

    /// Records relayed bytes and online devices every few seconds for the charts.
    pub async fn sample(self: Arc<Self>, hub: SharedHub) {
        let mut tick = tokio::time::interval(SAMPLE_EVERY);
        loop {
            tick.tick().await;
            let sample = {
                let hub = lock(&hub);
                Sample { time: unix_now(), bytes: hub.relay_total().bytes, online: hub.online_count() }
            };
            let mut history = guard(&self.history);
            if history.len() == SAMPLES {
                history.pop_front();
            }
            history.push_back(sample);
        }
    }

    fn signed_in(&self, request: &Request) -> bool {
        let Some(token) = request.cookie(COOKIE) else { return false };
        let mut sessions = guard(&self.sessions);
        sessions.retain(|_, expires| *expires > Instant::now());
        sessions.contains_key(token)
    }

    /// Whether `ip` may try a password now; too many failures in a minute lock it out for a while.
    fn may_try(&self, ip: IpAddr) -> bool {
        let failures = guard(&self.failures);
        failures.get(&ip).is_none_or(|(count, since)| *count < LOGIN_FAILURES || since.elapsed() > LOGIN_WINDOW)
    }

    fn failed(&self, ip: IpAddr) {
        let mut failures = guard(&self.failures);
        let entry = failures.entry(ip).or_insert((0, Instant::now()));
        if entry.1.elapsed() > LOGIN_WINDOW {
            *entry = (0, Instant::now());
        }
        entry.0 += 1;
    }

    async fn check(&self, ip: IpAddr, password: String) -> Result<(), Reply> {
        if !self.may_try(ip) {
            return Err(Reply::error(429, "rate-limited"));
        }
        let Ok(hash) = std::fs::read_to_string(self.dir.join(HASH_FILE)) else {
            return Err(Reply::error(503, "no-password"));
        };
        let valid = tokio::task::spawn_blocking(move || {
            Argon2::default().verify_password(password.as_bytes(), hash.trim()).is_ok()
        })
        .await
        .unwrap_or(false);
        if valid {
            guard(&self.failures).remove(&ip);
            Ok(())
        } else {
            self.failed(ip);
            Err(Reply::error(401, "wrong-password"))
        }
    }
}

/// Serves one HTTP request that arrived inside TLS; `start` holds the bytes already read.
pub async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    start: [u8; 4],
    ip: IpAddr,
    hub: SharedHub,
    panel: Arc<Panel>,
) -> io::Result<()> {
    let reply = match timeout(READ_TIMEOUT, read_request(&mut stream, &start)).await {
        Ok(Ok(request)) => route(request, ip, &hub, &panel).await,
        _ => Reply::raw(http_reply("400 Bad Request", "")),
    };
    stream.write_all(&reply.into_bytes()).await?;
    stream.shutdown().await
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }

    fn cookie(&self, name: &str) -> Option<&str> {
        self.header("cookie")?.split(';').find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then_some(value)
        })
    }

    fn json<T: for<'a> Deserialize<'a>>(&self) -> Result<T, Reply> {
        serde_json::from_slice(&self.body).map_err(|_| Reply::error(400, "bad-request"))
    }

    fn language(&self) -> Language {
        self.header("accept-language")
            .into_iter()
            .flat_map(|value| value.split(','))
            .map(|tag| tag.trim().split(';').next().unwrap_or_default().to_ascii_lowercase())
            .find_map(|tag| Language::ALL.into_iter().find(|language| tag.starts_with(language.code())))
            .unwrap_or(Language::English)
    }
}

async fn read_request<S: AsyncRead + Unpin>(stream: &mut S, start: &[u8]) -> io::Result<Request> {
    let mut data = start.to_vec();
    let mut buf = [0; 4096];
    let head_end = loop {
        if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        if data.len() > MAX_HEAD {
            return Err(io::Error::other("request head too large"));
        }
        let read = stream.read(&mut buf).await?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        data.extend_from_slice(&buf[..read]);
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (method, path) = (first.next().unwrap_or_default().to_string(), first.next().unwrap_or_default().to_string());
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect();
    let length = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err(io::Error::other("request body too large"));
    }
    let mut body = data[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut buf).await?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        body.extend_from_slice(&buf[..read]);
    }
    body.truncate(length);
    Ok(Request { method, path, headers, body })
}

struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    headers: Vec<String>,
    raw: Option<Vec<u8>>,
}

impl Reply {
    fn raw(bytes: Vec<u8>) -> Self {
        Self { status: 0, content_type: "", body: Vec::new(), headers: Vec::new(), raw: Some(bytes) }
    }

    fn json(status: u16, value: &Value) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_default();
        Self { status, content_type: "application/json", body, headers: Vec::new(), raw: None }
    }

    fn error(status: u16, code: &str) -> Self {
        Self::json(status, &json!({ "error": code }))
    }

    fn ok() -> Self {
        Self::json(200, &json!({ "ok": true }))
    }

    fn into_bytes(self) -> Vec<u8> {
        if let Some(raw) = self.raw {
            return raw;
        }
        let reason = match self.status {
            200 => "OK",
            301 => "Moved Permanently",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            429 => "Too Many Requests",
            _ => "Service Unavailable",
        };
        let mut head = format!(
            "HTTP/1.1 {} {reason}\r\nServer: nginx\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\
             Cache-Control: no-store\r\nX-Frame-Options: DENY\r\nX-Content-Type-Options: nosniff\r\n\
             Referrer-Policy: no-referrer\r\n\
             Content-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
             connect-src 'self'; img-src data:; form-action 'none'; frame-ancestors 'none'\r\n",
            self.status,
            self.content_type,
            self.body.len()
        );
        for header in self.headers {
            head.push_str(&header);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

#[derive(Deserialize)]
struct Login {
    password: String,
}

#[derive(Deserialize)]
struct DeleteNetwork {
    name: String,
    password: String,
}

#[derive(Deserialize)]
struct Block {
    key: String,
    blocked: bool,
}

async fn route(request: Request, ip: IpAddr, hub: &SharedHub, panel: &Panel) -> Reply {
    let Ok(secret) = secret(&panel.dir) else { return Reply::raw(http_reply("404 Not Found", "")) };
    let prefix = format!("/{secret}");
    let Some(rest) = request.path.strip_prefix(&prefix) else {
        return Reply::raw(http_reply("404 Not Found", ""));
    };
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    if request.method == "POST" && request.header("x-weft") != Some("1") {
        return Reply::error(403, "forbidden");
    }
    match (request.method.as_str(), rest) {
        ("GET", "") => {
            let mut reply = Reply::json(301, &json!({}));
            reply.headers.push(format!("Location: {prefix}/"));
            reply
        }
        ("GET", "/") => Reply {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: PAGE.into(),
            headers: Vec::new(),
            raw: None,
        },
        ("GET", "/api/strings") => strings(request.language()),
        ("POST", "/api/login") => {
            let Ok(Login { password }) = request.json::<Login>() else { return Reply::error(400, "bad-request") };
            if let Err(reply) = panel.check(ip, password).await {
                return reply;
            }
            let token = random(32);
            guard(&panel.sessions).insert(token.clone(), Instant::now() + SESSION_LIFETIME);
            let mut reply = Reply::ok();
            reply.headers.push(format!(
                "Set-Cookie: {COOKIE}={token}; Path={prefix}/; Max-Age={}; HttpOnly; Secure; SameSite=Strict",
                SESSION_LIFETIME.as_secs()
            ));
            reply
        }
        ("GET", "/api/session") => Reply::json(
            200,
            &json!({ "signed_in": panel.signed_in(&request), "has_password": has_password(&panel.dir) }),
        ),
        _ if !panel.signed_in(&request) => Reply::error(401, "signed-out"),
        ("POST", "/api/logout") => {
            if let Some(token) = request.cookie(COOKIE) {
                guard(&panel.sessions).remove(token);
            }
            let mut reply = Reply::ok();
            reply
                .headers
                .push(format!("Set-Cookie: {COOKIE}=; Path={prefix}/; Max-Age=0; HttpOnly; Secure; SameSite=Strict"));
            reply
        }
        ("GET", "/api/state") => state(hub, panel),
        ("GET", "/api/network") => {
            let name = query.strip_prefix("name=").map(decode).unwrap_or_default();
            members(hub, &name)
        }
        ("POST", "/api/delete-network") => {
            let Ok(DeleteNetwork { name, password }) = request.json::<DeleteNetwork>() else {
                return Reply::error(400, "bad-request");
            };
            if let Err(reply) = panel.check(ip, password).await {
                return reply;
            }
            admin_reply(admin::handle(hub, AdminRequest::DeleteNetwork { name }))
        }
        ("POST", "/api/block") => {
            let Ok(Block { key, blocked }) = request.json::<Block>() else { return Reply::error(400, "bad-request") };
            let request =
                if blocked { AdminRequest::Block { device: key } } else { AdminRequest::Unblock { device: key } };
            admin_reply(admin::handle(hub, request))
        }
        _ => Reply::raw(http_reply("404 Not Found", "")),
    }
}

fn admin_reply(response: AdminResponse) -> Reply {
    match response {
        AdminResponse::Error(error) => Reply::json(400, &json!({ "error": "failed", "message": error })),
        _ => Reply::ok(),
    }
}

fn strings(language: Language) -> Reply {
    let l = Localizer::new(language);
    let strings: serde_json::Map<String, Value> = l
        .catalog()
        .into_iter()
        .filter(|(id, _)| id.starts_with("panel-") || id.starts_with("role-"))
        .map(|(id, text)| (id, Value::String(text)))
        .collect();
    Reply::json(200, &json!({ "language": language.code(), "rtl": language.is_rtl(), "strings": strings }))
}

fn state(hub: &SharedHub, panel: &Panel) -> Reply {
    let stats = admin::handle(hub, AdminRequest::Stats);
    let networks = admin::handle(hub, AdminRequest::Networks);
    let devices = admin::handle(hub, AdminRequest::Devices);
    let (host, port) = {
        let hub = lock(hub);
        (hub.config.public_host.clone(), hub.config.listen.port())
    };
    let history: Vec<Sample> = guard(&panel.history).iter().copied().collect();
    Reply::json(
        200,
        &json!({
            "version": env!("CARGO_PKG_VERSION"),
            "host": host,
            "port": port,
            "now": unix_now(),
            "sample_every": SAMPLE_EVERY.as_secs(),
            "stats": as_value(stats, |response| matches!(response, AdminResponse::Stats(_))),
            "networks": as_value(networks, |response| matches!(response, AdminResponse::Networks(_))),
            "devices": as_value(devices, |response| matches!(response, AdminResponse::Devices(_))),
            "history": history,
        }),
    )
}

/// The data of an admin response, or null when it failed.
fn as_value(response: AdminResponse, wanted: impl Fn(&AdminResponse) -> bool) -> Value {
    if !wanted(&response) {
        return Value::Null;
    }
    serde_json::to_value(&response).ok().and_then(|value| value.get("data").cloned()).unwrap_or(Value::Null)
}

fn members(hub: &SharedHub, name: &str) -> Reply {
    let hub = lock(hub);
    let Ok(Some(network)) = hub.db.network_by_name(name) else { return Reply::error(404, "no-network") };
    let Ok(members) = hub.db.members(network.id) else { return Reply::error(500, "failed") };
    let members: Vec<Value> = members
        .iter()
        .map(|(device, role)| {
            json!({
                "nickname": device.nickname,
                "address": device.address,
                "key": device.key.to_string(),
                "role": format!("{role:?}").to_lowercase(),
                "online": hub.is_online(&device.key),
            })
        })
        .collect();
    Reply::json(200, &json!({ "name": network.name, "members": members }))
}

/// Decodes a percent-encoded query value.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let hex = |byte: u8| char::from(byte).to_digit(16);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    out.push((high * 16 + low) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b'+' => out.push(b' '),
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_queries() {
        assert_eq!(decode("lan%20party"), "lan party");
        assert_eq!(decode("%D0%B4%D1%80"), "\u{434}\u{440}");
        assert_eq!(decode("a+b"), "a b");
        assert_eq!(decode("50%"), "50%");
    }

    #[test]
    fn passwords_and_secrets() {
        let dir = std::env::temp_dir().join(format!("loom-panel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = secret(&dir).unwrap();
        assert_eq!(first.len(), 20);
        assert_eq!(secret(&dir).unwrap(), first);
        assert!(!has_password(&dir));
        set_password(&dir, "hunter2").unwrap();
        assert!(has_password(&dir));
        let hash = std::fs::read_to_string(dir.join(HASH_FILE)).unwrap();
        assert!(Argon2::default().verify_password(b"hunter2", hash.trim()).is_ok());
        assert_eq!(url("::1", 443, "abc"), "https://[::1]:443/abc/");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
