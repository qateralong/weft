pub mod config;
mod control;
pub mod db;
mod discovery;
mod hub;
mod limiter;
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
    tasks: Vec<JoinHandle<()>>,
}

impl Server {
    pub async fn start(config: Config, keypair: StaticKeypair, db: Db) -> io::Result<Self> {
        let listener = TcpListener::bind(config.listen).await?;
        let tcp_addr = listener.local_addr()?;
        let socket = UdpSocket::bind(tcp_addr).await?;
        let udp_addr = socket.local_addr()?;
        let public_key = keypair.public();
        let own = ObfsKey::for_receiver(&public_key);
        let hub = Arc::new(Mutex::new(Hub::new(db, config)));
        let tasks = vec![
            tokio::spawn(control::serve(listener, hub.clone(), Arc::new(keypair), udp_addr.port())),
            tokio::spawn(discovery::serve(socket, hub, own)),
        ];
        Ok(Self { tcp_addr, udp_addr, public_key, tasks })
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

pub fn server_link(host: &str, port: u16, key: &PublicKey) -> Result<Link, weft_proto::LinkError> {
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
    format!("weft://{host}:{port}#k={key}").parse()
}
