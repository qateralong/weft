use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use prost::{Enumeration, Message, Oneof};

pub use prost::DecodeError;

pub const PROTOCOL_VERSION: u32 = 1;
pub const DISCOVERY_TOKEN_LEN: usize = 16;

#[derive(Clone, PartialEq, Message)]
pub struct ClientMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(oneof = "ClientKind", tags = "2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17")]
    pub kind: Option<ClientKind>,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum ClientKind {
    #[prost(message, tag = "2")]
    Hello(Hello),
    #[prost(message, tag = "3")]
    CreateNetwork(NetworkCredentials),
    #[prost(message, tag = "4")]
    JoinNetwork(NetworkCredentials),
    #[prost(message, tag = "5")]
    LeaveNetwork(NetworkName),
    #[prost(message, tag = "6")]
    Ping(Empty),
    #[prost(message, tag = "7")]
    Candidates(Candidates),
    #[prost(message, tag = "8")]
    CallMeMaybe(PeerKey),
    #[prost(message, tag = "9")]
    Relay(RelayPacket),
    #[prost(message, tag = "10")]
    CreateInvite(InviteRequest),
    #[prost(message, tag = "11")]
    ListInvites(NetworkName),
    #[prost(message, tag = "12")]
    RevokeInvite(InviteCode),
    #[prost(message, tag = "13")]
    RedeemInvite(InviteCode),
    #[prost(message, tag = "14")]
    Kick(MemberAction),
    #[prost(message, tag = "15")]
    Ban(MemberAction),
    #[prost(message, tag = "16")]
    Unban(MemberAction),
    #[prost(message, tag = "17")]
    ListBans(NetworkName),
}

