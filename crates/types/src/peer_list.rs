use crate::{Compact, PeerAddress};
use actix::Message;
use arbitrary::Arbitrary;
use bytes::Buf;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, Arbitrary, PartialEq, Hash)]
pub struct PeerScore(u16);

impl PeerScore {
    pub const MIN: u16 = 0;
    pub const MAX: u16 = 100;
    pub const INITIAL: u16 = 50;
    pub const ACTIVE_THRESHOLD: u16 = 20;

    pub fn new(score: u16) -> Self {
        Self(score.clamp(Self::MIN, Self::MAX))
    }

    pub fn increase(&mut self) {
        self.0 = (self.0 + 1).min(Self::MAX);
    }

    pub fn decrease_offline(&mut self) {
        self.0 = self.0.saturating_sub(3);
    }

    pub fn decrease_bogus_data(&mut self) {
        self.0 = self.0.saturating_sub(5);
    }

    pub fn is_active(&self) -> bool {
        self.0 >= Self::ACTIVE_THRESHOLD
    }

    pub fn get(&self) -> u16 {
        self.0
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Arbitrary, Hash)]
pub struct PeerListItem {
    pub reputation_score: PeerScore,
    pub response_time: u16,
    pub address: PeerAddress,
    pub last_seen: u64,
    pub is_online: bool,
}

impl Default for PeerListItem {
    fn default() -> Self {
        Self {
            reputation_score: PeerScore(PeerScore::INITIAL),
            response_time: 0,
            address: PeerAddress {
                gossip: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0)),
                api: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0)),
                execution: RethPeerInfo::default(),
            },
            last_seen: Utc::now().timestamp_millis() as u64,
            is_online: true,
        }
    }
}

#[derive(
    Message,
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    Arbitrary,
    PartialOrd,
    Ord,
    Hash,
    Eq,
    PartialEq,
)]
#[rtype(result = "eyre::Result<()>")]
pub struct RethPeerInfo {
    // Reth's peering port: https://reth.rs/run/ports.html#peering-ports
    pub peering_tcp_addr: SocketAddr,
    pub peer_id: reth_transaction_pool::PeerId,
}

impl Default for RethPeerInfo {
    fn default() -> Self {
        Self {
            peering_tcp_addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0)),
            peer_id: Default::default(),
        }
    }
}

impl Compact for RethPeerInfo {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let mut size = 0;
        size += encode_address(&self.peering_tcp_addr, buf);
        size += self.peer_id.to_compact(buf);
        size
    }

    fn from_compact(buf: &[u8], _: usize) -> (Self, &[u8]) {
        let mut buf = buf;
        let (peering_tcp_addr, consumed) = decode_address(&buf);
        buf.advance(consumed);
        let (peer_id, buf) = reth_transaction_pool::PeerId::from_compact(buf, buf.len());
        (
            Self {
                peering_tcp_addr,
                peer_id,
            },
            buf,
        )
    }
}

fn encode_address<B>(address: &SocketAddr, buf: &mut B) -> usize
where
    B: bytes::BufMut + AsMut<[u8]>,
{
    let mut size = 0;
    match address {
        SocketAddr::V4(addr4) => {
            buf.put_u8(0); // Tag for IPv4
            buf.put_slice(&addr4.ip().octets());
            buf.put_u16(addr4.port());
            size += 7; // 1 byte tag + 4 bytes IPv4 + 2 bytes port
        }
        SocketAddr::V6(addr6) => {
            buf.put_u8(1); // Tag for IPv6
            buf.put_slice(&addr6.ip().octets());
            buf.put_u16(addr6.port());
            size += 19; // 1 byte tag + 16 bytes IPv6 + 2 bytes port
        }
    };
    size
}

