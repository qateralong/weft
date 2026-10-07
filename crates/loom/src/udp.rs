use std::time::Instant;

use rand::RngExt;
use tokio::net::UdpSocket;
use weft_proto::control::{DISCOVERY_TOKEN_LEN, RelayPacket, ServerKind, ServerMessage};
use weft_proto::loom::{Token, observed_packet, parse_relay, relayed_packet};
use weft_proto::{HEADER_LEN, Header, ObfsKey, TAG_LEN};

use crate::hub::{Delivery, Route, SharedHub, lock};

pub async fn serve(socket: std::sync::Arc<UdpSocket>, hub: SharedHub, own: ObfsKey) {
    let mut buf = vec![0; 65_535];
    loop {
        let (len, addr) = match socket.recv_from(&mut buf).await {
            Ok(received) => received,
            Err(error) => {
                tracing::debug!(%error, "udp receive failed");
                continue;
            }
        };
        let packet = &mut buf[..len];
        match own.open(packet) {
            Ok(Header::Discover) => discover(&socket, &hub, packet, addr).await,
            Ok(Header::Relay) => relay(&socket, &hub, packet).await,
            _ => {}
        }
    }
}

async fn discover(socket: &UdpSocket, hub: &SharedHub, packet: &[u8], addr: std::net::SocketAddr) {
    if packet.len() < HEADER_LEN + DISCOVERY_TOKEN_LEN + TAG_LEN {
        return;
    }
    let token: Token = packet[HEADER_LEN..HEADER_LEN + DISCOVERY_TOKEN_LEN].try_into().expect("length checked");
    let owner = {
        let mut hub = lock(hub);
        if let Some(key) = hub.discovered(&token, addr, Instant::now()) {
            tracing::debug!(?key, %addr, "endpoint discovered");
            hub.notify_related(&key);
        }
        hub.token_owner(&token)
    };
    let Some(owner) = owner else { return };
    let mut reply = {
        let mut rng = rand::rng();
        observed_packet(&token, addr, rng.random_range(0..=32), rng.random())
    };
    if ObfsKey::for_receiver(&owner).seal(&mut reply).is_ok() {
        let _ = socket.send_to(&reply, addr).await;
    }
}

async fn relay(socket: &UdpSocket, hub: &SharedHub, packet: &[u8]) {
    let Some((token, destination, inner)) = parse_relay(packet) else { return };
    let route = {
        let mut hub = lock(hub);
        let Some(source) = hub.token_owner(&token) else { return };
        hub.route(&source, destination, Instant::now())
    };
    if let Some(route) = route {
        forward(socket, route, inner).await;
    }
}

pub async fn forward(socket: &UdpSocket, route: Route, inner: &[u8]) {
    match route.delivery {
        Delivery::Udp(addr, obfs) => {
            let mut packet = relayed_packet(route.source, inner);
            if obfs.seal(&mut packet).is_ok() {
                let _ = socket.send_to(&packet, addr).await;
            }
        }
        Delivery::Tcp(tx) => {
            let packet = RelayPacket { address: u32::from(route.source), packet: inner.to_vec() };
            let _ = tx.send(ServerMessage::push(ServerKind::Relay(packet)));
        }
    }
}
