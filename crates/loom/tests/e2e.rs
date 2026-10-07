use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, SystemTime};

use loom::Server;
use loom::config::Config;
use loom::db::Db;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpStream, UdpSocket};
use weft_proto::control::{
    self, ClientKind, ClientMessage, ErrorCode, Hello, NetworkCredentials, NetworkName, PROTOCOL_VERSION, Role,
    ServerKind, ServerMessage, State, Welcome,
};
use weft_proto::obfs::discover_packet;
use weft_proto::{ObfsKey, PublicKey};
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
        let id = self.send(kind).await;
        match self.reply(id).await {
            ServerKind::Ack(_) => Ok(()),
            ServerKind::Failure(failure) => Err(failure.code()),
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn credentials(name: &str, password: &str) -> NetworkCredentials {
    NetworkCredentials { name: name.into(), password: password.into() }
}

async fn start() -> Server {
    let config = Config { listen: "127.0.0.1:0".parse().unwrap(), ..Config::default() };
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