fn decode_address(buf: &[u8]) -> (SocketAddr, usize) {
    let tag = buf[0];
    let address = match tag {
        0 => {
            // IPv4 address (needs 4 bytes IP + 2 bytes port after tag)
            if buf.len() < 11 {
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0))
            } else {
                let ip_octets: [u8; 4] = buf[1..5].try_into().unwrap();
                let port = u16::from_be_bytes(buf[5..7].try_into().unwrap());
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip_octets), port))
            }
        }
        1 => {
            // IPv6 address (needs 16 bytes IP + 2 bytes port after tag)
            if buf.len() < 23 {
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0))
            } else {
                let mut ip_octets = [0u8; 16];
                ip_octets.copy_from_slice(&buf[1..17]);
                let port = u16::from_be_bytes(buf[17..19].try_into().unwrap());
                SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(ip_octets), port, 0, 0))
            }
        }
        _ => SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0)),
    };

    let consumed = match tag {
        0 => 7,  // 4 bytes header + 1 byte tag + 4 bytes IPv4 + 2 bytes port
        1 => 19, // 4 bytes header + 1 byte tag + 16 bytes IPv6 + 2 bytes port
        _ => 1,  // 4 bytes header + 1 byte tag
    };
    (address, consumed)
}

/// This may seem a little heavy handed to implement Compact for the entire
/// struct but it has the advantage of being better able to handle migrations
/// in the future. New fields can be appended to the end of the compact buffer
/// and the from_compact can populate new fields with defaults if the buffer
/// length is from a previous smaller version of the struct.
impl Compact for PeerListItem {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let mut size = 0;

        // Encode reputation_score
        buf.put_u16(self.reputation_score.0);
        size += 2;

        // Encode response_time
        buf.put_u16(self.response_time);
        size += 2;

        // Encode socket addresses
        size += encode_address(&self.address.gossip, buf);

        size += encode_address(&self.address.api, buf);

        size += self.address.execution.to_compact(buf);

        // Encode last_seen
        buf.put_u64(self.last_seen);
        size += 8;

        buf.put_u8(if self.is_online { 1 } else { 0 });
        size += 1;

        size
    }

    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        let mut buf = buf;
        if buf.len() < 4 {
            return (Self::default(), &[]);
        }

        let reputation_score = PeerScore(u16::from_be_bytes(buf[0..2].try_into().unwrap()));
        buf.advance(2);
        let response_time = u16::from_be_bytes(buf[0..2].try_into().unwrap());
        buf.advance(2);

        if buf.len() < 1 {
            return (
                Self {
                    reputation_score,
                    response_time,
                    address: PeerAddress {
                        gossip: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0)),
                        api: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0)),
                        execution: RethPeerInfo::default(),
                    },
                    last_seen: 0,
                    is_online: false,
                },
                &[],
            );
        }

        let (gossip_address, consumed) = decode_address(&buf);
        buf.advance(consumed);

        let (api_address, consumed) = decode_address(&buf);
        buf.advance(consumed);

        // let (reth_peering_tcp, consumed) = decode_address(&buf[total_consumed..]);
        let (reth_peer_info, buf) = RethPeerInfo::from_compact(&buf, buf.len());
        // total_consumed += consumed;

        let address = PeerAddress {
            gossip: gossip_address,
            api: api_address,
            execution: reth_peer_info,
        };

        // Read last_seen if available
        let last_seen = if buf.len() >= 8 {
            u64::from_be_bytes(buf[0..8].try_into().unwrap())
        } else {
            0
        };

        let total_consumed = 8;

        let is_online = if buf.len() >= total_consumed + 1 {
            buf[total_consumed] == 1
        } else {
            false
        };

        (
            Self {
                reputation_score,
                response_time,
                address,
                last_seen,
                is_online,
            },
            &buf[total_consumed.min(buf.len())..],
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::{peer_list::*, *};
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    #[test]
    fn peer_list_item_compact_roundtrip() {
        let peer_list_item = PeerListItem::default();
        let mut buf = bytes::BytesMut::with_capacity(30);
        peer_list_item.to_compact(&mut buf);
        let (decoded, _) = PeerListItem::from_compact(&buf[..], buf.len());
        assert_eq!(peer_list_item, decoded);
    }

    #[test]
    fn address_encode_roundtrip() {
        let address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0));
        let mut buf = bytes::BytesMut::with_capacity(30);
        encode_address(&address, &mut buf);
        let (decoded, _) = decode_address(&buf[..]);
        assert_eq!(address, decoded);
    }
}
