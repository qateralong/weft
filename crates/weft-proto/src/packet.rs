pub const HEADER_LEN: usize = 16;
pub const TAG_LEN: usize = 16;
pub const MIN_PACKET_LEN: usize = HEADER_LEN + TAG_LEN;

const TYPE_HANDSHAKE_INIT: u8 = 1;
const TYPE_HANDSHAKE_RESP: u8 = 2;
const TYPE_DATA: u8 = 3;
const TYPE_DISCOVER: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Header {
    HandshakeInit { sender: u32 },
    HandshakeResp { sender: u32, receiver: u32 },
    Data { receiver: u32, counter: u64 },
    Discover,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PacketError {
    #[error("packet is too short")]
    TooShort,
    #[error("invalid header")]
    InvalidHeader,
    #[error("invalid inner packet")]
    InvalidPayload,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let (kind, a, b) = match *self {
            Header::HandshakeInit { sender } => (TYPE_HANDSHAKE_INIT, sender, 0),
            Header::HandshakeResp { sender, receiver } => (TYPE_HANDSHAKE_RESP, sender, u64::from(receiver)),
            Header::Data { receiver, counter } => (TYPE_DATA, receiver, counter),
            Header::Discover => (TYPE_DISCOVER, 0, 0),
        };
        let mut out = [0; HEADER_LEN];
        out[0] = kind;
        out[4..8].copy_from_slice(&a.to_le_bytes());
        out[8..16].copy_from_slice(&b.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8; HEADER_LEN]) -> Result<Self, PacketError> {
        if bytes[1..4] != [0; 3] {
            return Err(PacketError::InvalidHeader);
        }
        let a = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let b = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        match bytes[0] {
            TYPE_HANDSHAKE_INIT if b == 0 => Ok(Header::HandshakeInit { sender: a }),
            TYPE_HANDSHAKE_RESP => {
                let receiver = u32::try_from(b).map_err(|_| PacketError::InvalidHeader)?;
                Ok(Header::HandshakeResp { sender: a, receiver })
            }
            TYPE_DATA => Ok(Header::Data { receiver: a, counter: b }),
            TYPE_DISCOVER if a == 0 && b == 0 => Ok(Header::Discover),
            _ => Err(PacketError::InvalidHeader),
        }
    }

    pub fn masks_body(&self) -> bool {
        !matches!(self, Header::Data { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for header in [
            Header::HandshakeInit { sender: 0xdead_beef },
            Header::HandshakeResp { sender: 1, receiver: u32::MAX },
            Header::Data { receiver: 7, counter: u64::MAX },
            Header::Discover,
        ] {
            assert_eq!(Header::decode(&header.encode()), Ok(header));
        }
    }

    #[test]
    fn rejects_garbage() {
        let mut bytes = Header::Data { receiver: 1, counter: 2 }.encode();
        bytes[2] = 1;
        assert_eq!(Header::decode(&bytes), Err(PacketError::InvalidHeader));

        let mut bytes = Header::HandshakeInit { sender: 1 }.encode();
        bytes[15] = 1;
        assert_eq!(Header::decode(&bytes), Err(PacketError::InvalidHeader));

        let mut bytes = Header::HandshakeResp { sender: 1, receiver: 2 }.encode();
        bytes[12] = 1;
        assert_eq!(Header::decode(&bytes), Err(PacketError::InvalidHeader));

        let mut bytes = [0; HEADER_LEN];
        bytes[0] = 0xff;
        assert_eq!(Header::decode(&bytes), Err(PacketError::InvalidHeader));
    }
}
