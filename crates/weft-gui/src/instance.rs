//! Keeps one window per user: a second start asks the first one to show itself and exits.

#[cfg(unix)]
pub fn claim(show: impl Fn() + Send + 'static) -> bool {
    use std::io::Write;
    use std::os::unix::net::{UnixListener, UnixStream};

    let dir = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    let user = std::env::var("USER").unwrap_or_default();
    let path = dir.join(format!("weft-gui-{user}.sock"));
    if let Ok(mut stream) = UnixStream::connect(&path) {
        let _ = stream.write_all(b"show\n");
        return false;
    }
    let _ = std::fs::remove_file(&path);
    let Ok(listener) = UnixListener::bind(&path) else { return true };
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stream.is_ok() {
                show();
            }
        }
    });
    true
}

#[cfg(windows)]
pub fn claim(show: impl Fn() + Send + 'static) -> bool {
    use tokio::io::AsyncWriteExt;
    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

    let user = std::env::var("USERNAME").unwrap_or_default();
    let name = format!(r"\\.\pipe\weft-gui-{user}");
    let other = crate::daemon::block_on({
        let name = name.clone();
        async move {
            match ClientOptions::new().open(&name) {
                Ok(mut pipe) => {
                    let _ = pipe.write_all(b"show\n").await;
                    true
                }
                Err(_) => false,
            }
        }
    });
    if other {
        return false;
    }
    crate::daemon::spawn(async move {
        let Ok(mut server) = ServerOptions::new().first_pipe_instance(true).create(&name) else { return };
        loop {
            if server.connect().await.is_ok() {
                show();
            }
            match ServerOptions::new().create(&name) {
                Ok(next) => server = next,
                Err(_) => return,
            }
        }
    });
    true
}
