use crate::Compact;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerListEntry {
    pub reputation_score: u16,
    pub response_time: u16,
    pub ip_address: IpAddr,
}

impl Default for PeerListEntry {
    fn default() -> Self {
        Self {
            reputation_score: 0,
            response_time: 0,
            ip_address: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
        }
    }
}

impl Compact for PeerListEntry {
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

        // Encode ip_address
        match self.ip_address {
            IpAddr::V4(ipv4) => {
                buf.put_u8(0); // Tag for IPv4
                buf.put_slice(&ipv4.octets());
                size += 5; // 1 byte tag + 4 bytes IPv4
            }
            IpAddr::V6(ipv6) => {
                buf.put_u8(1); // Tag for IPv6
                buf.put_slice(&ipv6.octets());
                size += 17; // 1 byte tag + 16 bytes IPv6
            }
        }

        size
    }

    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        if buf.len() < 4 {
            return (Self::default(), &[]);
        }

        let reputation_score = u16::from_be_bytes([buf[0], buf[1]]);
        let response_time = u16::from_be_bytes([buf[2], buf[3]]);

        if buf.len() < 5 {
            return (
                Self {
                    reputation_score,
                    response_time,
                    ip_address: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
                },
                &[],
            );
        }

        let tag = buf[4];
        let ip_address = match tag {
            0 => {
                // IPv4 address (needs at least 4 more bytes after the tag)
                if buf.len() < 9 {
                    IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))
                } else {
                    let octets = [buf[5], buf[6], buf[7], buf[8]];
                    IpAddr::V4(Ipv4Addr::from(octets))
                }
            }
            1 => {
                // IPv6 address (needs at least 16 more bytes after the tag)
                if buf.len() < 21 {
                    IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))
                } else {
                    let mut octets = [0u8; 16];
                    octets.copy_from_slice(&buf[5..21]);
                    IpAddr::V6(Ipv6Addr::from(octets))
                }
            }
            _ => IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
        };

        let consumed = match tag {
            0 => 9,  // 4 bytes header + 1 byte tag + 4 bytes IPv4
            1 => 21, // 4 bytes header + 1 byte tag + 16 bytes IPv6
            _ => 5,  // 4 bytes header + 1 byte tag
        };

        (
            Self {
                reputation_score,
                response_time,
                ip_address,
            },
            &buf[consumed.min(buf.len())..],
        )
    }
}
