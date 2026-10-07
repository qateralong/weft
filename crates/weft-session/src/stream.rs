use std::sync::Arc;
use std::time::SystemTime;

use blake2::Blake2sMac256;
use blake2::digest::{KeyInit, Mac};
use rand::RngExt;
use snow::{HandshakeState, StatelessTransportState};
use weft_proto::{HEADER_LEN, Header, LengthMask, MIN_PACKET_LEN, ObfsKey, PublicKey, TAG_LEN};

use crate::error::Error;
use crate::keys::StaticKeypair;
use crate::noise::{self, MAX_HANDSHAKE_LEN, MAX_HANDSHAKE_PADDING};
use crate::tai64n::{TAI64N_LEN, Tai64N};

pub const MAX_FRAME_LEN: usize = u16::MAX as usize;
pub const MAX_MESSAGE_LEN: usize = MAX_FRAME_LEN - HEADER_LEN - TAG_LEN;
pub const MAX_HANDSHAKE_FRAME_LEN: usize = HEADER_LEN + MAX_HANDSHAKE_LEN;

pub struct ClientHandshake {
    state: HandshakeState,
    server: PublicKey,
    own: ObfsKey,
}

pub struct Accepted {
    pub channel: Channel,
    pub timestamp: Tai64N,
    pub frame: Vec<u8>,
}

pub struct Channel {
    sender: Sender,
    receiver: Receiver,
}

pub struct Sender {
    transport: Arc<StatelessTransportState>,
    obfs: ObfsKey,
    counter: u64,
    length: LengthMask,
}

pub struct Receiver {
    transport: Arc<StatelessTransportState>,
    obfs: ObfsKey,
    counter: u64,
    length: LengthMask,
    remote: PublicKey,
}

pub fn connect(
    keypair: &StaticKeypair,
    server: PublicKey,
    wall: SystemTime,
) -> Result<(ClientHandshake, Vec<u8>), Error> {
    let mut state = noise::builder(keypair).remote_public_key(server.as_bytes())?.build_initiator()?;
    let mut payload = vec![0; TAI64N_LEN + rand::rng().random_range(0..=MAX_HANDSHAKE_PADDING)];
    payload[..TAI64N_LEN].copy_from_slice(Tai64N::from_system_time(wall).as_bytes());
    let packet = handshake_packet(&mut state, Header::HandshakeInit { sender: 0 }, &payload)?;
    let frame = handshake_frame(packet, &ObfsKey::for_receiver(&server))?;
    Ok((ClientHandshake { state, server, own: ObfsKey::for_receiver(&keypair.public()) }, frame))
}

impl ClientHandshake {
    pub fn own_obfs(&self) -> &ObfsKey {
        &self.own
    }

    pub fn finish(mut self, packet: &[u8]) -> Result<Channel, Error> {
        let mut packet = packet.to_vec();
        if self.own.open(&mut packet)? != (Header::HandshakeResp { sender: 0, receiver: 0 }) {
            return Err(Error::Unexpected);
        }
        let mut payload = vec![0; packet.len()];
        self.state.read_message(&packet[HEADER_LEN..], &mut payload)?;
        Channel::new(self.state, true, self.own, self.server)
    }
}

pub fn accept(keypair: &StaticKeypair, packet: &[u8]) -> Result<Accepted, Error> {
    let own = ObfsKey::for_receiver(&keypair.public());
    let mut packet = packet.to_vec();
    if own.open(&mut packet)? != (Header::HandshakeInit { sender: 0 }) {
        return Err(Error::Unexpected);
    }
    let mut state = noise::builder(keypair).build_responder()?;
    let mut payload = vec![0; packet.len()];
    let len = state.read_message(&packet[HEADER_LEN..], &mut payload)?;
    let timestamp = payload[..len].get(..TAI64N_LEN).and_then(Tai64N::from_slice).ok_or(Error::InvalidPayload)?;
    let remote =
        state.get_remote_static().and_then(|key| PublicKey::from_slice(key).ok()).ok_or(Error::InvalidPayload)?;

    let padding = vec![0; rand::rng().random_range(0..=MAX_HANDSHAKE_PADDING)];
    let response = handshake_packet(&mut state, Header::HandshakeResp { sender: 0, receiver: 0 }, &padding)?;
    let frame = handshake_frame(response, &ObfsKey::for_receiver(&remote))?;
    let channel = Channel::new(state, false, own, remote)?;
    Ok(Accepted { channel, timestamp, frame })
}

