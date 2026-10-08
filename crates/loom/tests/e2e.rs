use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, SystemTime};

use loom::Server;
use loom::config::Config;
use loom::db::Db;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpStream, UdpSocket};
use weft_proto::control::{
    self, Candidates, ClientKind, ClientMessage, ErrorCode, Hello, InviteCode, InviteRequest, MemberAction,
    NetworkCredentials, NetworkName, NetworkSettings, PROTOCOL_VERSION, PeerKey, RelayPacket, Role, RoleChange,
    ServerKind, ServerMessage, State, Welcome,
};
use weft_proto::loom::{parse_observed, parse_relayed, relay_packet};
use weft_proto::obfs::discover_packet;
use weft_proto::{Header, ObfsKey, PublicKey};
use weft_session::StaticKeypair;
use weft_session::stream::{self, Receiver, Sender, io};

struct Client {
    key: PublicKey,
    sender: Sender,
    receiver: Receiver,
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    next_id: u32,
}

impl Client {
    async fn connect(server: &Server, secret: u8) -> Client {
        let keypair = StaticKeypair::from_secret(&[secret; 32]);
        let stream = TcpStream::connect(server.tcp_addr).await.unwrap();
        let (mut reader, mut writer) = stream.into_split();
        let (handshake, frame) = stream::connect(&keypair, server.public_key, SystemTime::now()).unwrap();
        writer.write_all(&frame).await.unwrap();
        let packet = io::read_handshake(&mut reader, handshake.own_obfs()).await.unwrap();
        let (sender, receiver) = handshake.finish(&packet).unwrap().split();
        Client { key: keypair.public(), sender, receiver, reader, writer, next_id: 1 }
    }

