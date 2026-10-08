use std::io;
use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, oneshot};
use weft_ipc::{Envelope, Failure, Request, Response};

const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(unix)]
const SOCKET_GROUP: &str = "weft";
/// SYSTEM and administrators get full access, interactive users read and write.
#[cfg(windows)]
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";

pub struct Command {
    pub request: Request,
    pub server: Option<String>,
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
    if let Ok(Some(group)) = nix::unistd::Group::from_name(SOCKET_GROUP) {
        std::os::unix::fs::chown(path, None, Some(group.gid.as_raw()))?;
    }
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
    let first = create_pipe(path.as_os_str(), true)?;
    Ok(Listener { path: path.as_os_str().to_owned(), first })
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn create_pipe(path: &std::ffi::OsStr, first: bool) -> io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use tokio::net::windows::named_pipe::ServerOptions;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

    let sddl: Vec<u16> = PIPE_SDDL.encode_utf16().chain(Some(0)).collect();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated and `descriptor` receives a LocalAlloc'ed pointer freed below.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    // SAFETY: `attributes` points to a valid security descriptor for the duration of the call.
    let pipe = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .create_with_security_attributes_raw(path, (&raw mut attributes).cast())
    };
    // SAFETY: the descriptor was allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
    unsafe { LocalFree(descriptor) };
    pipe
}

#[cfg(windows)]
pub async fn serve(listener: Listener, commands: mpsc::Sender<Command>) {
    let Listener { path, mut first } = listener;
    loop {
        if let Err(error) = first.connect().await {
            tracing::warn!(%error, "ipc accept failed");
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        let next = match create_pipe(&path, false) {
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
    while let Ok(Some(Envelope { request, server })) = weft_ipc::receive::<Envelope, _>(&mut reader).await {
        let (reply, response) = oneshot::channel();
        if commands.send(Command { request, server, reply }).await.is_err() {
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
