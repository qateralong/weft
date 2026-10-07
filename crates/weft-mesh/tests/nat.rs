use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant, SystemTime};

use weft_mesh::{Delivered, Mesh, Output, PeerConfig, PeerLink};
use weft_proto::PublicKey;
use weft_session::StaticKeypair;
use weft_sim::{Delivery, FakeLoom, Filtering, NatId, NatKind, Network};

struct Agent {
    mesh: Mesh,
    local: SocketAddr,
    key: PublicKey,
    address: Ipv4Addr,
    inbox: Vec<Delivered>,
}

struct World {
    net: Network,
    loom: FakeLoom,
    agents: Vec<Agent>,
    next_sync: Vec<Instant>,
}

const SYNC_INTERVAL: Duration = Duration::from_secs(1);
const SYNC_SKEW: Duration = Duration::from_millis(7);

const LOOM: &str = "203.0.113.1:443";

fn ipv4(source: Ipv4Addr, destination: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
    let total = 20 + payload.len();
    let mut packet = vec![0; total];
    packet[0] = 0x45;
    packet[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    packet[12..16].copy_from_slice(&source.octets());
    packet[16..20].copy_from_slice(&destination.octets());
    packet[20..].copy_from_slice(payload);
    packet
}

impl World {
    fn new() -> Self {
        let now = Instant::now();
        let mut net = Network::new(now, Duration::from_micros(500));
        let loom_addr: SocketAddr = LOOM.parse().unwrap();
        net.add_host(loom_addr, None);
        net.set_delay(loom_addr.ip(), Duration::from_millis(10));
        let loom = FakeLoom::new(loom_addr, &StaticKeypair::from_secret(&[200; 32]));
        Self { net, loom, agents: Vec::new(), next_sync: Vec::new() }
    }

    fn nat(&mut self, public: &str, kind: NatKind) -> NatId {
        self.net.add_nat(public.parse().unwrap(), kind)
    }

    fn agent(&mut self, local: &str, nat: Option<NatId>) -> usize {
        let n = self.agents.len() as u8 + 1;
        let keypair = StaticKeypair::from_secret(&[n; 32]);
        let key = keypair.public();
        let address = Ipv4Addr::new(100, 64, 0, n);
        let token = self.loom.register(key, address);
        let now = self.net.now();
        let mut mesh = Mesh::with_seed(keypair, now, SystemTime::now(), u64::from(n));
        mesh.set_server(now, self.loom.addr, self.loom.public_key(), token);
        let local: SocketAddr = local.parse().unwrap();
        self.net.add_host(local, nat);
        self.agents.push(Agent { mesh, local, key, address, inbox: Vec::new() });
        self.next_sync.push(now + SYNC_SKEW * n as u32);
        self.agents.len() - 1
    }

    fn sync(&mut self, i: usize) {
        let now = self.net.now();
        let configs: Vec<PeerConfig> = self
            .agents
            .iter()
            .map(|agent| PeerConfig {
                key: agent.key,
                address: agent.address,
                online: true,
                candidates: self.loom.endpoint(agent.address).into_iter().chain([agent.local]).collect::<Vec<_>>(),
            })
            .collect();
        self.agents[i].mesh.update_peers(now, configs);
        self.next_sync[i] = now + SYNC_INTERVAL;
    }

    fn candidates(&self, i: usize) -> Vec<SocketAddr> {
        let agent = &self.agents[i];
        self.loom.endpoint(agent.address).into_iter().chain([agent.local]).collect()
    }

    fn by_address(&self, address: Ipv4Addr) -> Option<usize> {
        self.agents.iter().position(|agent| agent.address == address)
    }

    fn deliver_tcp(&mut self, to: Ipv4Addr, source: Ipv4Addr, packet: &[u8]) {
        let now = self.net.now();
        if let Some(j) = self.by_address(to)
            && let Some(delivered) = self.agents[j].mesh.receive_tcp_relay(now, source, packet)
        {
            self.agents[j].inbox.push(delivered);
        }
    }

    fn pump(&mut self) {
        let now = self.net.now();
        loop {
            let mut busy = false;
            for i in 0..self.agents.len() {
                while let Some(output) = self.agents[i].mesh.poll_output() {
                    busy = true;
                    match output {
                        Output::Udp { to, datagram } => self.net.send(self.agents[i].local, to, datagram),
                        Output::TcpRelay { to, packet } => {
                            let source = self.agents[i].address;
                            match self.loom.relay_tcp(source, to, &packet) {
                                Some(Delivery::Udp { to, datagram }) => self.net.send(self.loom.addr, to, datagram),
                                Some(Delivery::Tcp { to, source, packet }) => self.deliver_tcp(to, source, &packet),
                                None => {}
                            }
                        }
                        Output::CallMeMaybe { peer } => {
                            let caller = self.agents[i].key;
                            if let Some(j) = self.agents.iter().position(|agent| agent.key == peer) {
                                let of_i = self.candidates(i);
                                let of_j = self.candidates(j);
                                self.agents[j].mesh.call_me_maybe(now, &caller, of_i);
                                self.agents[i].mesh.call_me_maybe(now, &peer, of_j);
                            }
                        }
                    }
                }
            }
            if !busy {
                return;
            }
        }
    }

    fn run_for(&mut self, duration: Duration) {
        let end = self.net.now() + duration;
        let mut idle_rounds = 0;
        loop {
            self.pump();
            for i in 0..self.agents.len() {
                if self.net.now() >= self.next_sync[i] {
                    self.sync(i);
                    self.pump();
                }
            }
            let next = self
                .agents
                .iter()
                .filter_map(|agent| agent.mesh.next_timeout())
                .chain(self.net.next_delivery())
                .chain(self.next_sync.iter().copied())
                .chain([end])
                .min()
                .unwrap();
            idle_rounds = if next <= self.net.now() { idle_rounds + 1 } else { 0 };
            assert!(idle_rounds < 1000, "simulation does not advance at {:?}", next);
            self.net.advance(next);
            let now = self.net.now();
            while let Some((host, from, data)) = self.net.pop_due() {
                if host == self.loom.addr {
                    for delivery in self.loom.handle(from, &data) {
                        match delivery {
                            Delivery::Udp { to, datagram } => self.net.send(self.loom.addr, to, datagram),
                            Delivery::Tcp { to, source, packet } => self.deliver_tcp(to, source, &packet),
                        }
                    }
                } else if let Some(agent) = self.agents.iter_mut().find(|agent| agent.local == host)
                    && let Some(delivered) = agent.mesh.receive_udp(now, from, &data)
                {
                    agent.inbox.push(delivered);
                }
            }
            for agent in &mut self.agents {
                if agent.mesh.next_timeout().is_some_and(|at| at <= now) {
                    agent.mesh.tick(now);
                }
            }
            if now >= end {
                self.pump();
                return;
            }
        }
    }

    fn link(&self, from: usize, to: usize) -> Option<PeerLink> {
        self.agents[from].mesh.link(&self.agents[to].key)
    }

    fn exchange(&mut self, a: usize, b: usize) {
        let now = self.net.now();
        let (from, to) = (self.agents[a].address, self.agents[b].address);
        let ping = ipv4(from, to, b"ping");
        let pong = ipv4(to, from, b"pong");
        self.agents[a].inbox.clear();
        self.agents[b].inbox.clear();
        self.agents[a].mesh.send(now, &ping).unwrap();
        self.agents[b].mesh.send(now, &pong).unwrap();
        self.run_for(Duration::from_secs(1));
        assert_eq!(self.agents[b].inbox, vec![Delivered { source: from, packet: ping }]);
        assert_eq!(self.agents[a].inbox, vec![Delivered { source: to, packet: pong }]);
    }
}

fn direct(link: Option<PeerLink>) -> Option<SocketAddr> {
    match link {
        Some(PeerLink::Direct { addr, .. }) => Some(addr),
        _ => None,
    }
}

fn cone(filtering: Filtering) -> NatKind {
    NatKind::EndpointIndependent(filtering)
}

#[test]
fn public_hosts_connect_directly() {
    let mut world = World::new();
    let a = world.agent("198.51.100.10:5000", None);
    let b = world.agent("198.51.100.20:5000", None);
    world.run_for(Duration::from_secs(5));
    assert_eq!(direct(world.link(a, b)), Some("198.51.100.20:5000".parse().unwrap()));
    assert_eq!(direct(world.link(b, a)), Some("198.51.100.10:5000".parse().unwrap()));
    world.exchange(a, b);
}

#[test]
fn port_restricted_cones_punch_through() {
    let mut world = World::new();
    let nat_a = world.nat("198.51.100.1", cone(Filtering::AddressAndPort));
    let nat_b = world.nat("198.51.100.2", cone(Filtering::AddressAndPort));
    let a = world.agent("192.168.1.10:5000", Some(nat_a));
    let b = world.agent("192.168.2.10:5000", Some(nat_b));
    world.run_for(Duration::from_secs(10));
    let path = direct(world.link(a, b)).expect("direct path");
    assert_eq!(path.ip(), "198.51.100.2".parse::<IpAddr>().unwrap());
    assert!(direct(world.link(b, a)).is_some());
    world.exchange(a, b);
}

#[test]
fn symmetric_nats_use_the_relay() {
    let mut world = World::new();
    let nat_a = world.nat("198.51.100.1", NatKind::Symmetric);
    let nat_b = world.nat("198.51.100.2", NatKind::Symmetric);
    let a = world.agent("192.168.1.10:5000", Some(nat_a));
    let b = world.agent("192.168.2.10:5000", Some(nat_b));
    world.run_for(Duration::from_secs(15));
    assert_eq!(world.link(a, b), Some(PeerLink::Relay));
    assert_eq!(world.link(b, a), Some(PeerLink::Relay));
    let before = world.loom.relayed;
    world.exchange(a, b);
    assert!(world.loom.relayed > before);
}

#[test]
fn symmetric_nat_reaches_an_open_cone_directly() {
    let mut world = World::new();
    let nat_a = world.nat("198.51.100.1", NatKind::Symmetric);
    let nat_b = world.nat("198.51.100.2", cone(Filtering::Open));
    let a = world.agent("192.168.1.10:5000", Some(nat_a));
    let b = world.agent("192.168.2.10:5000", Some(nat_b));
    world.run_for(Duration::from_secs(15));
    assert!(direct(world.link(a, b)).is_some());
    assert!(direct(world.link(b, a)).is_some());
    world.exchange(a, b);
}

#[test]
fn same_lan_uses_local_addresses() {
    let mut world = World::new();
    let nat = world.nat("198.51.100.1", NatKind::Symmetric);
    let a = world.agent("192.168.1.10:5000", Some(nat));
    let b = world.agent("192.168.1.20:5000", Some(nat));
    world.run_for(Duration::from_secs(10));
    assert_eq!(direct(world.link(a, b)), Some("192.168.1.20:5000".parse().unwrap()));
    assert_eq!(direct(world.link(b, a)), Some("192.168.1.10:5000".parse().unwrap()));
    world.exchange(a, b);
}

#[test]
fn lost_direct_path_falls_back_to_relay_and_recovers() {
    let mut world = World::new();
    let a = world.agent("198.51.100.10:5000", None);
    let b = world.agent("198.51.100.20:5000", None);
    world.run_for(Duration::from_secs(5));
    assert!(direct(world.link(a, b)).is_some());

    let (ip_a, ip_b) = ("198.51.100.10".parse().unwrap(), "198.51.100.20".parse().unwrap());
    world.net.block(ip_a, ip_b);
    world.run_for(Duration::from_secs(30));
    assert_eq!(world.link(a, b), Some(PeerLink::Relay));
    world.exchange(a, b);

    world.net.unblock(ip_a, ip_b);
    world.run_for(Duration::from_secs(50));
    assert!(direct(world.link(a, b)).is_some());
    world.exchange(a, b);
}

#[test]
fn tcp_relay_when_udp_to_loom_is_blocked() {
    let mut world = World::new();
    let nat_a = world.nat("198.51.100.1", NatKind::Symmetric);
    let nat_b = world.nat("198.51.100.2", NatKind::Symmetric);
    let a = world.agent("192.168.1.10:5000", Some(nat_a));
    let b = world.agent("192.168.2.10:5000", Some(nat_b));
    world.net.block("198.51.100.1".parse().unwrap(), "203.0.113.1".parse().unwrap());
    world.run_for(Duration::from_secs(15));
    assert_eq!(world.link(a, b), Some(PeerLink::Relay));
    assert!(world.agents[a].mesh.observed().is_none());
    world.exchange(a, b);
}

#[test]
fn many_peers_in_mixed_networks() {
    let mut world = World::new();
    let kinds = [None, Some(cone(Filtering::Open)), Some(cone(Filtering::AddressAndPort)), Some(NatKind::Symmetric)];
    let mut agents = Vec::new();
    for (i, kind) in kinds.iter().enumerate() {
        let nat = kind.map(|kind| world.nat(&format!("198.51.100.{}", i + 1), kind));
        let local = match nat {
            Some(_) => format!("192.168.{}.10:5000", i + 1),
            None => format!("198.51.100.{}:5000", 100 + i),
        };
        agents.push(world.agent(&local, nat));
    }
    world.run_for(Duration::from_secs(20));
    for &a in &agents {
        for &b in &agents {
            if a < b {
                assert!(world.link(a, b).is_some_and(|link| link != PeerLink::Connecting), "{a} -> {b}");
                world.exchange(a, b);
            }
        }
    }
}

#[test]
fn linux_conntrack_nats_punch_through() {
    let mut world = World::new();
    let nat_a = world.nat("198.51.100.1", NatKind::Conntrack);
    let nat_b = world.nat("198.51.100.2", NatKind::Conntrack);
    let a = world.agent("192.168.1.10:5000", Some(nat_a));
    let b = world.agent("192.168.2.10:6000", Some(nat_b));
    world.run_for(Duration::from_secs(15));
    assert!(direct(world.link(a, b)).is_some(), "{:?}", world.link(a, b));
    assert!(direct(world.link(b, a)).is_some(), "{:?}", world.link(b, a));
    world.exchange(a, b);
}

fn udp(source: Ipv4Addr, destination: Ipv4Addr, port: u16, payload: &[u8]) -> Vec<u8> {
    let total = 28 + payload.len();
    let mut packet = vec![0; total];
    packet[0] = 0x45;
    packet[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    packet[8] = 64;
    packet[9] = 17;
    packet[12..16].copy_from_slice(&source.octets());
    packet[16..20].copy_from_slice(&destination.octets());
    packet[20..22].copy_from_slice(&port.to_be_bytes());
    packet[22..24].copy_from_slice(&port.to_be_bytes());
    packet[24..26].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    packet[28..].copy_from_slice(payload);
    packet
}

#[test]
fn broadcast_and_multicast_reach_every_peer() {
    let mut world = World::new();
    let agents: Vec<usize> = (0..3).map(|i| world.agent(&format!("198.51.100.{}:5000", 10 + i), None)).collect();
    for &i in &agents {
        let address = world.agents[i].address;
        world.agents[i].mesh.set_local(address, 10);
    }
    world.run_for(Duration::from_secs(5));

    let now = world.net.now();
    let source = world.agents[0].address;
    let packets: Vec<Vec<u8>> = [Ipv4Addr::BROADCAST, Ipv4Addr::new(100, 127, 255, 255), Ipv4Addr::new(224, 0, 2, 60)]
        .into_iter()
        .map(|destination| udp(source, destination, 4445, b"lan game"))
        .collect();
    for packet in &packets {
        world.agents[0].mesh.send(now, packet).unwrap();
    }
    assert_eq!(
        world.agents[0].mesh.send(now, &udp(source, Ipv4Addr::BROADCAST, 137, b"netbios")),
        Err(weft_mesh::SendError::Filtered)
    );
    world.run_for(Duration::from_secs(1));
    for &i in &agents[1..] {
        let received: Vec<Vec<u8>> = world.agents[i].inbox.iter().map(|d| d.packet.clone()).collect();
        assert_eq!(received, packets, "agent {i}");
    }
    assert!(world.agents[0].inbox.is_empty());
}

#[test]
fn packets_for_a_foreign_destination_are_dropped() {
    let mut world = World::new();
    let a = world.agent("198.51.100.10:5000", None);
    let b = world.agent("198.51.100.20:5000", None);
    world.agents[b].mesh.set_local(Ipv4Addr::new(100, 64, 0, 99), 10);
    world.run_for(Duration::from_secs(5));
    let now = world.net.now();
    let (from, to) = (world.agents[a].address, world.agents[b].address);
    world.agents[a].mesh.send(now, &udp(from, to, 4000, b"hello")).unwrap();
    world.run_for(Duration::from_secs(1));
    assert!(world.agents[b].inbox.is_empty());
}
