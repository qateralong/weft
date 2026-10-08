use std::collections::BTreeSet;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};
use weft_proto::PublicKey;

use crate::db::DbError;
use crate::hub::{Hub, SharedHub, lock};
use crate::relay::Traffic;

const TOP_RELAY: usize = 5;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum AdminRequest {
    Stats,
    Networks,
    Devices,
    Block { device: String },
    Unblock { device: String },
    DeleteNetwork { name: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "result", content = "data", rename_all = "snake_case")]
pub enum AdminResponse {
    Ok,
    Stats(Stats),
    Networks(Vec<NetworkInfo>),
    Devices(Vec<DeviceInfo>),
    Error(String),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Stats {
    pub uptime_secs: u64,
    pub devices: usize,
    pub online: usize,
    pub blocked: usize,
    pub networks: usize,
    pub relay: Traffic,
    pub relay_mbit: u32,
    pub relay_total_mbit: u32,
    pub top_relay: Vec<DeviceInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct NetworkInfo {
    pub name: String,
    pub members: usize,
    pub online: usize,
    pub owner: Option<String>,
    pub locked: bool,
    pub approval: bool,
    pub created: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub nickname: String,
    pub address: Ipv4Addr,
    pub key: String,
    pub online: bool,
    pub blocked: bool,
    pub last_seen: i64,
    pub networks: usize,
    pub relay: Traffic,
}

pub fn handle(hub: &SharedHub, request: AdminRequest) -> AdminResponse {
    let mut hub = lock(hub);
    let result = match request {
        AdminRequest::Stats => stats(&hub).map(AdminResponse::Stats),
        AdminRequest::Networks => networks(&hub).map(AdminResponse::Networks),
        AdminRequest::Devices => devices(&hub).map(AdminResponse::Devices),
        AdminRequest::Block { device } => block(&mut hub, &device, true),
        AdminRequest::Unblock { device } => block(&mut hub, &device, false),
        AdminRequest::DeleteNetwork { name } => delete_network(&mut hub, &name),
    };
    result.unwrap_or_else(|error| AdminResponse::Error(error.to_string()))
}

#[derive(Debug, thiserror::Error)]
enum AdminError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("no such device")]
    NoDevice,
    #[error("several devices match; use the address or key")]
    Ambiguous,
    #[error("no such network")]
    NoNetwork,
}

fn devices(hub: &Hub) -> Result<Vec<DeviceInfo>, AdminError> {
    Ok(hub
        .db
        .all_devices()?
        .into_iter()
        .map(|row| DeviceInfo {
            online: hub.is_online(&row.device.key),
            relay: hub.relay_traffic(&row.device.key),
            key: row.device.key.to_string(),
            nickname: row.device.nickname,
            address: row.device.address,
            blocked: row.blocked,
            last_seen: row.last_seen,
            networks: row.networks,
        })
        .collect())
}

fn stats(hub: &Hub) -> Result<Stats, AdminError> {
    let devices = devices(hub)?;
    let mut top_relay: Vec<DeviceInfo> = devices.iter().filter(|device| device.relay.packets > 0).cloned().collect();
    top_relay.sort_by_key(|device| std::cmp::Reverse(device.relay.bytes));
    top_relay.truncate(TOP_RELAY);
    Ok(Stats {
        uptime_secs: hub.uptime().as_secs(),
        devices: devices.len(),
        online: devices.iter().filter(|device| device.online).count(),
        blocked: devices.iter().filter(|device| device.blocked).count(),
        networks: hub.db.network_summaries()?.len(),
        relay: hub.relay_total(),
        relay_mbit: hub.config.relay_mbit,
        relay_total_mbit: hub.config.relay_total_mbit,
        top_relay,
    })
}

fn networks(hub: &Hub) -> Result<Vec<NetworkInfo>, AdminError> {
    let mut networks = Vec::new();
    for summary in hub.db.network_summaries()? {
        let online = hub.db.members(summary.id)?.iter().filter(|(device, _)| hub.is_online(&device.key)).count();
        networks.push(NetworkInfo {
            name: summary.name,
            members: summary.members,
            online,
            owner: summary.owner,
            locked: summary.locked,
            approval: summary.approval,
            created: summary.created,
        });
    }
    Ok(networks)
}

fn block(hub: &mut Hub, query: &str, blocked: bool) -> Result<AdminResponse, AdminError> {
    let rows = hub.db.all_devices()?;
    let devices: Vec<_> = rows.iter().map(|row| row.device.clone()).collect();
    let index = crate::control::resolve(devices.iter(), query).map_err(|code| match code {
        weft_proto::control::ErrorCode::AmbiguousMember => AdminError::Ambiguous,
        _ => AdminError::NoDevice,
    })?;
    let key = devices[index].key;
    if blocked {
        hub.db.block(&key)?;
        let related = hub.db.related(&key)?;
        if hub.disconnect(&key) {
            hub.notify(&related);
        }
        tracing::info!(?key, "device blocked");
    } else {
        hub.db.unblock(&key)?;
        tracing::info!(?key, "device unblocked");
    }
    Ok(AdminResponse::Ok)
}

fn delete_network(hub: &mut Hub, name: &str) -> Result<AdminResponse, AdminError> {
    let network = hub.db.network_by_name(name)?.ok_or(AdminError::NoNetwork)?;
    let members: BTreeSet<PublicKey> = hub.db.members(network.id)?.into_iter().map(|(device, _)| device.key).collect();
    hub.db.delete_network(network.id)?;
    hub.memberships_changed();
    hub.notify(&members);
    tracing::info!(network = network.name, "network deleted by the administrator");
    Ok(AdminResponse::Ok)
}

#[cfg(unix)]
pub use unix::{request, serve};

#[cfg(unix)]
mod unix {
    use std::io;
    use std::path::Path;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};

    use super::{AdminRequest, AdminResponse, handle};
    use crate::hub::SharedHub;

    pub fn bind(path: &Path) -> io::Result<UnixListener> {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }

    pub async fn serve(listener: UnixListener, hub: SharedHub) {
        loop {
            let Ok((stream, _)) = listener.accept().await else { continue };
            let hub = hub.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let response = match serde_json::from_str::<AdminRequest>(&line) {
                        Ok(request) => handle(&hub, request),
                        Err(error) => AdminResponse::Error(error.to_string()),
                    };
                    let Ok(mut text) = serde_json::to_string(&response) else { return };
                    text.push('\n');
                    if writer.write_all(text.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    }

    pub async fn request(path: &Path, request: &AdminRequest) -> io::Result<AdminResponse> {
        let stream = UnixStream::connect(path).await?;
        let (reader, mut writer) = stream.into_split();
        let mut text = serde_json::to_string(request).map_err(io::Error::other)?;
        text.push('\n');
        writer.write_all(text.as_bytes()).await?;
        let line = BufReader::new(reader)
            .lines()
            .next_line()
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "loom closed the admin connection"))?;
        serde_json::from_str(&line).map_err(io::Error::other)
    }
}

#[cfg(unix)]
pub use unix::bind;
