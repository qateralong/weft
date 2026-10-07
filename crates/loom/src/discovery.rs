use tokio::net::UdpSocket;
use weft_proto::control::DISCOVERY_TOKEN_LEN;
use weft_proto::{HEADER_LEN, Header, ObfsKey, TAG_LEN};

use crate::hub::{SharedHub, lock};

pub async fn serve(socket: UdpSocket, hub: SharedHub, own: ObfsKey) {
    let mut buf = vec![0; 2048];
    loop {
        let (len, addr) = match socket.recv_from(&mut buf).await {
            Ok(received) => received,
            Err(error) => {
                tracing::debug!(%error, "udp receive failed");
                continue;
            }
        };
        if len < HEADER_LEN + DISCOVERY_TOKEN_LEN + TAG_LEN || own.open(&mut buf[..len]) != Ok(Header::Discover) {
            continue;
        }
        let token = buf[HEADER_LEN..HEADER_LEN + DISCOVERY_TOKEN_LEN].try_into().expect("token length checked");
        let mut hub = lock(&hub);
        if let Some(key) = hub.discovered(&token, addr) {
            tracing::debug!(?key, %addr, "endpoint discovered");
            hub.notify_related(&key);
        }
    }
}