pub fn handshake_length(own: &ObfsKey, masked: [u8; 2]) -> Option<usize> {
    let mask = own.handshake_length_mask();
    let len = usize::from(u16::from_be_bytes([masked[0] ^ mask[0], masked[1] ^ mask[1]]));
    (MIN_PACKET_LEN..=MAX_HANDSHAKE_FRAME_LEN).contains(&len).then_some(len)
}

impl Channel {
    fn new(state: HandshakeState, initiator: bool, own: ObfsKey, remote: PublicKey) -> Result<Self, Error> {
        let hash = state.get_handshake_hash().to_vec();
        let transport = Arc::new(state.into_stateless_transport_mode()?);
        let (send_label, recv_label): (&[u8], &[u8]) =
            if initiator { (b"weft-len-i2r", b"weft-len-r2i") } else { (b"weft-len-r2i", b"weft-len-i2r") };
        Ok(Channel {
            sender: Sender {
                transport: transport.clone(),
                obfs: ObfsKey::for_receiver(&remote),
                counter: 0,
                length: LengthMask::new(derive(&hash, send_label)),
            },
            receiver: Receiver {
                transport,
                obfs: own,
                counter: 0,
                length: LengthMask::new(derive(&hash, recv_label)),
                remote,
            },
        })
    }

    pub fn remote(&self) -> PublicKey {
        self.receiver.remote
    }

    pub fn split(self) -> (Sender, Receiver) {
        (self.sender, self.receiver)
    }

    pub fn seal(&mut self, message: &[u8]) -> Result<Vec<u8>, Error> {
        self.sender.seal(message)
    }

    pub fn read_length(&mut self, masked: [u8; 2]) -> Option<usize> {
        self.receiver.read_length(masked)
    }

    pub fn open(&mut self, packet: &[u8]) -> Result<Vec<u8>, Error> {
        self.receiver.open(packet)
    }
}

impl Sender {
    pub fn seal(&mut self, message: &[u8]) -> Result<Vec<u8>, Error> {
        if message.len() > MAX_MESSAGE_LEN {
            return Err(Error::TooLarge);
        }
        let mut packet = vec![0; HEADER_LEN + message.len() + TAG_LEN];
        packet[..HEADER_LEN].copy_from_slice(&Header::Data { receiver: 0, counter: self.counter }.encode());
        self.transport.write_message(self.counter, message, &mut packet[HEADER_LEN..])?;
        self.counter += 1;
        self.obfs.seal(&mut packet)?;
        let mut length = (packet.len() as u16).to_be_bytes();
        self.length.apply(&mut length);
        Ok([length.as_slice(), &packet].concat())
    }
}

impl Receiver {
    pub fn remote(&self) -> PublicKey {
        self.remote
    }

    pub fn read_length(&mut self, mut masked: [u8; 2]) -> Option<usize> {
        self.length.apply(&mut masked);
        let len = usize::from(u16::from_be_bytes(masked));
        (len >= MIN_PACKET_LEN).then_some(len)
    }

    pub fn open(&mut self, packet: &[u8]) -> Result<Vec<u8>, Error> {
        let mut packet = packet.to_vec();
        let Header::Data { receiver: 0, counter } = self.obfs.open(&mut packet)? else {
            return Err(Error::Unexpected);
        };
        if counter != self.counter {
            return Err(Error::Replay);
        }
        let mut message = vec![0; packet.len()];
        let len = self.transport.read_message(counter, &packet[HEADER_LEN..], &mut message)?;
        self.counter += 1;
        message.truncate(len);
        Ok(message)
    }
}

fn handshake_packet(state: &mut HandshakeState, header: Header, payload: &[u8]) -> Result<Vec<u8>, Error> {
    let mut packet = vec![0; MAX_HANDSHAKE_FRAME_LEN];
    let len = state.write_message(payload, &mut packet[HEADER_LEN..])?;
    packet.truncate(HEADER_LEN + len);
    packet[..HEADER_LEN].copy_from_slice(&header.encode());
    Ok(packet)
}

fn handshake_frame(mut packet: Vec<u8>, receiver: &ObfsKey) -> Result<Vec<u8>, Error> {
    receiver.seal(&mut packet)?;
    let mask = receiver.handshake_length_mask();
    let len = (packet.len() as u16).to_be_bytes();
    Ok([&[len[0] ^ mask[0], len[1] ^ mask[1]], packet.as_slice()].concat())
}

