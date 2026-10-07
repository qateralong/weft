use weft_proto::PacketError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Packet(#[from] PacketError),
    #[error("noise: {0}")]
    Noise(#[from] snow::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("unknown peer")]
    UnknownPeer,
    #[error("unknown receiver index")]
    UnknownIndex,
    #[error("stale handshake")]
    StaleHandshake,
    #[error("replayed packet")]
    Replay,
    #[error("session expired")]
    Expired,
    #[error("invalid handshake payload")]
    InvalidPayload,
    #[error("unexpected packet")]
    Unexpected,
    #[error("message is too large")]
    TooLarge,
}
