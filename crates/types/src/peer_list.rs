use crate::Compact;
use arbitrary::Arbitrary;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Arbitrary)]
pub struct PeerListItem {
    pub reputation_score: u16,
    pub response_time: u16,
    pub address: SocketAddr,
    pub last_seen: u64,
}

impl Default for PeerListItem {
    fn default() -> Self {
        Self {
            reputation_score: 0,
            response_time: 0,
            address: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0)),
            last_seen: Utc::now().timestamp_millis() as u64,
        }
    }
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
        buf.put_u16(self.reputation_score);
        size += 2;

        // Encode response_time
        buf.put_u16(self.response_time);
        size += 2;

        // Encode socket address
        match self.address {
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
        }

        // Encode last_seen
        buf.put_u64(self.last_seen);
        size += 8;

        size
    }

    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        if buf.len() < 4 {
            return (Self::default(), &[]);
        }

        let reputation_score = u16::from_be_bytes(buf[0..2].try_into().unwrap());
        let response_time = u16::from_be_bytes(buf[2..4].try_into().unwrap());

        if buf.len() < 5 {
            return (
                Self {
                    reputation_score,
                    response_time,
                    address: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0)),
                    last_seen: 0,
                },
                &[],
            );
        }

        let tag = buf[4];
        let address = match tag {
            0 => {
                // IPv4 address (needs 4 bytes IP + 2 bytes port after tag)
                if buf.len() < 11 {
                    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0))
                } else {
                    let ip_octets: [u8; 4] = buf[5..9].try_into().unwrap();
                    let port = u16::from_be_bytes(buf[9..11].try_into().unwrap());
                    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip_octets), port))
                }
            }
            1 => {
                // IPv6 address (needs 16 bytes IP + 2 bytes port after tag)
                if buf.len() < 23 {
                    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0))
                } else {
                    let mut ip_octets = [0u8; 16];
                    ip_octets.copy_from_slice(&buf[5..21]);
                    let port = u16::from_be_bytes(buf[21..23].try_into().unwrap());
                    SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(ip_octets), port, 0, 0))
                }
            }
            _ => SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0)),
        };

        let consumed = match tag {
            0 => 11, // 4 bytes header + 1 byte tag + 4 bytes IPv4 + 2 bytes port
            1 => 23, // 4 bytes header + 1 byte tag + 16 bytes IPv6 + 2 bytes port
            _ => 5,  // 4 bytes header + 1 byte tag
        };

        // Read last_seen if available
        let last_seen = if buf.len() >= consumed + 8 {
            u64::from_be_bytes(buf[consumed..consumed + 8].try_into().unwrap())
        } else {
            0
        };

        let total_consumed = consumed + 8;

        (
            Self {
                reputation_score,
                response_time,
                address,
                last_seen,
            },
            &buf[total_consumed.min(buf.len())..],
        )
    }
}