#[derive(Clone, PartialEq, Message)]
pub struct InviteRequest {
    #[prost(string, tag = "1")]
    pub network: String,
    #[prost(uint32, tag = "2")]
    pub max_uses: u32,
    #[prost(uint64, tag = "3")]
    pub expires_in: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct InviteCode {
    #[prost(string, tag = "1")]
    pub code: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct MemberAction {
    #[prost(string, tag = "1")]
    pub network: String,
    #[prost(string, tag = "2")]
    pub member: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Invite {
    #[prost(string, tag = "1")]
    pub code: String,
    #[prost(string, tag = "2")]
    pub network: String,
    #[prost(uint32, tag = "3")]
    pub max_uses: u32,
    #[prost(uint32, tag = "4")]
    pub uses: u32,
    #[prost(uint64, tag = "5")]
    pub expires: u64,
    #[prost(string, tag = "6")]
    pub creator: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct InviteList {
    #[prost(message, repeated, tag = "1")]
    pub invites: Vec<Invite>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Ban {
    #[prost(bytes = "vec", tag = "1")]
    pub key: Vec<u8>,
    #[prost(string, tag = "2")]
    pub nickname: String,
    #[prost(fixed32, tag = "3")]
    pub address: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct BanList {
    #[prost(message, repeated, tag = "1")]
    pub bans: Vec<Ban>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Candidates {
    #[prost(message, repeated, tag = "1")]
    pub endpoints: Vec<Endpoint>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PeerKey {
    #[prost(bytes = "vec", tag = "1")]
    pub key: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RelayPacket {
    #[prost(fixed32, tag = "1")]
    pub address: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub packet: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Hello {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(string, tag = "2")]
    pub nickname: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct NetworkCredentials {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub password: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct NetworkName {
    #[prost(string, tag = "1")]
    pub name: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Empty {}

#[derive(Clone, PartialEq, Message)]
pub struct ServerMessage {
    #[prost(uint32, tag = "1")]
    pub reply_to: u32,
    #[prost(oneof = "ServerKind", tags = "2, 3, 4, 5, 6, 7, 8, 9, 10, 11")]
    pub kind: Option<ServerKind>,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum ServerKind {
    #[prost(message, tag = "2")]
    Welcome(Welcome),
    #[prost(message, tag = "3")]
    Ack(Empty),
    #[prost(message, tag = "4")]
    Failure(Failure),
    #[prost(message, tag = "5")]
    State(State),
    #[prost(message, tag = "6")]
    CallMeMaybe(PeerCandidates),
    #[prost(message, tag = "7")]
    Relay(RelayPacket),
    #[prost(message, tag = "8")]
    Invite(Invite),
    #[prost(message, tag = "9")]
    Invites(InviteList),
    #[prost(message, tag = "10")]
    Joined(NetworkName),
    #[prost(message, tag = "11")]
    Bans(BanList),
}

#[derive(Clone, PartialEq, Message)]
pub struct PeerCandidates {
    #[prost(bytes = "vec", tag = "1")]
    pub key: Vec<u8>,
    #[prost(message, repeated, tag = "2")]
    pub endpoints: Vec<Endpoint>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Welcome {
    #[prost(fixed32, tag = "1")]
    pub address: u32,
    #[prost(uint32, tag = "2")]
    pub prefix_len: u32,
    #[prost(bytes = "vec", tag = "3")]
    pub discovery_token: Vec<u8>,
    #[prost(uint32, tag = "4")]
    pub udp_port: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Failure {
    #[prost(enumeration = "ErrorCode", tag = "1")]
    pub code: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum ErrorCode {
    Unspecified = 0,
    UnsupportedVersion = 1,
    InvalidRequest = 2,
    InvalidName = 3,
    InvalidNickname = 4,
    InvalidPassword = 5,
    NetworkExists = 6,
    NetworkNotFound = 7,
    WrongPassword = 8,
    NetworkFull = 9,
    RateLimited = 10,
    AlreadyMember = 11,
    NotMember = 12,
    PoolExhausted = 13,
    Internal = 14,
    Forbidden = 15,
    InviteNotFound = 16,
    Banned = 17,
    MemberNotFound = 18,
    AmbiguousMember = 19,
    TooManyInvites = 20,
}

#[derive(Clone, PartialEq, Message)]
pub struct State {
    #[prost(message, repeated, tag = "1")]
    pub networks: Vec<Network>,
    #[prost(message, repeated, tag = "2")]
    pub peers: Vec<Peer>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Network {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(enumeration = "Role", tag = "2")]
    pub role: i32,
    #[prost(bytes = "vec", repeated, tag = "3")]
    pub members: Vec<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum Role {
    Member = 0,
    Admin = 1,
    Owner = 2,
}

#[derive(Clone, PartialEq, Message)]
pub struct Peer {
    #[prost(bytes = "vec", tag = "1")]
    pub key: Vec<u8>,
    #[prost(string, tag = "2")]
    pub nickname: String,
    #[prost(fixed32, tag = "3")]
    pub address: u32,
    #[prost(bool, tag = "4")]
    pub online: bool,
    #[prost(message, optional, tag = "5")]
    pub endpoint: Option<Endpoint>,
    #[prost(message, repeated, tag = "6")]
    pub candidates: Vec<Endpoint>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Endpoint {
    #[prost(bytes = "vec", tag = "1")]
    pub ip: Vec<u8>,
    #[prost(uint32, tag = "2")]
    pub port: u32,
}

impl From<SocketAddr> for Endpoint {
    fn from(addr: SocketAddr) -> Self {
        let ip = match addr.ip().to_canonical() {
            IpAddr::V4(ip) => ip.octets().to_vec(),
            IpAddr::V6(ip) => ip.octets().to_vec(),
        };
        Endpoint { ip, port: u32::from(addr.port()) }
    }
}

impl Endpoint {
    pub fn to_socket_addr(&self) -> Option<SocketAddr> {
        let port = u16::try_from(self.port).ok().filter(|&port| port != 0)?;
        let ip = match self.ip.len() {
            4 => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(self.ip.as_slice()).ok()?)),
            16 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(self.ip.as_slice()).ok()?)),
            _ => return None,
        };
        Some(SocketAddr::new(ip, port))
    }
}

impl ServerMessage {
    pub fn push(kind: ServerKind) -> Self {
        Self { reply_to: 0, kind: Some(kind) }
    }

    pub fn reply(id: u32, kind: ServerKind) -> Self {
        Self { reply_to: id, kind: Some(kind) }
    }

    pub fn failure(id: u32, code: ErrorCode) -> Self {
        Self::reply(id, ServerKind::Failure(Failure { code: code as i32 }))
    }
}

pub fn encode<M: Message>(message: &M) -> Vec<u8> {
    message.encode_to_vec()
}

pub fn decode<M: Message + Default>(bytes: &[u8]) -> Result<M, DecodeError> {
    M::decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_roundtrip() {
        let message = ClientMessage {
            id: 7,
            kind: Some(ClientKind::JoinNetwork(NetworkCredentials {
                name: "Φίλοι".into(), password: "secret".into()
            })),
        };
        assert_eq!(decode::<ClientMessage>(&encode(&message)).unwrap(), message);
    }

    #[test]
    fn server_roundtrip() {
        let state = State {
            networks: vec![Network { name: "lan".into(), role: Role::Owner as i32, members: vec![vec![1; 32]] }],
            peers: vec![Peer {
                key: vec![2; 32],
                nickname: "vasya".into(),
                address: u32::from(Ipv4Addr::new(100, 64, 0, 2)),
                online: true,
                endpoint: Some("203.0.113.9:41000".parse::<SocketAddr>().unwrap().into()),
                candidates: vec!["192.168.1.5:41000".parse::<SocketAddr>().unwrap().into()],
            }],
        };
        let message = ServerMessage::push(ServerKind::State(state));
        let decoded = decode::<ServerMessage>(&encode(&message)).unwrap();
        assert_eq!(decoded, message);
        let Some(ServerKind::State(state)) = decoded.kind else { panic!() };
        assert_eq!(state.networks[0].role(), Role::Owner);

        let failure = ServerMessage::failure(3, ErrorCode::WrongPassword);
        let Some(ServerKind::Failure(failure)) = decode::<ServerMessage>(&encode(&failure)).unwrap().kind else {
            panic!()
        };
        assert_eq!(failure.code(), ErrorCode::WrongPassword);
    }

    #[test]
    fn endpoints() {
        for text in ["198.51.100.1:443", "[2001:db8::7]:5000"] {
            let addr: SocketAddr = text.parse().unwrap();
            assert_eq!(Endpoint::from(addr).to_socket_addr(), Some(addr));
        }
        let mapped: SocketAddr = "[::ffff:192.0.2.1]:80".parse().unwrap();
        assert_eq!(Endpoint::from(mapped).to_socket_addr(), Some("192.0.2.1:80".parse().unwrap()));
        assert_eq!(Endpoint { ip: vec![1, 2, 3], port: 1 }.to_socket_addr(), None);
        assert_eq!(Endpoint { ip: vec![1, 2, 3, 4], port: 0 }.to_socket_addr(), None);
    }
}
