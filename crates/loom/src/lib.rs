pub mod admin;
pub mod config;
mod control;
pub mod db;
mod hub;
mod limiter;
pub mod panel;
pub mod relay;
pub mod tls;
mod udp;
mod validate;

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::net::{TcpListener, UdpSocket};
use tokio::task::JoinHandle;
use weft_proto::{Link, ObfsKey, PublicKey};
use weft_session::StaticKeypair;

use crate::config::Config;
use crate::db::Db;
use crate::hub::Hub;

pub struct Server {
    pub tcp_addr: SocketAddr,
    pub udp_addr: SocketAddr,
    pub public_key: PublicKey,
    #[cfg_attr(not(unix), allow(dead_code))]
    hub: crate::hub::SharedHub,
    tasks: Vec<JoinHandle<()>>,
}

impl Server {
    pub async fn start(config: Config, keypair: StaticKeypair, db: Db) -> io::Result<Self> {
        let (listener, socket) = bind_pair(config.listen).await?;
        let tcp_addr = listener.local_addr()?;
        let socket = Arc::new(socket);
        let udp_addr = socket.local_addr()?;
        let public_key = keypair.public();
        let own = ObfsKey::for_receiver(&public_key);
        let tls = tls::acceptor(&config).map_err(io::Error::other)?;
        let panel = Arc::new(panel::Panel::new(config.data_dir.clone()));
        let hub = Arc::new(Mutex::new(Hub::new(db, config)));
        let tasks = vec![
            tokio::spawn(control::serve(listener, hub.clone(), Arc::new(keypair), socket.clone(), tls, panel.clone())),
            tokio::spawn(udp::serve(socket, hub.clone(), own)),
            tokio::spawn(panel.sample(hub.clone())),
        ];
        Ok(Self { tcp_addr, udp_addr, public_key, hub, tasks })
    }

    /// Serves `loom admin` requests on a local socket that only the owner can open.
    #[cfg(unix)]
    pub fn serve_admin(&mut self, path: &std::path::Path) -> io::Result<()> {
        let listener = admin::bind(path)?;
        self.tasks.push(tokio::spawn(admin::serve(listener, self.hub.clone())));
        Ok(())
    }

    pub fn link(&self, host: &str) -> Result<Link, weft_proto::LinkError> {
        server_link(host, self.tcp_addr.port(), &self.public_key)
    }

    pub async fn run(mut self) {
        for task in std::mem::take(&mut self.tasks) {
            let _ = task.await;
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// TCP and UDP on the same port. For port 0 the UDP port is picked first and retried, as Windows
/// reserves ranges of UDP ports that a free TCP port may fall into.
async fn bind_pair(addr: SocketAddr) -> io::Result<(TcpListener, UdpSocket)> {
    if addr.port() != 0 {
        let listener = TcpListener::bind(addr).await?;
        let socket = UdpSocket::bind(listener.local_addr()?).await?;
        return Ok((listener, socket));
    }
    let mut last = io::Error::other("no free port");
    for _ in 0..20 {
        let socket = UdpSocket::bind(addr).await?;
        match TcpListener::bind(socket.local_addr()?).await {
            Ok(listener) => return Ok((listener, socket)),
            Err(error) => last = error,
        }
    }
    Err(last)
}

pub fn server_link(host: &str, port: u16, key: &PublicKey) -> Result<Link, weft_proto::LinkError> {
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
    format!("weft://{host}:{port}#k={key}").parse()
}
