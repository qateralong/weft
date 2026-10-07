use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use weft_ipc::{Failure, Request, Response};

const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Command {
    pub request: Request,
    pub reply: oneshot::Sender<Response>,
}

pub fn bind(path: &Path) -> io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    Ok(listener)
}

pub async fn serve(listener: UnixListener, commands: mpsc::Sender<Command>) {
    loop {
        match listener.accept().await {
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

async fn client(stream: UnixStream, commands: mpsc::Sender<Command>) {
    let (reader, mut writer) = stream.into_split();
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
