use std::io;
use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, oneshot};
use weft_ipc::{Failure, Request, Response};

const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Command {
    pub request: Request,
    pub reply: oneshot::Sender<Response>,
}

#[cfg(unix)]
pub struct Listener(tokio::net::UnixListener);

#[cfg(unix)]
pub fn bind(path: &Path) -> io::Result<Listener> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    Ok(Listener(listener))
}

#[cfg(unix)]
pub async fn serve(listener: Listener, commands: mpsc::Sender<Command>) {
    loop {
        match listener.0.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(client(stream, commands.clone()));
            }
            Err(error) => {
                tracing::warn!(%error, "ipc accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

#[cfg(unix)]
pub fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(windows)]
pub struct Listener {
    path: std::ffi::OsString,
    first: tokio::net::windows::named_pipe::NamedPipeServer,
}

#[cfg(windows)]
pub fn bind(path: &Path) -> io::Result<Listener> {
    let first = tokio::net::windows::named_pipe::ServerOptions::new().first_pipe_instance(true).create(path)?;
    Ok(Listener { path: path.as_os_str().to_owned(), first })
}

#[cfg(windows)]
pub async fn serve(listener: Listener, commands: mpsc::Sender<Command>) {
    use tokio::net::windows::named_pipe::ServerOptions;
    let Listener { path, mut first } = listener;
    loop {
        if let Err(error) = first.connect().await {
            tracing::warn!(%error, "ipc accept failed");
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        let next = match ServerOptions::new().create(&path) {
            Ok(next) => next,
            Err(error) => {
                tracing::error!(%error, "cannot create the next pipe instance");
                return;
            }
        };
        tokio::spawn(client(std::mem::replace(&mut first, next), commands.clone()));
    }
}

#[cfg(windows)]
pub fn cleanup(_path: &Path) {}

async fn client<S: AsyncRead + AsyncWrite + Send + 'static>(stream: S, commands: mpsc::Sender<Command>) {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    while let Ok(Some(request)) = weft_ipc::receive::<Request, _>(&mut reader).await {
        let (reply, response) = oneshot::channel();
        if commands.send(Command { request, reply }).await.is_err() {
            return;
        }
        let response = match tokio::time::timeout(REPLY_TIMEOUT, response).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => Response::Error(Failure::Internal),
            Err(_) => Response::Error(Failure::Timeout),
        };
        if weft_ipc::send(&mut writer, &response).await.is_err() {
            return;
        }
    }
}
