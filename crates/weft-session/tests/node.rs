use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use weft_proto::PacketError;
use weft_session::{Error, Event, Node, PeerId, Received, StaticKeypair};

struct Net {
    now: Instant,
    a: Node,
    b: Node,
    a_to_b: PeerId,
    b_to_a: PeerId,
}

fn wall() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

fn node(secret: u8, now: Instant) -> Node {
    Node::with_seed(StaticKeypair::from_secret(&[secret; 32]), now, wall(), u64::from(secret))
}

fn ipv4(payload: &[u8]) -> Vec<u8> {
    let total = 20 + payload.len();
    let mut packet = vec![0; total];
    packet[0] = 0x45;
    packet[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    packet[20..].copy_from_slice(payload);
    packet
}

impl Net {
    fn new() -> Self {
        let now = Instant::now();
        let mut a = node(1, now);
        let mut b = node(2, now);
        let a_to_b = a.add_peer(now, b.public_key());
        let b_to_a = b.add_peer(now, a.public_key());
        Net { now, a, b, a_to_b, b_to_a }
    }

    fn connected() -> Self {
        let mut net = Net::new();
        net.exchange();
        assert!(net.a.is_established(net.a_to_b));
        assert!(net.b.is_established(net.b_to_a));
        net
    }

    fn exchange(&mut self) -> (Vec<Received>, Vec<Received>) {
        let (mut at_a, mut at_b) = (Vec::new(), Vec::new());
        loop {
            let from_a = deliver(&mut self.a, &mut self.b, self.now);
            let from_b = deliver(&mut self.b, &mut self.a, self.now);
            if from_a.is_empty() && from_b.is_empty() {
                return (at_a, at_b);
            }
            at_b.extend(from_a.into_iter().map(Result::unwrap));
            at_a.extend(from_b.into_iter().map(Result::unwrap));
        }
    }

    fn advance(&mut self, by: Duration) {
        self.now += by;
        self.a.tick(self.now);
        self.b.tick(self.now);
    }
}

fn deliver(from: &mut Node, to: &mut Node, now: Instant) -> Vec<Result<Received, Error>> {
    std::iter::from_fn(|| from.poll_transmit()).map(|t| to.receive(now, &t.datagram)).collect()
}

fn events(node: &mut Node) -> Vec<Event> {
    std::iter::from_fn(|| node.poll_event()).collect()
}

fn packets(received: &[Received]) -> Vec<Vec<u8>> {
    received.iter().filter_map(|r| r.packet.clone()).collect()
}

#[test]
fn handshake_and_data_both_ways() {
    let mut net = Net::connected();
    assert!(events(&mut net.a).contains(&Event::Established(net.a_to_b)));
    assert!(events(&mut net.b).contains(&Event::Established(net.b_to_a)));

    let ping = ipv4(b"ping");
    let pong = ipv4(b"pong, a bit longer");
    net.a.send(net.now, net.a_to_b, &ping).unwrap();
    net.b.send(net.now, net.b_to_a, &pong).unwrap();
    let (at_a, at_b) = net.exchange();
    assert_eq!(packets(&at_b), vec![ping]);
    assert_eq!(packets(&at_a), vec![pong]);
    assert!(at_b.iter().all(|r| r.peer == net.b_to_a));
}

#[test]
fn packets_sent_before_handshake_are_queued() {
    let mut net = Net::new();
    let packets_out: Vec<_> = (0..3u8).map(|i| ipv4(&[i; 10])).collect();
    for packet in &packets_out {
        net.a.send(net.now, net.a_to_b, packet).unwrap();
    }
    let (_, at_b) = net.exchange();
    assert_eq!(packets(&at_b), packets_out);
}

#[test]
fn replayed_data_is_rejected() {
    let mut net = Net::connected();
    net.a.send(net.now, net.a_to_b, &ipv4(b"once")).unwrap();
    let datagram = net.a.poll_transmit().unwrap().datagram;
    assert!(net.b.receive(net.now, &datagram).is_ok());
    assert!(matches!(net.b.receive(net.now, &datagram), Err(Error::Replay)));
}

#[test]
fn replayed_handshake_is_rejected() {
    let now = Instant::now();
    let mut a = node(1, now);
    let mut b = node(2, now);
    a.add_peer(now, b.public_key());
    b.add_peer(now, a.public_key());
    while b.poll_transmit().is_some() {}
    let init = a.poll_transmit().unwrap().datagram;
    assert!(b.receive(now, &init).is_ok());
    assert!(matches!(b.receive(now, &init), Err(Error::StaleHandshake)));
}

#[test]
fn unknown_peer_is_rejected() {
    let now = Instant::now();
    let mut a = node(1, now);
    let mut b = node(2, now);
    a.add_peer(now, b.public_key());
    let init = a.poll_transmit().unwrap().datagram;
    assert!(matches!(b.receive(now, &init), Err(Error::UnknownPeer)));
}

#[test]
fn packet_for_another_node_is_rejected() {
    let mut net = Net::connected();
    let mut c = node(3, net.now);
    net.a.send(net.now, net.a_to_b, &ipv4(b"secret")).unwrap();
    let datagram = net.a.poll_transmit().unwrap().datagram;
    assert!(matches!(c.receive(net.now, &datagram), Err(Error::Packet(PacketError::InvalidHeader))));
}

#[test]
fn tampered_data_is_rejected() {
    let mut net = Net::connected();
    net.a.send(net.now, net.a_to_b, &ipv4(b"payload")).unwrap();
    let mut datagram = net.a.poll_transmit().unwrap().datagram;
    datagram[20] ^= 1;
    assert!(matches!(net.b.receive(net.now, &datagram), Err(Error::Noise(_))));
}

#[test]
fn garbage_is_rejected() {
    let mut net = Net::connected();
    for len in [0, 10, 31, 32, 100, 1400] {
        let garbage: Vec<u8> = (0..len).map(|i| (i * 131 + 7) as u8).collect();
        assert!(net.b.receive(net.now, &garbage).is_err());
    }
}

#[test]
fn invalid_outgoing_packet_is_rejected() {
    let mut net = Net::connected();
    assert!(net.a.send(net.now, net.a_to_b, &[0x45; 10]).is_err());
    let mut padded = ipv4(b"x");
    padded.push(0);
    assert!(net.a.send(net.now, net.a_to_b, &padded).is_err());
    assert!(net.a.send(net.now, net.a_to_b, &[]).is_err());
}

#[test]
fn handshakes_have_random_sizes() {
    let now = Instant::now();
    let mut a = node(1, now);
    let b = node(2, now);
    let id = a.add_peer(now, b.public_key());
    let mut sizes = std::collections::HashSet::new();
    let mut t = now;
    for _ in 0..10 {
        sizes.insert(a.poll_transmit().unwrap().datagram.len());
        t += Duration::from_secs(6);
        a.tick(t);
    }
    assert!(sizes.len() > 5, "{sizes:?}");
    assert!(a.peer_key(id).is_some());
}

#[test]
fn keepalives_flow_and_are_not_delivered_as_packets() {
    let mut net = Net::connected();
    net.advance(Duration::from_secs(31));
    let (at_a, at_b) = net.exchange();
    assert!(!at_a.is_empty() && !at_b.is_empty());
    assert!(packets(&at_a).is_empty() && packets(&at_b).is_empty());
}

#[test]
fn lost_handshake_is_retried() {
    let now = Instant::now();
    let mut a = node(1, now);
    let b = node(2, now);
    a.add_peer(now, b.public_key());
    assert!(a.poll_transmit().is_some());
    a.tick(now + Duration::from_secs(4));
    assert!(a.poll_transmit().is_none());
    let retry = a.next_timeout().unwrap();
    assert!(retry >= now + Duration::from_secs(5) && retry <= now + Duration::from_millis(5333));
    a.tick(retry);
    assert!(a.poll_transmit().is_some());
}

#[test]
fn silent_peer_becomes_unreachable() {
    let now = Instant::now();
    let mut a = node(1, now);
    let b = node(2, now);
    let id = a.add_peer(now, b.public_key());
    let mut t = now;
    while t < now + Duration::from_secs(100) {
        t += Duration::from_secs(1);
        a.tick(t);
        while a.poll_transmit().is_some() {}
    }
    assert_eq!(events(&mut a), vec![Event::Unreachable(id)]);
}

#[test]
fn long_running_session_rekeys_and_keeps_working() {
    let mut net = Net::connected();
    for _ in 0..(600 / 5) {
        net.advance(Duration::from_secs(5));
        net.exchange();
        assert!(net.a.is_established(net.a_to_b));
    }
    let mut established = 0;
    while let Some(event) = net.a.poll_event() {
        assert!(matches!(event, Event::Established(_)));
        established += 1;
    }
    assert!(established >= 6, "{established}");

    let packet = ipv4(b"still here");
    net.a.send(net.now, net.a_to_b, &packet).unwrap();
    let (_, at_b) = net.exchange();
    assert_eq!(packets(&at_b), vec![packet]);
}

#[test]
fn expired_session_is_replaced() {
    let mut net = Net::connected();
    net.now += Duration::from_secs(181);
    net.a.send(net.now, net.a_to_b, &ipv4(b"late")).unwrap();
    net.a.tick(net.now);
    net.b.tick(net.now);
    let (_, at_b) = net.exchange();
    assert_eq!(packets(&at_b), vec![ipv4(b"late")]);
}

#[test]
fn removed_peer_is_forgotten() {
    let mut net = Net::connected();
    assert!(net.b.remove_peer(net.b_to_a));
    net.a.send(net.now, net.a_to_b, &ipv4(b"gone")).unwrap();
    let datagram = net.a.poll_transmit().unwrap().datagram;
    assert!(matches!(net.b.receive(net.now, &datagram), Err(Error::UnknownIndex)));
    assert!(net.b.peer_id(&net.a.public_key()).is_none());
}
