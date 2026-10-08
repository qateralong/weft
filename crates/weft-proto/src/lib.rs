pub mod control;
pub mod key;
pub mod link;
pub mod loom;
pub mod obfs;
pub mod packet;
pub mod padding;

use std::net::Ipv4Addr;

pub use key::{KEY_LEN, KeyError, PublicKey};
pub use link::{Host, Link, LinkError};
pub use obfs::{LengthMask, ObfsKey};
pub use packet::{HEADER_LEN, Header, MIN_PACKET_LEN, PacketError, TAG_LEN};

/// Virtual address where every daemon answers DNS queries for peer names.
pub const DNS_ADDRESS: Ipv4Addr = Ipv4Addr::new(100, 100, 100, 100);
