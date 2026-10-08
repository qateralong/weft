use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NatId(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatKind {
    /// One public port per internal socket, the local port when it is free.
    EndpointIndependent(Filtering),
    /// A new public port for every destination.
    Symmetric,
    /// Linux conntrack masquerade: keeps the source port and filters by address and port,
    /// but an unsolicited inbound packet leaves an entry for 30 s that forces a different
    /// public port for a later outbound flow to the same remote.
    Conntrack,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filtering {
    Open,
    Address,
    AddressAndPort,
}

struct Nat {
    public: IpAddr,
    kind: NatKind,
    hairpin: bool,
    mappings: HashMap<(SocketAddr, Option<SocketAddr>), u16>,
    reverse: HashMap<u16, SocketAddr>,
    contacted: HashSet<(u16, SocketAddr)>,
    stale: HashMap<(SocketAddr, u16), Instant>,
    next_port: u16,
}

const STALE_TIMEOUT: Duration = Duration::from_secs(30);

struct InFlight {
    from: SocketAddr,
    to: SocketAddr,
    data: Vec<u8>,
}

pub struct Network {
    now: Instant,
    latency: Duration,
    nats: Vec<Nat>,
    hosts: HashMap<SocketAddr, Option<NatId>>,
    queue: BTreeMap<(Instant, u64), InFlight>,
    seq: u64,
    blocked: HashSet<(IpAddr, IpAddr)>,
    delays: HashMap<IpAddr, Duration>,
    pub delivered: u64,
    pub dropped: u64,
}

impl Network {
    pub fn new(now: Instant, latency: Duration) -> Self {
        Self {
            now,
            latency,
            nats: Vec::new(),
            hosts: HashMap::new(),
            queue: BTreeMap::new(),
            seq: 0,
            blocked: HashSet::new(),
            delays: HashMap::new(),
            delivered: 0,
            dropped: 0,
        }
    }

    pub fn now(&self) -> Instant {
        self.now
    }

    pub fn add_nat(&mut self, public: IpAddr, kind: NatKind) -> NatId {
        self.nats.push(Nat {
            public,
            kind,
            hairpin: false,
            mappings: HashMap::new(),
            reverse: HashMap::new(),
            contacted: HashSet::new(),
            stale: HashMap::new(),
            next_port: 40_000,
        });
        NatId(self.nats.len() - 1)
    }

    pub fn add_host(&mut self, addr: SocketAddr, nat: Option<NatId>) {
        self.hosts.insert(addr, nat);
    }

    /// Adds extra one-way delay to every packet sent from or to this public address.
    pub fn set_delay(&mut self, ip: IpAddr, delay: Duration) {
        self.delays.insert(ip, delay);
    }

    /// Drops all traffic between two public IP addresses in both directions.
    pub fn block(&mut self, a: IpAddr, b: IpAddr) {
        self.blocked.insert((a, b));
        self.blocked.insert((b, a));
    }

    pub fn unblock(&mut self, a: IpAddr, b: IpAddr) {
        self.blocked.remove(&(a, b));
        self.blocked.remove(&(b, a));
    }

    pub fn send(&mut self, from: SocketAddr, to: SocketAddr, data: Vec<u8>) {
        let Some(&nat) = self.hosts.get(&from) else {
            self.dropped += 1;
            return;
        };
        let same_lan = self.hosts.get(&to).is_some_and(|&other| other.is_some() && other == nat);
        let source = match nat {
            Some(nat) if !same_lan => self.translate_out(nat, from, to),
            _ => from,
        };
        if self.blocked.contains(&(source.ip(), to.ip())) {
            self.dropped += 1;
            return;
        }
        self.seq += 1;
        let delay = |ip: IpAddr| self.delays.get(&ip).copied().unwrap_or_default();
        let at = self.now + self.latency + delay(source.ip()) + delay(to.ip());
        self.queue.insert((at, self.seq), InFlight { from: source, to, data });
    }

    pub fn next_delivery(&self) -> Option<Instant> {
        self.queue.keys().next().map(|&(at, _)| at)
    }

    pub fn advance(&mut self, to: Instant) {
        self.now = self.now.max(to);
    }

    /// Returns the next packet due by now as (receiving host, apparent source, data).
    pub fn pop_due(&mut self) -> Option<(SocketAddr, SocketAddr, Vec<u8>)> {
        loop {
            let (&(at, seq), _) = self.queue.iter().next()?;
            if at > self.now {
                return None;
            }
            let packet = self.queue.remove(&(at, seq)).expect("key exists");
            match self.translate_in(packet.from, packet.to) {
                Some(host) => {
                    self.delivered += 1;
                    return Some((host, packet.from, packet.data));
                }
                None => self.dropped += 1,
            }
        }
    }

    fn translate_out(&mut self, nat: NatId, from: SocketAddr, to: SocketAddr) -> SocketAddr {
        let now = self.now;
        let nat = &mut self.nats[nat.0];
        let key = match nat.kind {
            NatKind::EndpointIndependent(_) => (from, None),
            NatKind::Symmetric | NatKind::Conntrack => (from, Some(to)),
        };
        let port = match nat.mappings.get(&key) {
            Some(&port) => port,
            None => {
                let preserved = from.port();
                let clash = nat.stale.get(&(to, preserved)).is_some_and(|&until| now < until);
                let taken = nat.reverse.get(&preserved).is_some_and(|&owner| owner != from);
                let preserves = matches!(nat.kind, NatKind::Conntrack | NatKind::EndpointIndependent(_));
                let port = if preserves && !clash && !taken {
                    preserved
                } else {
                    nat.next_port += 1;
                    nat.next_port
                };
                nat.mappings.insert(key, port);
                nat.reverse.insert(port, from);
                port
            }
        };
        nat.contacted.insert((port, to));
        SocketAddr::new(nat.public, port)
    }

    fn translate_in(&mut self, from: SocketAddr, to: SocketAddr) -> Option<SocketAddr> {
        let now = self.now;
        if let Some(nat) = self.nats.iter_mut().find(|nat| nat.public == to.ip()) {
            if from.ip() == nat.public && !nat.hairpin {
                return None;
            }
            let host = nat.reverse.get(&to.port()).copied();
            let allowed = host.is_some()
                && match nat.kind {
                    NatKind::EndpointIndependent(Filtering::Open) => true,
                    NatKind::EndpointIndependent(Filtering::Address) => {
                        nat.contacted.iter().any(|&(port, remote)| port == to.port() && remote.ip() == from.ip())
                    }
                    NatKind::EndpointIndependent(Filtering::AddressAndPort)
                    | NatKind::Symmetric
                    | NatKind::Conntrack => nat.contacted.contains(&(to.port(), from)),
                };
            if !allowed && nat.kind == NatKind::Conntrack {
                nat.stale.insert((from, to.port()), now + STALE_TIMEOUT);
            }
            return host.filter(|_| allowed);
        }
        match self.hosts.get(&to) {
            Some(None) => Some(to),
            Some(Some(nat)) => {
                let sender_nat = self.hosts.get(&from).copied().flatten();
                (sender_nat == Some(*nat)).then_some(to)
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn deliver_all(net: &mut Network) -> Vec<(SocketAddr, SocketAddr, Vec<u8>)> {
        let mut out = Vec::new();
        while let Some(at) = net.next_delivery() {
            net.advance(at);
            while let Some(packet) = net.pop_due() {
                out.push(packet);
            }
        }
        out
    }

    #[test]
    fn port_restricted_nat_needs_outgoing_packet_first() {
        let mut net = Network::new(Instant::now(), Duration::from_millis(10));
        let nat = net.add_nat("198.51.100.1".parse().unwrap(), NatKind::EndpointIndependent(Filtering::AddressAndPort));
        let inside = addr("192.168.1.2:5000");
        let server = addr("203.0.113.1:443");
        let stranger = addr("203.0.113.2:443");
        net.add_host(inside, Some(nat));
        net.add_host(server, None);
        net.add_host(stranger, None);

        net.send(inside, server, vec![1]);
        let delivered = deliver_all(&mut net);
        assert_eq!(delivered.len(), 1);
        let mapped = delivered[0].1;
        assert_eq!(mapped.ip(), "198.51.100.1".parse::<IpAddr>().unwrap());

        net.send(server, mapped, vec![2]);
        net.send(stranger, mapped, vec![3]);
        let delivered = deliver_all(&mut net);
        assert_eq!(delivered, vec![(inside, server, vec![2])]);
    }

    #[test]
    fn symmetric_nat_uses_new_port_per_destination() {
        let mut net = Network::new(Instant::now(), Duration::from_millis(10));
        let nat = net.add_nat("198.51.100.1".parse().unwrap(), NatKind::Symmetric);
        let inside = addr("192.168.1.2:5000");
        net.add_host(inside, Some(nat));
        for remote in ["203.0.113.1:1", "203.0.113.2:1"] {
            net.add_host(addr(remote), None);
            net.send(inside, addr(remote), vec![]);
        }
        let ports: HashSet<u16> = deliver_all(&mut net).iter().map(|(_, from, _)| from.port()).collect();
        assert_eq!(ports.len(), 2);
    }

    #[test]
    fn conntrack_clash_changes_the_port() {
        let mut net = Network::new(Instant::now(), Duration::from_millis(10));
        let nat = net.add_nat("198.51.100.1".parse().unwrap(), NatKind::Conntrack);
        let inside = addr("192.168.1.2:5000");
        let server = addr("203.0.113.1:443");
        let peer = addr("203.0.113.2:7000");
        net.add_host(inside, Some(nat));
        net.add_host(server, None);
        net.add_host(peer, None);

        net.send(inside, server, vec![]);
        assert_eq!(deliver_all(&mut net)[0].1, addr("198.51.100.1:5000"));
        net.send(peer, addr("198.51.100.1:5000"), vec![]);
        assert!(deliver_all(&mut net).is_empty());
        net.send(inside, peer, vec![]);
        let delivered = deliver_all(&mut net);
        assert_ne!(delivered[0].1.port(), 5000);

        net.advance(net.now() + STALE_TIMEOUT);
        let other = addr("203.0.113.3:7000");
        net.add_host(other, None);
        net.send(inside, other, vec![]);
        assert_eq!(deliver_all(&mut net)[0].1, addr("198.51.100.1:5000"));
    }

    #[test]
    fn hosts_behind_the_same_nat_reach_each_other_directly() {
        let mut net = Network::new(Instant::now(), Duration::from_millis(1));
        let nat = net.add_nat("198.51.100.1".parse().unwrap(), NatKind::Symmetric);
        let a = addr("192.168.1.2:5000");
        let b = addr("192.168.1.3:5000");
        let outside = addr("10.0.0.1:1");
        net.add_host(a, Some(nat));
        net.add_host(b, Some(nat));
        net.add_host(outside, None);
        net.send(a, b, vec![7]);
        net.send(outside, b, vec![8]);
        assert_eq!(deliver_all(&mut net), vec![(b, a, vec![7])]);
    }
}
