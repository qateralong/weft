use blake2::Blake2sMac256;
use blake2::digest::{KeyInit, Mac};
use chacha20::ChaCha20;
use chacha20::cipher::{KeyIvInit, StreamCipher};

use crate::key::PublicKey;
use crate::packet::{HEADER_LEN, Header, MIN_PACKET_LEN, PacketError, TAG_LEN};

const LABEL: &[u8] = b"weft-obfs-v1";
const HANDSHAKE_LENGTH_NONCE: [u8; 12] = *b"weft-tcp-len";

#[derive(Clone)]
pub struct ObfsKey([u8; 32]);

impl ObfsKey {
    pub fn for_receiver(key: &PublicKey) -> Self {
        let mut mac = <Blake2sMac256 as KeyInit>::new_from_slice(LABEL).expect("label fits blake2s key");
        mac.update(key.as_bytes());
        Self(mac.finalize().into_bytes().into())
    }

    pub fn seal(&self, packet: &mut [u8]) -> Result<(), PacketError> {
        if packet.len() < MIN_PACKET_LEN {
            return Err(PacketError::TooShort);
        }
        let header = Header::decode(packet[..HEADER_LEN].try_into().unwrap())?;
        let end = if header.masks_body() { packet.len() - TAG_LEN } else { HEADER_LEN };
        self.cipher(packet).apply_keystream(&mut packet[..end]);
        Ok(())
    }

    pub fn open(&self, packet: &mut [u8]) -> Result<Header, PacketError> {
        if packet.len() < MIN_PACKET_LEN {
            return Err(PacketError::TooShort);
        }
        let mut cipher = self.cipher(packet);
        cipher.apply_keystream(&mut packet[..HEADER_LEN]);
        let header = Header::decode(packet[..HEADER_LEN].try_into().unwrap())?;
        if header.masks_body() {
            let end = packet.len() - TAG_LEN;
            cipher.apply_keystream(&mut packet[HEADER_LEN..end]);
        }
        Ok(header)
    }

    pub fn handshake_length_mask(&self) -> [u8; 2] {
        let mut mask = [0; 2];
        ChaCha20::new(&self.0.into(), &HANDSHAKE_LENGTH_NONCE.into()).apply_keystream(&mut mask);
        mask
    }

    fn cipher(&self, packet: &[u8]) -> ChaCha20 {
        let sample = &packet[packet.len() - TAG_LEN..];
        let nonce: [u8; 12] = sample[..12].try_into().unwrap();
        ChaCha20::new(&self.0.into(), &nonce.into())
    }
}

pub struct LengthMask(ChaCha20);

impl LengthMask {
    pub fn new(key: [u8; 32]) -> Self {
        Self(ChaCha20::new(&key.into(), &[0; 12].into()))
    }

    pub fn apply(&mut self, length: &mut [u8; 2]) {
        self.0.apply_keystream(length);
    }
}

pub fn discover_packet(token: &[u8; 16], padding: usize, trailer: [u8; TAG_LEN]) -> Vec<u8> {
    let mut packet = Header::Discover.encode().to_vec();
    packet.extend_from_slice(token);
    packet.resize(packet.len() + padding, 0);
    packet.extend_from_slice(&trailer);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(header: Header, body_len: usize) -> Vec<u8> {
        let mut packet = header.encode().to_vec();
        packet.extend((0..body_len).map(|i| (i * 31 + 5) as u8));
        packet
    }

    fn receiver() -> ObfsKey {
        ObfsKey::for_receiver(&PublicKey::from_bytes([1; 32]))
    }

    #[test]
    fn data_masks_only_header() {
        let header = Header::Data { receiver: 9, counter: 1234 };
        let plain = packet(header, 80);
        let mut wire = plain.clone();
        receiver().seal(&mut wire).unwrap();
        assert_ne!(wire[..HEADER_LEN], plain[..HEADER_LEN]);
        assert_eq!(wire[HEADER_LEN..], plain[HEADER_LEN..]);
        assert_eq!(receiver().open(&mut wire), Ok(header));
        assert_eq!(wire, plain);
    }

    #[test]
    fn handshake_masks_everything_but_sample() {
        let header = Header::HandshakeInit { sender: 77 };
        let plain = packet(header, 120);
        let mut wire = plain.clone();
        receiver().seal(&mut wire).unwrap();
        let end = wire.len() - TAG_LEN;
        let unchanged = wire[..end].iter().zip(&plain[..end]).filter(|(a, b)| a == b).count();
        assert!(unchanged < end / 8);
        assert_eq!(wire[end..], plain[end..]);
        assert_eq!(receiver().open(&mut wire), Ok(header));
        assert_eq!(wire, plain);
    }

    #[test]
    fn mask_depends_on_sample() {
        let header = Header::HandshakeResp { sender: 1, receiver: 2 };
        let mut a = packet(header, 40);
        let mut b = a.clone();
        let len = b.len();
        b[len - TAG_LEN] ^= 1;
        receiver().seal(&mut a).unwrap();
        receiver().seal(&mut b).unwrap();
        assert_ne!(a[..HEADER_LEN], b[..HEADER_LEN]);
    }

    #[test]
    fn wrong_key_is_rejected() {
        let mut wire = packet(Header::Data { receiver: 3, counter: 4 }, 32);
        receiver().seal(&mut wire).unwrap();
        let other = ObfsKey::for_receiver(&PublicKey::from_bytes([2; 32]));
        assert_eq!(other.open(&mut wire), Err(PacketError::InvalidHeader));
    }

    #[test]
    fn discover_roundtrip() {
        let token = [9; 16];
        let mut wire = discover_packet(&token, 5, [3; TAG_LEN]);
        receiver().seal(&mut wire).unwrap();
        assert!(!wire.windows(16).any(|w| w == token));
        assert_eq!(receiver().open(&mut wire), Ok(Header::Discover));
        assert_eq!(wire[HEADER_LEN..HEADER_LEN + 16], token);
    }

    #[test]
    fn length_masks() {
        let mut a = LengthMask::new([5; 32]);
        let mut b = LengthMask::new([5; 32]);
        let mut first = 300u16.to_be_bytes();
        let mut second = 300u16.to_be_bytes();
        a.apply(&mut first);
        a.apply(&mut second);
        assert_ne!(first, second);
        b.apply(&mut first);
        b.apply(&mut second);
        assert_eq!(u16::from_be_bytes(first), 300);
        assert_eq!(u16::from_be_bytes(second), 300);
        assert_ne!(receiver().handshake_length_mask(), [0; 2]);
    }

    #[test]
    fn too_short() {
        let mut wire = vec![0; MIN_PACKET_LEN - 1];
        assert_eq!(receiver().open(&mut wire), Err(PacketError::TooShort));
        assert_eq!(receiver().seal(&mut wire), Err(PacketError::TooShort));
    }
}
