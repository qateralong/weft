use std::net::SocketAddr;

use blake2::Blake2sMac256;
use blake2::digest::{KeyInit, Mac};
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use weft_proto::loom::{decode_endpoint, encode_endpoint};
use weft_proto::{HEADER_LEN, Header, KEY_LEN, ObfsKey, PacketError, PublicKey};
use x25519_dalek::StaticSecret;

pub const TX_LEN: usize = 12;
const NONCE_LEN: usize = 24;
const LABEL: &[u8] = b"weft-disco-v1";
const PING: u8 = 1;
const PONG: u8 = 2;

pub type TxId = [u8; TX_LEN];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message {
    Ping { tx: TxId },
    Pong { tx: TxId, observed: SocketAddr },
}

#[derive(Clone)]
pub struct DiscoKey(XChaCha20Poly1305);

impl DiscoKey {
    pub fn derive(own_secret: &[u8; KEY_LEN], remote: &PublicKey) -> Option<Self> {
        let shared = StaticSecret::from(*own_secret).diffie_hellman(&x25519_dalek::PublicKey::from(*remote.as_bytes()));
        if !shared.was_contributory() {
            return None;
        }
        let mut mac = <Blake2sMac256 as KeyInit>::new_from_slice(LABEL).expect("label fits blake2s key");
        mac.update(shared.as_bytes());
        let key: [u8; 32] = mac.finalize().into_bytes().into();
        Some(Self(XChaCha20Poly1305::new(&key.into())))
    }
}

pub fn seal(
    key: &DiscoKey,
    sender: &PublicKey,
    receiver: &ObfsKey,
    message: &Message,
    padding: usize,
    nonce: [u8; NONCE_LEN],
) -> Result<Vec<u8>, PacketError> {
    let mut plaintext = match message {
        Message::Ping { tx } => [&[PING][..], tx].concat(),
        Message::Pong { tx, observed } => [&[PONG][..], tx, &encode_endpoint(*observed)].concat(),
    };
    plaintext.resize(plaintext.len() + padding, 0);
    let ciphertext = key
        .0
        .encrypt(&XNonce::from(nonce), Payload { msg: &plaintext, aad: sender.as_bytes() })
        .map_err(|_| PacketError::InvalidPayload)?;
    let mut packet = [&Header::Disco.encode()[..], sender.as_bytes(), &nonce, &ciphertext].concat();
    receiver.seal(&mut packet)?;
    Ok(packet)
}

pub fn sender(packet: &[u8]) -> Option<PublicKey> {
    PublicKey::from_slice(packet.get(HEADER_LEN..HEADER_LEN + KEY_LEN)?).ok()
}

pub fn open(key: &DiscoKey, packet: &[u8]) -> Option<Message> {
    let sender = packet.get(HEADER_LEN..HEADER_LEN + KEY_LEN)?;
    let nonce: [u8; NONCE_LEN] = packet.get(HEADER_LEN + KEY_LEN..HEADER_LEN + KEY_LEN + NONCE_LEN)?.try_into().ok()?;
    let ciphertext = packet.get(HEADER_LEN + KEY_LEN + NONCE_LEN..)?;
    let plaintext = key.0.decrypt(&XNonce::from(nonce), Payload { msg: ciphertext, aad: sender }).ok()?;
    let tx: TxId = plaintext.get(1..1 + TX_LEN)?.try_into().ok()?;
    match *plaintext.first()? {
        PING => Some(Message::Ping { tx }),
        PONG => Some(Message::Pong { tx, observed: decode_endpoint(plaintext.get(1 + TX_LEN..)?)? }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use weft_session::StaticKeypair;

    use super::*;

    #[test]
    fn ping_pong_roundtrip() {
        let a = StaticKeypair::from_secret(&[1; 32]);
        let b = StaticKeypair::from_secret(&[2; 32]);
        let key_ab = DiscoKey::derive(a.secret(), &b.public()).unwrap();
        let key_ba = DiscoKey::derive(b.secret(), &a.public()).unwrap();
        let obfs_b = ObfsKey::for_receiver(&b.public());

        for message in [
            Message::Ping { tx: [7; TX_LEN] },
            Message::Pong { tx: [8; TX_LEN], observed: "203.0.113.4:5000".parse().unwrap() },
        ] {
            let mut packet = seal(&key_ab, &a.public(), &obfs_b, &message, 9, [3; NONCE_LEN]).unwrap();
            assert_eq!(obfs_b.open(&mut packet), Ok(Header::Disco));
            assert_eq!(sender(&packet), Some(a.public()));
            assert_eq!(open(&key_ba, &packet), Some(message));
        }
    }

    #[test]
    fn forged_sender_is_rejected() {
        let a = StaticKeypair::from_secret(&[1; 32]);
        let b = StaticKeypair::from_secret(&[2; 32]);
        let c = StaticKeypair::from_secret(&[3; 32]);
        let obfs_b = ObfsKey::for_receiver(&b.public());
        let key_cb = DiscoKey::derive(c.secret(), &b.public()).unwrap();
        let mut packet =
            seal(&key_cb, &a.public(), &obfs_b, &Message::Ping { tx: [1; TX_LEN] }, 0, [0; NONCE_LEN]).unwrap();
        obfs_b.open(&mut packet).unwrap();
        let key_ba = DiscoKey::derive(b.secret(), &a.public()).unwrap();
        assert_eq!(open(&key_ba, &packet), None);
    }

    #[test]
    fn low_order_key_is_rejected() {
        assert!(DiscoKey::derive(&[1; 32], &PublicKey::from_bytes([0; 32])).is_none());
    }
}