fn derive(hash: &[u8], label: &[u8]) -> [u8; 32] {
    let mut mac = <Blake2sMac256 as KeyInit>::new_from_slice(hash).expect("handshake hash fits blake2s key");
    mac.update(label);
    mac.finalize().into_bytes().into()
}

#[cfg(feature = "tokio")]
pub mod io {
    use tokio::io::{AsyncRead, AsyncReadExt};
    use weft_proto::ObfsKey;

    use super::{MAX_FRAME_LEN, Receiver, handshake_length};
    use crate::error::Error;

    pub async fn read_handshake<R: AsyncRead + Unpin>(reader: &mut R, own: &ObfsKey) -> Result<Vec<u8>, Error> {
        let mut masked = [0; 2];
        reader.read_exact(&mut masked).await?;
        let len = handshake_length(own, masked).ok_or(Error::Unexpected)?;
        let mut packet = vec![0; len];
        reader.read_exact(&mut packet).await?;
        Ok(packet)
    }

    pub async fn read_message<R: AsyncRead + Unpin>(reader: &mut R, receiver: &mut Receiver) -> Result<Vec<u8>, Error> {
        let mut masked = [0; 2];
        reader.read_exact(&mut masked).await?;
        let len = receiver.read_length(masked).filter(|&len| len <= MAX_FRAME_LEN).ok_or(Error::Unexpected)?;
        let mut packet = vec![0; len];
        reader.read_exact(&mut packet).await?;
        receiver.open(&packet)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    fn keys() -> (StaticKeypair, StaticKeypair) {
        (StaticKeypair::from_secret(&[1; 32]), StaticKeypair::from_secret(&[2; 32]))
    }

    fn wall() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    fn established() -> (Channel, Channel) {
        let (client, server) = keys();
        let (handshake, frame) = connect(&client, server.public(), wall()).unwrap();
        let own = ObfsKey::for_receiver(&server.public());
        let len = handshake_length(&own, [frame[0], frame[1]]).unwrap();
        assert_eq!(len, frame.len() - 2);
        let accepted = accept(&server, &frame[2..]).unwrap();
        assert_eq!(accepted.channel.remote(), client.public());
        assert_eq!(accepted.timestamp, Tai64N::from_system_time(wall()));
        let len = handshake_length(handshake.own_obfs(), [accepted.frame[0], accepted.frame[1]]).unwrap();
        assert_eq!(len, accepted.frame.len() - 2);
        let channel = handshake.finish(&accepted.frame[2..]).unwrap();
        assert_eq!(channel.remote(), server.public());
        (channel, accepted.channel)
    }

    fn transfer(from: &mut Channel, to: &mut Channel, message: &[u8]) -> Vec<u8> {
        let frame = from.seal(message).unwrap();
        let len = to.read_length([frame[0], frame[1]]).unwrap();
        assert_eq!(len, frame.len() - 2);
        to.open(&frame[2..]).unwrap()
    }

    #[test]
    fn messages_flow_both_ways() {
        let (mut client, mut server) = established();
        for i in 0..20u8 {
            let message = vec![i; usize::from(i) * 50];
            assert_eq!(transfer(&mut client, &mut server, &message), message);
            assert_eq!(transfer(&mut server, &mut client, &message), message);
        }
    }

    #[test]
    fn frames_look_random() {
        let (mut client, _) = established();
        let a = client.seal(&[0; 64]).unwrap();
        let b = client.seal(&[0; 64]).unwrap();
        assert_ne!(a[..2], b[..2]);
        assert!(!a.windows(16).any(|w| w == [0; 16]));
    }

    #[test]
    fn out_of_order_is_rejected() {
        let (mut client, mut server) = established();
        client.seal(b"lost").unwrap();
        let second = client.seal(b"two").unwrap();
        assert!(matches!(server.open(&second[2..]), Err(Error::Replay)));
    }

    #[test]
    fn wrong_server_key_fails() {
        let (client, server) = keys();
        let other = StaticKeypair::from_secret(&[3; 32]);
        let (_, frame) = connect(&client, other.public(), wall()).unwrap();
        assert!(accept(&server, &frame[2..]).is_err());
    }

    #[test]
    fn too_large() {
        let (mut client, _) = established();
        assert!(matches!(client.seal(&vec![0; MAX_MESSAGE_LEN + 1]), Err(Error::TooLarge)));
        assert!(client.seal(&vec![0; MAX_MESSAGE_LEN]).is_ok());
    }
}