    async fn send(&mut self, kind: ClientKind) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        let frame = self.sender.seal(&control::encode(&ClientMessage { id, kind: Some(kind) })).unwrap();
        self.writer.write_all(&frame).await.unwrap();
        id
    }

    async fn recv(&mut self) -> ServerMessage {
        let bytes =
            tokio::time::timeout(Duration::from_secs(10), io::read_message(&mut self.reader, &mut self.receiver))
                .await
                .expect("server answered in time")
                .unwrap();
        control::decode(&bytes).unwrap()
    }

    async fn reply(&mut self, id: u32) -> ServerKind {
        loop {
            let message = self.recv().await;
            if message.reply_to == id {
                return message.kind.unwrap();
            }
        }
    }

    async fn state_where(&mut self, check: impl Fn(&State) -> bool) -> State {
        loop {
            if let Some(ServerKind::State(state)) = self.recv().await.kind
                && check(&state)
            {
                return state;
            }
        }
    }

    async fn hello(&mut self, nickname: &str) -> Welcome {
        let id = self.send(ClientKind::Hello(Hello { version: PROTOCOL_VERSION, nickname: nickname.into() })).await;
        match self.reply(id).await {
            ServerKind::Welcome(welcome) => welcome,
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn request(&mut self, kind: ClientKind) -> Result<(), ErrorCode> {
        match self.ask(kind).await? {
            ServerKind::Ack(_) => Ok(()),
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn ask(&mut self, kind: ClientKind) -> Result<ServerKind, ErrorCode> {
        let id = self.send(kind).await;
        match self.reply(id).await {
            ServerKind::Failure(failure) => Err(failure.code()),
            other => Ok(other),
        }
    }

    async fn invite(&mut self, network: &str, max_uses: u32) -> Result<String, ErrorCode> {
        let request = InviteRequest { network: network.into(), max_uses, expires_in: 3600 };
        match self.ask(ClientKind::CreateInvite(request)).await? {
            ServerKind::Invite(invite) => Ok(invite.code),
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn redeem(&mut self, code: &str) -> Result<String, ErrorCode> {
        match self.ask(ClientKind::RedeemInvite(InviteCode { code: code.into() })).await? {
            ServerKind::Joined(network) => Ok(network.name),
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn credentials(name: &str, password: &str) -> NetworkCredentials {
    NetworkCredentials { name: name.into(), password: password.into() }
}

fn member(network: &str, member: &str) -> MemberAction {
    MemberAction { network: network.into(), member: member.into() }
}

async fn start() -> Server {
    start_with(Config::default()).await
}

async fn start_with(config: Config) -> Server {
    let config = Config { listen: "127.0.0.1:0".parse().unwrap(), ..config };
    let keypair = StaticKeypair::from_secret(&[100; 32]);
    Server::start(config, keypair, Db::open_in_memory().unwrap()).await.unwrap()
}

#[tokio::test]
async fn networks_membership_and_discovery() {
    let server = start().await;
    let mut a = Client::connect(&server, 1).await;
    let mut b = Client::connect(&server, 2).await;

    let welcome_a = a.hello("alice").await;
    assert_eq!(Ipv4Addr::from(welcome_a.address), Ipv4Addr::new(100, 64, 0, 1));
    assert_eq!(welcome_a.prefix_len, 10);
    assert_eq!(welcome_a.udp_port, u32::from(server.udp_addr.port()));
    let welcome_b = b.hello("bob").await;
    assert_eq!(Ipv4Addr::from(welcome_b.address), Ipv4Addr::new(100, 64, 0, 2));

    a.request(ClientKind::CreateNetwork(credentials("Φίλοι", "secret"))).await.unwrap();
    assert_eq!(
        a.request(ClientKind::CreateNetwork(credentials("φίλοι", "other"))).await,
        Err(ErrorCode::NetworkExists)
    );
    assert_eq!(b.request(ClientKind::JoinNetwork(credentials("Φίλοι", "wrong"))).await, Err(ErrorCode::WrongPassword));
    assert_eq!(b.request(ClientKind::JoinNetwork(credentials("nope", "x"))).await, Err(ErrorCode::NetworkNotFound));
    b.request(ClientKind::JoinNetwork(credentials("ΦΊΛΟΙ", "secret"))).await.unwrap();

    let state = a.state_where(|state| !state.peers.is_empty()).await;
    assert_eq!(state.networks[0].name, "Φίλοι");
    assert_eq!(state.networks[0].role(), Role::Owner);
    assert_eq!(state.peers[0].nickname, "bob");
    assert!(state.peers[0].online);
    assert_eq!(state.peers[0].endpoint, None);

    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let token: [u8; 16] = welcome_b.discovery_token.as_slice().try_into().unwrap();
    let mut packet = discover_packet(&token, 3, [7; 16]);
    ObfsKey::for_receiver(&server.public_key).seal(&mut packet).unwrap();
    let target = SocketAddr::new(server.udp_addr.ip(), welcome_b.udp_port as u16);
    udp.send_to(&packet, target).await.unwrap();
    let state = a.state_where(|state| state.peers.first().is_some_and(|peer| peer.endpoint.is_some())).await;
    assert_eq!(state.peers[0].endpoint.as_ref().unwrap().to_socket_addr(), Some(udp.local_addr().unwrap()));
    assert_eq!(state.peers[0].key, b.key.as_bytes().to_vec());

    drop(b);
    let state = a.state_where(|state| state.peers.first().is_some_and(|peer| !peer.online)).await;
    assert_eq!(state.peers[0].endpoint, None);

    let mut b = Client::connect(&server, 2).await;
    assert_eq!(b.hello("bobby").await.address, welcome_b.address);
    b.request(ClientKind::LeaveNetwork(NetworkName { name: "φίλοι".into() })).await.unwrap();
    let state = a.state_where(|state| state.peers.is_empty()).await;
    assert_eq!(state.networks[0].members.len(), 1);
    assert_eq!(
        b.request(ClientKind::LeaveNetwork(NetworkName { name: "φίλοι".into() })).await,
        Err(ErrorCode::NotMember)
    );
}

#[tokio::test]
async fn wrong_passwords_are_rate_limited() {
    let server = start().await;
    let mut a = Client::connect(&server, 1).await;
    a.hello("alice").await;
    a.request(ClientKind::CreateNetwork(credentials("lan", "secret"))).await.unwrap();
    let mut b = Client::connect(&server, 2).await;
    b.hello("bob").await;
    for _ in 0..5 {
        assert_eq!(
            b.request(ClientKind::JoinNetwork(credentials("lan", "guess"))).await,
            Err(ErrorCode::WrongPassword)
        );
    }
    assert_eq!(b.request(ClientKind::JoinNetwork(credentials("lan", "secret"))).await, Err(ErrorCode::RateLimited));
}

#[tokio::test]
async fn invalid_hello_is_refused() {
    let server = start().await;
    let mut a = Client::connect(&server, 1).await;
    let id = a.send(ClientKind::Hello(Hello { version: 999, nickname: "x".into() })).await;
    assert!(matches!(a.reply(id).await, ServerKind::Failure(f) if f.code() == ErrorCode::UnsupportedVersion));

    let mut b = Client::connect(&server, 2).await;
    let id = b.send(ClientKind::Hello(Hello { version: PROTOCOL_VERSION, nickname: " ".into() })).await;
    assert!(matches!(b.reply(id).await, ServerKind::Failure(f) if f.code() == ErrorCode::InvalidNickname));
}

#[tokio::test]
async fn reconnect_replaces_old_session() {
    let server = start().await;
    let mut old = Client::connect(&server, 1).await;
    old.hello("alice").await;
    // Handshake timestamps have 20 ms granularity, an earlier one would be rejected as a replay.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut new = Client::connect(&server, 1).await;
    new.hello("alice").await;
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if io::read_message(&mut old.reader, &mut old.receiver).await.is_err() {
                return;
            }
        }
    })
    .await;
    assert!(closed.is_ok());
    new.request(ClientKind::Ping(control::Empty {})).await.unwrap();
}

async fn discover(server: &Server, udp: &UdpSocket, welcome: &Welcome, own: &ObfsKey) {
    let token: [u8; 16] = welcome.discovery_token.as_slice().try_into().unwrap();
    let mut packet = discover_packet(&token, 0, [5; 16]);
    ObfsKey::for_receiver(&server.public_key).seal(&mut packet).unwrap();
    udp.send_to(&packet, server.udp_addr).await.unwrap();
    let mut buf = vec![0; 2048];
    let (len, _) = tokio::time::timeout(Duration::from_secs(5), udp.recv_from(&mut buf)).await.unwrap().unwrap();
    assert_eq!(own.open(&mut buf[..len]), Ok(Header::Observed));
    assert_eq!(parse_observed(&buf[..len]), Some((token, udp.local_addr().unwrap())));
}

#[tokio::test]
async fn relay_candidates_and_call_me_maybe() {
    let server = start().await;
    let mut a = Client::connect(&server, 1).await;
    let mut b = Client::connect(&server, 2).await;
    let mut c = Client::connect(&server, 3).await;
    let welcome_a = a.hello("alice").await;
    let welcome_b = b.hello("bob").await;
    let welcome_c = c.hello("carol").await;
    a.request(ClientKind::CreateNetwork(credentials("lan", "pw"))).await.unwrap();
    b.request(ClientKind::JoinNetwork(credentials("lan", "pw"))).await.unwrap();

    let udp_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let udp_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let obfs_a = ObfsKey::for_receiver(&a.key);
    let obfs_b = ObfsKey::for_receiver(&b.key);
    discover(&server, &udp_a, &welcome_a, &obfs_a).await;
    discover(&server, &udp_b, &welcome_b, &obfs_b).await;

    let local: SocketAddr = "192.168.1.20:4000".parse().unwrap();
    a.send(ClientKind::Candidates(Candidates { endpoints: vec![local.into()] })).await;
    let state = b.state_where(|s| s.peers.first().is_some_and(|p| !p.candidates.is_empty())).await;
    assert_eq!(state.peers[0].candidates[0].to_socket_addr(), Some(local));
    assert_eq!(state.peers[0].endpoint.as_ref().unwrap().to_socket_addr(), Some(udp_a.local_addr().unwrap()));

    a.send(ClientKind::CallMeMaybe(PeerKey { key: b.key.as_bytes().to_vec() })).await;
    loop {
        if let Some(ServerKind::CallMeMaybe(from)) = b.recv().await.kind {
            assert_eq!(from.key, a.key.as_bytes().to_vec());
            let endpoints: Vec<_> = from.endpoints.iter().filter_map(|e| e.to_socket_addr()).collect();
            assert_eq!(endpoints, vec![udp_a.local_addr().unwrap(), local]);
            break;
        }
    }
    loop {
        if let Some(ServerKind::CallMeMaybe(echo)) = a.recv().await.kind {
            assert_eq!(echo.key, b.key.as_bytes().to_vec());
            let endpoints: Vec<_> = echo.endpoints.iter().filter_map(|e| e.to_socket_addr()).collect();
            assert_eq!(endpoints, vec![udp_b.local_addr().unwrap()]);
            break;
        }
    }

    let inner: Vec<u8> = (0..64).collect();
    let token_a: [u8; 16] = welcome_a.discovery_token.as_slice().try_into().unwrap();
    let mut packet = relay_packet(&token_a, Ipv4Addr::from(welcome_b.address), &inner);
    ObfsKey::for_receiver(&server.public_key).seal(&mut packet).unwrap();
    udp_a.send_to(&packet, server.udp_addr).await.unwrap();
    let mut buf = vec![0; 2048];
    let (len, _) = tokio::time::timeout(Duration::from_secs(5), udp_b.recv_from(&mut buf)).await.unwrap().unwrap();
    assert_eq!(obfs_b.open(&mut buf[..len]), Ok(Header::Relayed));
    assert_eq!(parse_relayed(&buf[..len]), Some((Ipv4Addr::from(welcome_a.address), inner.as_slice())));

    b.send(ClientKind::Relay(RelayPacket { address: welcome_a.address, packet: inner.clone() })).await;
    let (len, _) = tokio::time::timeout(Duration::from_secs(5), udp_a.recv_from(&mut buf)).await.unwrap().unwrap();
    assert_eq!(obfs_a.open(&mut buf[..len]), Ok(Header::Relayed));
    assert_eq!(parse_relayed(&buf[..len]), Some((Ipv4Addr::from(welcome_b.address), inner.as_slice())));

    a.send(ClientKind::Relay(RelayPacket { address: welcome_c.address, packet: inner.clone() })).await;
    a.send(ClientKind::CallMeMaybe(PeerKey { key: c.key.as_bytes().to_vec() })).await;
    c.send(ClientKind::Relay(RelayPacket { address: welcome_a.address, packet: inner.clone() })).await;
    let id = c.send(ClientKind::Ping(control::Empty {})).await;
    loop {
        let message = c.recv().await;
        assert!(!matches!(message.kind, Some(ServerKind::Relay(_) | ServerKind::CallMeMaybe(_))));
        if message.reply_to == id {
            break;
        }
    }
    assert!(tokio::time::timeout(Duration::from_millis(300), udp_a.recv_from(&mut buf)).await.is_err());
}

#[tokio::test]
async fn invites_kicks_and_bans() {
    let server = start().await;
    let mut a = Client::connect(&server, 1).await;
    let mut b = Client::connect(&server, 2).await;
    let mut c = Client::connect(&server, 3).await;
    a.hello("alice").await;
    let welcome_b = b.hello("bob").await;
    c.hello("carol").await;
    a.request(ClientKind::CreateNetwork(credentials("lan", "secret"))).await.unwrap();

    assert_eq!(b.invite("lan", 0).await, Err(ErrorCode::NotMember));
    let once = a.invite("lan", 1).await.unwrap();
    assert_eq!(once.len(), 14);
    assert_eq!(b.redeem(&once.to_lowercase()).await.as_deref(), Ok("lan"));
    assert_eq!(c.redeem(&once).await, Err(ErrorCode::InviteNotFound));
    assert_eq!(b.redeem("NOPE").await, Err(ErrorCode::InviteNotFound));
    assert_eq!(b.invite("lan", 0).await, Err(ErrorCode::Forbidden));
    assert_eq!(b.request(ClientKind::Kick(member("lan", "alice"))).await, Err(ErrorCode::Forbidden));

    let open = a.invite("lan", 0).await.unwrap();
    let Ok(ServerKind::Invites(list)) = a.ask(ClientKind::ListInvites(NetworkName { name: "lan".into() })).await else {
        panic!()
    };
    assert_eq!(list.invites.len(), 1);
    assert_eq!((list.invites[0].code.as_str(), list.invites[0].creator.as_str()), (open.as_str(), "alice"));
    assert!(list.invites[0].expires > 0);

    assert_eq!(a.request(ClientKind::Kick(member("lan", "nobody"))).await, Err(ErrorCode::MemberNotFound));
    assert_eq!(a.request(ClientKind::Kick(member("lan", "alice"))).await, Err(ErrorCode::Forbidden));
    a.request(ClientKind::Kick(member("lan", "BOB"))).await.unwrap();
    b.state_where(|state| state.networks.is_empty()).await;
    assert_eq!(b.redeem(&open).await.as_deref(), Ok("lan"));

    let address = Ipv4Addr::from(welcome_b.address).to_string();
    a.request(ClientKind::Ban(member("lan", &address))).await.unwrap();
    b.state_where(|state| state.networks.is_empty()).await;
    assert_eq!(b.request(ClientKind::JoinNetwork(credentials("lan", "secret"))).await, Err(ErrorCode::Banned));
    assert_eq!(b.redeem(&open).await, Err(ErrorCode::Banned));
    let Ok(ServerKind::Bans(bans)) = a.ask(ClientKind::ListBans(NetworkName { name: "lan".into() })).await else {
        panic!()
    };
    assert_eq!(bans.devices.len(), 1);
    assert_eq!(bans.devices[0].nickname, "bob");

    a.request(ClientKind::Unban(member("lan", "bob"))).await.unwrap();
    assert_eq!(a.request(ClientKind::Unban(member("lan", "bob"))).await, Err(ErrorCode::MemberNotFound));
    b.request(ClientKind::JoinNetwork(credentials("lan", "secret"))).await.unwrap();

    a.request(ClientKind::RevokeInvite(InviteCode { code: open.to_lowercase() })).await.unwrap();
    assert_eq!(c.redeem(&open).await, Err(ErrorCode::InviteNotFound));
}

#[tokio::test]
async fn approval_roles_and_settings() {
    let server = start().await;
    let mut a = Client::connect(&server, 1).await;
    let mut b = Client::connect(&server, 2).await;
    let mut c = Client::connect(&server, 3).await;
    a.hello("alice").await;
    b.hello("bob").await;
    c.hello("carol").await;
    let lan = || NetworkName { name: "lan".into() };
    let settings = |locked, approval, password: Option<&str>| {
        ClientKind::UpdateNetwork(NetworkSettings {
            name: "lan".into(),
            locked,
            approval,
            password: password.map(Into::into),
        })
    };
    let role = |member: &str, role: Role| {
        ClientKind::SetRole(RoleChange { network: "lan".into(), member: member.into(), role: role as i32 })
    };
    a.request(ClientKind::CreateNetwork(credentials("lan", "secret"))).await.unwrap();
    a.request(settings(None, Some(true), None)).await.unwrap();

    let joined = b.ask(ClientKind::JoinNetwork(credentials("lan", "secret"))).await;
    assert!(matches!(joined, Ok(ServerKind::Pending(_))));
    let code = a.invite("lan", 0).await.unwrap();
    assert!(matches!(c.ask(ClientKind::RedeemInvite(InviteCode { code })).await, Ok(ServerKind::Pending(_))));
    a.state_where(|state| state.networks[0].requests == 2).await;
    let Ok(ServerKind::Requests(list)) = a.ask(ClientKind::ListRequests(lan())).await else { panic!() };
    let names: Vec<&str> = list.devices.iter().map(|device| device.nickname.as_str()).collect();
    assert_eq!(names, ["bob", "carol"]);
    assert_eq!(b.ask(ClientKind::ListRequests(lan())).await.err(), Some(ErrorCode::NotMember));

    a.request(ClientKind::Approve(member("lan", "bob"))).await.unwrap();
    b.state_where(|state| state.networks.len() == 1).await;
    a.request(ClientKind::Deny(member("lan", "carol"))).await.unwrap();
    assert_eq!(a.request(ClientKind::Deny(member("lan", "carol"))).await, Err(ErrorCode::MemberNotFound));

    a.request(role("bob", Role::Admin)).await.unwrap();
    b.state_where(|state| state.networks.first().is_some_and(|network| network.role() == Role::Admin)).await;
    assert_eq!(b.request(role("alice", Role::Member)).await, Err(ErrorCode::Forbidden));
    assert_eq!(b.request(settings(None, None, Some("another"))).await, Err(ErrorCode::Forbidden));
    b.request(settings(Some(true), None, None)).await.unwrap();
    let locked = c.ask(ClientKind::JoinNetwork(credentials("lan", "secret"))).await;
    assert_eq!(locked.err(), Some(ErrorCode::NetworkLocked));

    a.request(settings(Some(false), Some(false), Some("changed"))).await.unwrap();
    let wrong = c.ask(ClientKind::JoinNetwork(credentials("lan", "secret"))).await;
    assert_eq!(wrong.err(), Some(ErrorCode::WrongPassword));
    c.request(ClientKind::JoinNetwork(credentials("lan", "changed"))).await.unwrap();

    a.request(role("bob", Role::Member)).await.unwrap();
    assert_eq!(b.request(ClientKind::DeleteNetwork(lan())).await, Err(ErrorCode::Forbidden));
    a.request(ClientKind::DeleteNetwork(lan())).await.unwrap();
    c.state_where(|state| state.networks.is_empty()).await;
}

#[cfg(unix)]
#[tokio::test]
async fn administration_and_relay_limits() {
    use loom::admin::{AdminRequest, AdminResponse, request};

    let mut server = start_with(Config { relay_mbit: 1, ..Config::default() }).await;
    let path = std::env::temp_dir().join(format!("loom-admin-{}.sock", std::process::id()));
    server.serve_admin(&path).unwrap();
    let mut a = Client::connect(&server, 1).await;
    let mut b = Client::connect(&server, 2).await;
    a.hello("alice").await;
    let welcome_b = b.hello("bob").await;
    a.request(ClientKind::CreateNetwork(credentials("lan", "secret"))).await.unwrap();
    b.request(ClientKind::JoinNetwork(credentials("lan", "secret"))).await.unwrap();

    for _ in 0..100 {
        a.send(ClientKind::Relay(RelayPacket { address: welcome_b.address, packet: vec![7; 1200] })).await;
    }
    a.request(ClientKind::Ping(control::Empty {})).await.unwrap();
    let Ok(AdminResponse::Stats(stats)) = request(&path, &AdminRequest::Stats).await else { panic!() };
    assert_eq!((stats.devices, stats.online, stats.networks), (2, 2, 1));
    assert_eq!(stats.relay.packets + stats.relay.dropped, 100);
    assert!(stats.relay.dropped > 0 && stats.relay.bytes < 100_000, "{:?}", stats.relay);
    assert_eq!(stats.top_relay[0].nickname, "alice");

    let Ok(AdminResponse::Networks(networks)) = request(&path, &AdminRequest::Networks).await else { panic!() };
    assert_eq!(
        (networks[0].name.as_str(), networks[0].members, networks[0].owner.as_deref()),
        ("lan", 2, Some("alice"))
    );

    let block = |device: &str| AdminRequest::Block { device: device.into() };
    assert!(matches!(request(&path, &block("nobody")).await, Ok(AdminResponse::Error(_))));
    assert_eq!(request(&path, &block("BOB")).await.unwrap(), AdminResponse::Ok);
    a.state_where(|state| state.peers.iter().all(|peer| !peer.online)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut again = Client::connect(&server, 2).await;
    let id = again.send(ClientKind::Hello(Hello { version: PROTOCOL_VERSION, nickname: "bob".into() })).await;
    assert!(matches!(again.reply(id).await, ServerKind::Failure(f) if f.code() == ErrorCode::Banned));
    let Ok(AdminResponse::Devices(devices)) = request(&path, &AdminRequest::Devices).await else { panic!() };
    assert_eq!(
        devices.iter().map(|d| (d.nickname.as_str(), d.blocked)).collect::<Vec<_>>(),
        [("alice", false), ("bob", true)]
    );

    let unblock = AdminRequest::Unblock { device: Ipv4Addr::from(welcome_b.address).to_string() };
    assert_eq!(request(&path, &unblock).await.unwrap(), AdminResponse::Ok);
    tokio::time::sleep(Duration::from_millis(50)).await;
    Client::connect(&server, 2).await.hello("bob").await;

    let delete = AdminRequest::DeleteNetwork { name: "LAN".into() };
    assert_eq!(request(&path, &delete).await.unwrap(), AdminResponse::Ok);
    a.state_where(|state| state.networks.is_empty()).await;
    let _ = std::fs::remove_file(&path);
}
