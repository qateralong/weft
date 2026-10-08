//! Talking to weftd from the window: requests run on a small runtime off the UI thread.

use std::future::Future;
use std::sync::{Arc, LazyLock};

use weft_i18n::Localizer;
use weft_ipc::{Failure, Request, Response};

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime")
});

/// Runs `future` on the background runtime; awaiting the result is fine on the UI thread.
pub async fn background<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> T {
    RUNTIME.spawn(future).await.expect("background task")
}

#[cfg(windows)]
pub fn spawn(future: impl Future<Output = ()> + Send + 'static) {
    RUNTIME.spawn(future);
}

/// Sends a request, turning every failure into a translated message.
pub async fn send(l: &Arc<Localizer>, request: Request, server: Option<String>) -> Result<Response, String> {
    let l = l.clone();
    background(async move {
        let path = weft_ipc::socket_path();
        let response = weft_ipc::request_on(&path, &request, server.as_deref()).await.map_err(|error| {
            let path = path.display().to_string();
            match error.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                    l.tr_args("error-daemon-missing", &[("path", &path)])
                }
                std::io::ErrorKind::PermissionDenied => l.tr_args("gui-error-permission", &[("path", &path)]),
                _ => l.tr_args("error-daemon", &[("path", &path), ("reason", &error.to_string())]),
            }
        })?;
        match response {
            Response::Error(failure) => Err(l.tr(failure.message_id())),
            response => Ok(response),
        }
    })
    .await
}

/// Why the daemon cannot be used, for offering a fix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonState {
    Ok,
    Missing,
    Denied,
    Other,
}

pub async fn state() -> DaemonState {
    background(async {
        match weft_ipc::request(&weft_ipc::socket_path(), &Request::Status).await {
            Ok(Response::Error(Failure::AccessDenied)) => DaemonState::Denied,
            Ok(_) => DaemonState::Ok,
            Err(error) => match error.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => DaemonState::Missing,
                std::io::ErrorKind::PermissionDenied => DaemonState::Denied,
                _ => DaemonState::Other,
            },
        }
    })
    .await
}

pub async fn report(l: &Arc<Localizer>, logs: bool) -> Result<String, String> {
    match send(l, Request::Diagnose { logs }, None).await? {
        Response::Diagnostics(diagnostics) => {
            Ok(weft_ipc::report::format(&diagnostics, &|id, args| l.tr_args(id, args)))
        }
        _ => Err(l.tr("error-internal")),
    }
}

/// Runs a short future to completion from outside the runtime.
#[cfg(windows)]
pub fn block_on<T>(future: impl Future<Output = T>) -> T {
    RUNTIME.block_on(future)
}
