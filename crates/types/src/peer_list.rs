use crate::{BlockHash, ChunkPathHash, Compact, GossipDataRequest, PeerAddress};
use actix::Message;
use alloy_primitives::B256;
use arbitrary::Arbitrary;
use bytes::Buf as _;
use reth::providers::errors::db::DatabaseError;
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, Arbitrary, PartialEq, Hash)]
pub struct PeerScore(u16);

pub const DATA_REQUEST_RETRIES: u8 = 5;

impl PeerScore {
    pub const MIN: u16 = 0;
    pub const MAX: u16 = 100;
    pub const INITIAL: u16 = 50;
    pub const ACTIVE_THRESHOLD: u16 = 20;
    /// Score threshold for unstaked peers to be persisted to the database
    pub const PERSISTENCE_THRESHOLD: u16 = 80;

    pub fn new(score: u16) -> Self {
        Self(score.clamp(Self::MIN, Self::MAX))
    }

    pub fn increase(&mut self) {
        self.0 = (self.0 + 1).min(Self::MAX);
    }

    /// Limited increase for data requests (prevents farming)
    pub fn increase_limited(&mut self, amount: u16) {
        self.0 = (self.0 + amount).min(Self::MAX);
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

    /// Checks if the peer score is high enough to be persisted to database (for unstaked peers)
    pub fn is_persistable(&self) -> bool {
        self.0 >= Self::PERSISTENCE_THRESHOLD
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
            last_seen: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
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
#[serde(deny_unknown_fields)]
pub struct RethPeerInfo {
    // Reth's PUBLICLY ACCESSIBLE peering port: https://reth.rs/run/ports.html#peering-ports
    pub peering_tcp_addr: SocketAddr,
    #[serde(default)]
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
        let (peering_tcp_addr, consumed) = decode_address(buf);
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

pub fn encode_address<B>(address: &SocketAddr, buf: &mut B) -> usize
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

pub fn decode_address(buf: &[u8]) -> (SocketAddr, usize) {
    let tag = buf[0];
    let address = match tag {
        0 => {
            // IPv4 address (needs 4 bytes IP + 2 bytes port after tag)
            if buf.len() < 7 {
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0))
            } else {
                let ip_octets: [u8; 4] = buf[1..5].try_into().unwrap();
                let port = u16::from_be_bytes(buf[5..7].try_into().unwrap());
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip_octets), port))
            }
        }
        1 => {
            // IPv6 address (needs 16 bytes IP + 2 bytes port after tag)
            if buf.len() < 19 {
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0))
            } else {
                let mut ip_octets = [0_u8; 16];
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

        if buf.is_empty() {
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

        let (gossip_address, consumed) = decode_address(buf);
        buf.advance(consumed);

        let (api_address, consumed) = decode_address(buf);
        buf.advance(consumed);

        // let (reth_peering_tcp, consumed) = decode_address(&buf[total_consumed..]);
        let (reth_peer_info, buf) = RethPeerInfo::from_compact(buf, buf.len());
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

        let is_online = if buf.len() > total_consumed {
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

#[derive(Copy, Clone, Debug)]
pub struct HandshakeMessage {
    /// API address of the peer to handshake with
    pub api_address: SocketAddr,
    /// If true, tries to handshake even if the peer is already known and the last handshake
    /// was successful
    pub force: bool,
}

#[derive(Message, Debug)]
#[rtype(result = "()")]
pub enum PeerNetworkServiceMessage {
    Handshake(HandshakeMessage),
    AnnounceYourselfToPeer(PeerListItem),
    RequestDataFromNetwork {
        data_request: GossipDataRequest,
        use_trusted_peers_only: bool,
        response: tokio::sync::oneshot::Sender<Result<(), PeerNetworkError>>,
        retries: u8,
    },
}

#[derive(Debug, Error, Clone)]
pub enum PeerNetworkError {
    #[error("Internal database error: {0}")]
    Database(DatabaseError),
    #[error("Peer list internal error: {0:?}")]
    InternalSendError(String),
    #[error("Peer list internal error: {0}")]
    OtherInternalError(String),
    #[error("Failed to request data from network: {0}")]
    FailedToRequestData(String),
    #[error("No peers available to request data from")]
    NoPeersAvailable,
    #[error("Unexpected data received: {0}")]
    UnexpectedData(String),
}

impl From<SendError<PeerNetworkServiceMessage>> for PeerNetworkError {
    fn from(err: SendError<PeerNetworkServiceMessage>) -> Self {
        Self::InternalSendError(format!("Failed to send a message: {:?}", err))
    }
}

impl From<DatabaseError> for PeerNetworkError {
    fn from(err: DatabaseError) -> Self {
        Self::Database(err)
    }
}

impl From<eyre::Report> for PeerNetworkError {
    fn from(err: eyre::Report) -> Self {
        Self::Database(DatabaseError::Other(err.to_string()))
    }
}

#[derive(Clone, Debug)]
pub struct PeerNetworkSender(UnboundedSender<PeerNetworkServiceMessage>);

impl PeerNetworkSender {
    pub fn new(sender: UnboundedSender<PeerNetworkServiceMessage>) -> Self {
        Self(sender)
    }

    pub fn new_with_receiver() -> (
        Self,
        tokio::sync::mpsc::UnboundedReceiver<PeerNetworkServiceMessage>,
    ) {
        let (sender, receiver) = unbounded_channel();
        (Self(sender), receiver)
    }

    pub fn send(
        &self,
        message: PeerNetworkServiceMessage,
    ) -> Result<(), SendError<PeerNetworkServiceMessage>> {
        self.0.send(message)
    }

    pub fn announce_yourself_to_peer(
        &self,
        peer: PeerListItem,
    ) -> Result<(), SendError<PeerNetworkServiceMessage>> {
        let message = PeerNetworkServiceMessage::AnnounceYourselfToPeer(peer);
        self.send(message)
    }

    pub fn initiate_handshake(
        &self,
        api_address: SocketAddr,
        force: bool,
    ) -> Result<(), SendError<PeerNetworkServiceMessage>> {
        let message = PeerNetworkServiceMessage::Handshake(HandshakeMessage { api_address, force });
        self.send(message)
    }

    pub async fn request_block_to_be_gossiped_from_network(
        &self,
        block_hash: BlockHash,
        use_trusted_peers_only: bool,
        retries: u8,
    ) -> Result<(), PeerNetworkError> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let message = PeerNetworkServiceMessage::RequestDataFromNetwork {
            data_request: GossipDataRequest::Block(block_hash),
            use_trusted_peers_only,
            response: sender,
            retries,
        };
        self.send(message)?;

        receiver.await.map_err(|recv_error| {
            PeerNetworkError::OtherInternalError(format!(
                "Failed to receive response: {:?}",
                recv_error
            ))
        })?
    }

    pub async fn request_payload_to_be_gossiped_from_network(
        &self,
        evm_payload_hash: B256,
        use_trusted_peers_only: bool,
    ) -> Result<(), PeerNetworkError> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let message = PeerNetworkServiceMessage::RequestDataFromNetwork {
            data_request: GossipDataRequest::ExecutionPayload(evm_payload_hash),
            use_trusted_peers_only,
            response: sender,
            retries: DATA_REQUEST_RETRIES,
        };
        self.send(message)?;

        receiver.await.map_err(|recv_error| {
            PeerNetworkError::OtherInternalError(format!(
                "Failed to receive response: {:?}",
                recv_error
            ))
        })?
    }

    pub async fn request_chunk_from_network(
        &self,
        chunk_path_hash: ChunkPathHash,
        use_trusted_peers_only: bool,
    ) -> Result<(), PeerNetworkError> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let message = PeerNetworkServiceMessage::RequestDataFromNetwork {
            data_request: GossipDataRequest::Chunk(chunk_path_hash),
            use_trusted_peers_only,
            response: sender,
            retries: DATA_REQUEST_RETRIES,
        };
        self.send(message)?;

        receiver.await.map_err(|recv_error| {
            PeerNetworkError::OtherInternalError(format!(
                "Failed to receive response: {:?}",
                recv_error
            ))
        })?
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

    #[test]
    fn address_encode_decode_roundtrip_short_buffer() {
        let original_address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(100, 0, 0, 1), 2000));
        let mut buf = bytes::BytesMut::with_capacity(10);
        let size = encode_address(&original_address, &mut buf);
        assert_eq!(size, 7);
        assert_eq!(buf.len(), 7);

        let (decoded_address, consumed) = decode_address(&buf[..]);
        assert_eq!(consumed, 7);
        assert_eq!(decoded_address, original_address);
    }
}
