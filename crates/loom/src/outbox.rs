//! Messages waiting to be written to one client. A client that stops reading is cut off instead
//! of letting its queue grow without end.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use prost::Message;
use tokio::sync::{Notify, mpsc};
use weft_proto::control::{ServerKind, ServerMessage};

/// Bytes that may wait for one client: a quarter of a second of relay at its fastest.
pub const MAX_QUEUED: usize = 1 << 20;

#[derive(Clone)]
pub struct Outbox {
    tx: mpsc::UnboundedSender<ServerMessage>,
    queued: Arc<AtomicUsize>,
    overflow: Arc<Notify>,
}

pub struct Inbox {
    rx: mpsc::UnboundedReceiver<ServerMessage>,
    queued: Arc<AtomicUsize>,
}

pub fn channel() -> (Outbox, Inbox) {
    let (tx, rx) = mpsc::unbounded_channel();
    let queued = Arc::new(AtomicUsize::new(0));
    (Outbox { tx, queued: queued.clone(), overflow: Arc::new(Notify::new()) }, Inbox { rx, queued })
}

impl Outbox {
    /// Queues a message. When the client is too far behind, a relayed packet is dropped like on
    /// a congested link, and anything else marks the connection for closing.
    pub fn send(&self, message: ServerMessage) -> bool {
        let size = message.encoded_len();
        if self.queued.load(Ordering::Relaxed) + size > MAX_QUEUED {
            if !matches!(message.kind, Some(ServerKind::Relay(_))) {
                self.overflow.notify_one();
            }
            return false;
        }
        self.queued.fetch_add(size, Ordering::Relaxed);
        self.tx.send(message).is_ok()
    }

    /// Resolves once a message could not be queued because the client stopped reading.
    pub async fn overflowed(&self) {
        self.overflow.notified().await;
    }
}

impl Inbox {
    pub async fn recv(&mut self) -> Option<ServerMessage> {
        let message = self.rx.recv().await?;
        self.queued.fetch_sub(message.encoded_len(), Ordering::Relaxed);
        Some(message)
    }
}

#[cfg(test)]
mod tests {
    use weft_proto::control::{Network, RelayPacket, State};

    use super::*;

    #[tokio::test]
    async fn cuts_off_a_client_that_stops_reading() {
        let wait = std::time::Duration::from_millis(50);
        let (outbox, mut inbox) = channel();
        let relay = || ServerMessage::push(ServerKind::Relay(RelayPacket { address: 1, packet: vec![0; 64 * 1024] }));
        let accepted = (0..32).take_while(|_| outbox.send(relay())).count();
        assert_eq!(accepted, 15);
        assert!(tokio::time::timeout(wait, outbox.overflowed()).await.is_err());
        let network = Network { name: "x".repeat(70_000), ..Default::default() };
        let state = State { networks: vec![network], peers: Vec::new() };
        assert!(!outbox.send(ServerMessage::push(ServerKind::State(state))));
        tokio::time::timeout(wait, outbox.overflowed()).await.unwrap();
        assert!(inbox.recv().await.is_some());
        assert!(outbox.send(relay()));
    }
}
