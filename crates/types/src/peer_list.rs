use crate::{
    BlockHash, ChunkPathHash, Compact, IrysAddress, IrysPeerId, PeerAddress, ProtocolVersion,
};

use crate::v2::GossipDataRequestV2;
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
use tracing::debug;

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

    /// Base method to increase score by a given amount
    fn increase_by(&mut self, amount: u16) {
        self.0 = (self.0 + amount).min(Self::MAX);
    }

    /// Base method to decrease score by a given amount
    fn decrease_by(&mut self, amount: u16) {
        self.0 = self.0.saturating_sub(amount);
    }

    pub fn decrease_slow(&mut self) {
        self.decrease_by(1);
    }

    pub fn decrease_offline(&mut self, msg: &str) {
        debug!("Decreasing peer score due to offline status: {}", msg);
        self.decrease_by(3);
    }

    pub fn decrease_bogus_data(&mut self, msg: &str) {
        debug!("Decreasing peer score due to bogus data: {}", msg);
        self.decrease_by(5);
    }

    pub fn increase_online(&mut self) {
        self.increase_by(1);
    }

    pub fn set_to_max(&mut self) {
        self.0 = Self::MAX;
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
pub struct PeerListItemInner {
    pub reputation_score: PeerScore,
    pub response_time: u16,
    pub address: PeerAddress,
    pub last_seen: u64,
    pub is_online: bool,
    pub protocol_version: ProtocolVersion,
    /// Mining/staking address for this peer
    /// None means: old record from DB - will be populated from DB key on load
    pub mining_address: Option<IrysAddress>,
}

impl Default for PeerListItemInner {
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
            protocol_version: ProtocolVersion::default(),
            mining_address: None,
        }
    }
}

/// PeerListItem is the main type used throughout the codebase.
/// It has non-optional peer_id and mining_address fields for type safety.
/// PeerListItemInner is only used for database serialization.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Hash)]
pub struct PeerListItem {
    /// Peer network identifier (separate from mining address)
    pub peer_id: IrysPeerId,
    /// Mining/staking address for this peer
    pub mining_address: IrysAddress,
    pub reputation_score: PeerScore,
    pub response_time: u16,
    pub address: PeerAddress,
    pub last_seen: u64,
    pub is_online: bool,
    pub protocol_version: ProtocolVersion,
}

impl PeerListItem {
    /// Create a PeerListItem from PeerListItemInner loaded from a database.
    /// Uses the mining_address_key (DB key) as fallback for missing fields.
    pub fn from_inner(inner: PeerListItemInner, peer_id: IrysPeerId) -> Self {
        // For legacy peers, the key used to be the mining address. They have the same byte layout,
        //  so we can use it as a fallback for LEGACY PEERS ONLY.
        let mining_address = inner.mining_address.unwrap_or_else(|| {
            tracing::warn!(
                "Legacy fallback: using peer_id as mining_address for peer_id={:?}, last_seen={}, protocol_version={:?}",
                peer_id,
                inner.last_seen,
                inner.protocol_version
            );
            // Convert peer_id to bytes and back to IrysAddress to avoid direct conversion
            // This is needed because peer_id and mining_address have the same byte layout for legacy peers
            let peer_id_bytes: [u8; 20] = peer_id.into();
            IrysAddress::from(peer_id_bytes)
        });
        Self {
            peer_id,
            mining_address,
            reputation_score: inner.reputation_score,
            response_time: inner.response_time,
            address: inner.address,
            last_seen: inner.last_seen,
            is_online: inner.is_online,
            protocol_version: inner.protocol_version,
        }
    }

    /// Convert to PeerListItemInner for database storage. NEVER use this method ANYWHERE except
    /// for database serialization!
    pub fn to_inner(&self) -> PeerListItemInner {
        PeerListItemInner {
            mining_address: Some(self.mining_address),
            reputation_score: self.reputation_score,
            response_time: self.response_time,
            address: self.address,
            last_seen: self.last_seen,
            is_online: self.is_online,
            protocol_version: self.protocol_version,
        }
    }
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, Arbitrary, PartialOrd, Ord, Hash, Eq, PartialEq,
)]
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

const IPV4_TAG: u8 = 0;
const IPV6_TAG: u8 = 1;

const TAG_SIZE: usize = 1;
const IPV4_ADDR_SIZE: usize = 4;
const IPV6_ADDR_SIZE: usize = 16;
const PORT_SIZE: usize = 2;

pub fn encode_address<B>(address: &SocketAddr, buf: &mut B) -> usize
where
    B: bytes::BufMut + AsMut<[u8]>,
{
    let mut size = 0;
    match address {
        SocketAddr::V4(addr4) => {
            buf.put_u8(IPV4_TAG); // Tag for IPv4
            buf.put_slice(&addr4.ip().octets());
            buf.put_u16(addr4.port());
            size += TAG_SIZE + IPV4_ADDR_SIZE + PORT_SIZE;
        }
        SocketAddr::V6(addr6) => {
            buf.put_u8(IPV6_TAG); // Tag for IPv6
            buf.put_slice(&addr6.ip().octets());
            buf.put_u16(addr6.port());
            size += TAG_SIZE + IPV6_ADDR_SIZE + PORT_SIZE;
        }
    };
    size
}

pub fn decode_address(buf: &[u8]) -> (SocketAddr, usize) {
    let tag = buf[0];
    let address = match tag {
        IPV4_TAG => {
            // IPv4 address (needs 4 bytes IP + 2 bytes port after tag)
            if buf.len() < TAG_SIZE + IPV4_ADDR_SIZE + PORT_SIZE {
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0))
            } else {
                let ip_octets: [u8; 4] =
                    buf[TAG_SIZE..TAG_SIZE + IPV4_ADDR_SIZE].try_into().unwrap();
                let port = u16::from_be_bytes(
                    buf[TAG_SIZE + IPV4_ADDR_SIZE..TAG_SIZE + IPV4_ADDR_SIZE + PORT_SIZE]
                        .try_into()
                        .unwrap(),
                );
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip_octets), port))
            }
        }
        IPV6_TAG => {
            // IPv6 address (needs 16 bytes IP + 2 bytes port after tag)
            if buf.len() < TAG_SIZE + IPV6_ADDR_SIZE + PORT_SIZE {
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0))
            } else {
                let mut ip_octets = [0_u8; 16];
                ip_octets.copy_from_slice(&buf[TAG_SIZE..TAG_SIZE + IPV6_ADDR_SIZE]);
                let port = u16::from_be_bytes(
                    buf[TAG_SIZE + IPV6_ADDR_SIZE..TAG_SIZE + IPV6_ADDR_SIZE + PORT_SIZE]
                        .try_into()
                        .unwrap(),
                );
                SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(ip_octets), port, 0, 0))
            }
        }
        _ => SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0)),
    };

    let consumed = match tag {
        IPV4_TAG => TAG_SIZE + IPV4_ADDR_SIZE + PORT_SIZE,
        IPV6_TAG => TAG_SIZE + IPV6_ADDR_SIZE + PORT_SIZE,
        _ => TAG_SIZE, // unknown tag: we only consumed the tag byte
    };
    (address, consumed)
}

/// This may seem a little heavy handed to implement Compact for the entire
/// struct but it has the advantage of being better able to handle migrations
/// in the future. New fields can be appended to the end of the compact buffer
/// and the from_compact can populate new fields with defaults if the buffer
/// length is from a previous smaller version of the struct.
impl Compact for PeerListItemInner {
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

        // Always write the is_online flag (1 byte). Decoding is backward-compatible and will
        // default to false if this byte is missing
        buf.put_u8(if self.is_online { 1 } else { 0 });
        size += 1;

        buf.put_u32(self.protocol_version as u32);
        size += 4;

        // Append mining_address (20 bytes) at the end for backward compatibility
        // Old records won't have this field, will decode as None
        if let Some(mining_address) = &self.mining_address {
            buf.put_slice(mining_address.as_ref());
            size += 20;
        }

        size
    }

    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        // Compact layout (in order):
        // - u16 reputation_score (2 bytes, BE)
        // - u16 response_time (2 bytes, BE)
        // - SocketAddr gossip (tagged: 1 byte + IPv4(4)+port(2)=7 or IPv6(16)+port(2)=19)
        // - SocketAddr api (same as above)
        // - RethPeerInfo (variable size; see its Compact impl)
        // - u64 last_seen (8 bytes, BE)
        // - optional u8 is_online (1 byte). Decoder tolerates it being absent and defaults to false.
        // - optional u32 protocol_version (4 bytes, BE). Appended after is_online. Decoder tolerates
        //   it being absent (for backward compatibility with older compact encodings) and defaults
        //   to ProtocolVersion::default().
        //
        // Decoding strategy:
        // - For the fixed-size prefix (score, response_time) we advance the buf slice in-place.
        // - For variable-size sections we call helpers that return the remaining slice.
        // - For the tail (last_seen [+ optional is_online]) we read without immediately advancing,
        //   track a local total_consumed, and compute the correct remainder to return.
        let mut buf = buf;

        // If we don't even have the fixed-size prefix (4 bytes), fall back to defaults and no remainder.
        if buf.len() < 4 {
            return (Self::default(), &[]);
        }

        // Decode the fixed-size prefix and advance the buffer.
        let reputation_score = PeerScore(u16::from_be_bytes(buf[0..2].try_into().unwrap()));
        buf.advance(2);
        let response_time = u16::from_be_bytes(buf[0..2].try_into().unwrap());
        buf.advance(2);

        // If the buffer ends here, the addresses and all subsequent fields are missing.
        // Return sensible defaults and an empty remainder (we consumed the entire provided buffer).
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
                    protocol_version: ProtocolVersion::default(),
                    mining_address: None,
                },
                &[],
            );
        }

        let (gossip_address, consumed) = decode_address(buf);
        buf.advance(consumed);

        let (api_address, consumed) = decode_address(buf);
        buf.advance(consumed);

        let (reth_peer_info, buf) = RethPeerInfo::from_compact(buf, buf.len());

        let address = PeerAddress {
            gossip: gossip_address,
            api: api_address,
            execution: reth_peer_info,
        };

        // Read last_seen (8 bytes) if available. We do not advance the buf slice here;
        // instead we track how many bytes we will consume from this tail section via
        // total_consumed, and advance the returned remainder accordingly.
        let last_seen = if buf.len() >= 8 {
            u64::from_be_bytes(buf[0..8].try_into().unwrap())
        } else {
            0
        };

        let mut total_consumed = 8;
        let mut is_online = false;

        if buf.len() > total_consumed {
            is_online = buf[total_consumed] == 1;
            total_consumed += 1;
        }

        // Read protocol_version (4 bytes) if available. If it's not available, it means that it's a
        //  v1 record, so we default to ProtocolVersion::V1.
        let mut protocol_version = ProtocolVersion::V1;
        if buf.len() >= total_consumed + 4 {
            let version_bytes: [u8; 4] = buf[total_consumed..total_consumed + 4]
                .try_into()
                .expect("slice with incorrect length");
            let version_u32 = u32::from_be_bytes(version_bytes);
            protocol_version = ProtocolVersion::from(version_u32);
            total_consumed += 4;
        }

        // Read mining_address (20 bytes) if available
        // This field was added after protocol_version, so older records won't have it
        let mining_address = if buf.len() >= total_consumed + 20 {
            let address_bytes: [u8; 20] = buf[total_consumed..total_consumed + 20]
                .try_into()
                .expect("slice with incorrect length");
            total_consumed += 20;
            Some(IrysAddress::from(address_bytes))
        } else {
            None // Old record without mining_address
        };

        (
            Self {
                reputation_score,
                response_time,
                address,
                last_seen,
                is_online,
                protocol_version,
                mining_address,
            },
            // Advance the remainder past the bytes we logically consumed in this tail section.
            &buf[total_consumed.min(buf.len())..],
        )
    }
}

#[derive(Copy, Clone, Debug)]
pub struct HandshakeMessage {
    /// Gossip address of the peer to handshake with
    pub gossip_address: SocketAddr,
    /// Fallback for older versions - API address of the peer to handshake with
    pub api_address: SocketAddr,
    /// If true, tries to handshake even if the peer is already known and the last handshake
    /// was successful
    pub force: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnnouncementFinishedMessage {
    pub peer_gossip_address: SocketAddr,
    /// Fallback for older versions - API address of the peer
    pub peer_api_address: SocketAddr,
    pub success: bool,
    pub retry: bool,
}

#[derive(Debug)]
pub enum PeerNetworkServiceMessage {
    Handshake(HandshakeMessage),
    AnnounceYourselfToPeer(PeerListItem),
    AnnouncementFinished(AnnouncementFinishedMessage),
    RequestDataFromNetwork {
        data_request: GossipDataRequestV2,
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
    #[error("Invalid block body from peer {peer_id}: {reason}")]
    InvalidBlockBody { peer_id: IrysPeerId, reason: String },
}

impl From<SendError<PeerNetworkServiceMessage>> for PeerNetworkError {
    fn from(err: SendError<PeerNetworkServiceMessage>) -> Self {
        Self::InternalSendError(format!("Failed to send a message: {err:?}"))
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
        gossip_address: SocketAddr,
        force: bool,
    ) -> Result<(), SendError<PeerNetworkServiceMessage>> {
        let message = PeerNetworkServiceMessage::Handshake(HandshakeMessage {
            api_address,
            gossip_address,
            force,
        });
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
            data_request: GossipDataRequestV2::BlockHeader(block_hash),
            use_trusted_peers_only,
            response: sender,
            retries,
        };
        self.send(message)?;

        receiver.await.map_err(|recv_error| {
            PeerNetworkError::OtherInternalError(format!(
                "Failed to receive response: {recv_error:?}"
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
            data_request: GossipDataRequestV2::ExecutionPayload(evm_payload_hash),
            use_trusted_peers_only,
            response: sender,
            retries: DATA_REQUEST_RETRIES,
        };
        self.send(message)?;

        receiver.await.map_err(|recv_error| {
            PeerNetworkError::OtherInternalError(format!(
                "Failed to receive response: {recv_error:?}"
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
            data_request: GossipDataRequestV2::Chunk(chunk_path_hash),
            use_trusted_peers_only,
            response: sender,
            retries: DATA_REQUEST_RETRIES,
        };
        self.send(message)?;

        receiver.await.map_err(|recv_error| {
            PeerNetworkError::OtherInternalError(format!(
                "Failed to receive response: {recv_error:?}"
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
        let mining_addr = IrysAddress::from([1_u8; 20]);
        let peer_list_item_inner = PeerListItemInner {
            reputation_score: PeerScore::new(75),
            response_time: 150,
            address: PeerAddress {
                gossip: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 100), 8080)),
                api: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 5), 3000)),
                execution: RethPeerInfo {
                    peering_tcp_addr: SocketAddr::V4(SocketAddrV4::new(
                        Ipv4Addr::new(172, 16, 0, 1),
                        30303,
                    )),
                    peer_id: reth_transaction_pool::PeerId::random(),
                },
            },
            last_seen: 1704067200000, // Jan 1, 2024 timestamp in milliseconds
            is_online: true,
            protocol_version: ProtocolVersion::V2,
            mining_address: Some(mining_addr),
        };
        let mut buf = bytes::BytesMut::with_capacity(150);
        peer_list_item_inner.to_compact(&mut buf);
        let (decoded, _) = PeerListItemInner::from_compact(&buf[..], buf.len());
        assert_eq!(peer_list_item_inner, decoded);
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

    mod peer_score_tests {
        use super::*;
        use rstest::rstest;

        #[rstest]
        #[case(50, 5, 45)]
        #[case(3, 5, 0)]
        #[case(100, 5, 95)]
        #[case(2, 5, 0)]
        fn test_decrease_bogus_data(
            #[case] initial: u16,
            #[case] _decrease: u16,
            #[case] expected: u16,
        ) {
            let mut score = PeerScore::new(initial);
            score.decrease_bogus_data("test");
            assert_eq!(score.get(), expected);
        }

        #[rstest]
        #[case(50, 3, 47)]
        #[case(1, 3, 0)]
        #[case(100, 3, 97)]
        #[case(2, 3, 0)]
        fn test_decrease_offline(
            #[case] initial: u16,
            #[case] _decrease: u16,
            #[case] expected: u16,
        ) {
            let mut score = PeerScore::new(initial);
            score.decrease_offline("test");
            assert_eq!(score.get(), expected);
        }

        #[rstest]
        #[case(50, 1, 49)]
        #[case(0, 1, 0)]
        #[case(100, 1, 99)]
        fn test_decrease_slow_response(
            #[case] initial: u16,
            #[case] _decrease: u16,
            #[case] expected: u16,
        ) {
            let mut score = PeerScore::new(initial);
            score.decrease_slow();
            assert_eq!(score.get(), expected);
        }

        #[rstest]
        #[case(PeerScore::ACTIVE_THRESHOLD + 1, 3, false)]
        #[case(PeerScore::ACTIVE_THRESHOLD + 3, 3, true)]
        #[case(PeerScore::ACTIVE_THRESHOLD + 5, 5, true)]
        #[case(PeerScore::ACTIVE_THRESHOLD, 1, false)]
        fn test_active_threshold_crossing(
            #[case] initial: u16,
            #[case] decrease: u16,
            #[case] should_be_active: bool,
        ) {
            let mut score = PeerScore::new(initial);
            score.decrease_by(decrease);
            assert_eq!(score.is_active(), should_be_active);
        }

        #[rstest]
        #[case(PeerScore::PERSISTENCE_THRESHOLD + 1, 2, false)]
        #[case(PeerScore::PERSISTENCE_THRESHOLD + 5, 5, true)]
        #[case(PeerScore::PERSISTENCE_THRESHOLD, 1, false)]
        fn test_persistence_threshold_crossing(
            #[case] initial: u16,
            #[case] decrease: u16,
            #[case] should_be_persistable: bool,
        ) {
            let mut score = PeerScore::new(initial);
            score.decrease_by(decrease);
            assert_eq!(score.is_persistable(), should_be_persistable);
        }

        #[test]
        fn test_score_clamping() {
            let score = PeerScore::new(150);
            assert_eq!(score.get(), PeerScore::MAX);

            let mut score = PeerScore::new(PeerScore::MAX);
            score.increase_by(10);
            assert_eq!(score.get(), PeerScore::MAX);

            let mut score = PeerScore::new(0);
            score.decrease_by(10);
            assert_eq!(score.get(), 0);
        }

        #[test]
        fn test_combined_decreases() {
            let mut score = PeerScore::new(50);

            score.decrease_bogus_data("bogus data");
            assert_eq!(score.get(), 45);

            score.decrease_offline("test");
            assert_eq!(score.get(), 42);

            score.decrease_slow();
            assert_eq!(score.get(), 41);

            score.decrease_offline("test");
            assert_eq!(score.get(), 38);
        }

        #[rstest]
        #[case(0, 10)]
        #[case(50, 50)]
        #[case(90, 20)]
        fn test_increase_and_decrease_cycles(#[case] start: u16, #[case] cycles: u16) {
            let mut score = PeerScore::new(start);

            for _ in 0..cycles {
                let before = score.get();
                score.increase_online();
                if before < PeerScore::MAX {
                    assert_eq!(score.get(), before + 1);
                } else {
                    assert_eq!(score.get(), PeerScore::MAX);
                }

                score.decrease_slow();
                if score.get() > 0 {
                    assert_eq!(score.get(), before);
                }
            }
        }

        #[rstest]
        #[case(0_u16, vec![(0_u8, 10_u16), (1, 5), (2, 0), (3, 0)])]
        #[case(50, vec![(1, 10), (0, 5), (1, 20), (0, 30)])]
        #[case(100, vec![(0, 10), (1, 110), (0, 50)])]
        fn test_score_bounds_with_operations(
            #[case] initial: u16,
            #[case] operations: Vec<(u8, u16)>,
        ) {
            let mut score = PeerScore::new(initial);

            for (op_type, amount) in operations {
                match op_type {
                    0 => score.increase_by(amount),
                    1 => score.decrease_by(amount),
                    2 => score.decrease_bogus_data("bogus data"),
                    3 => score.decrease_offline("offline"),
                    _ => {}
                }

                // Score is always >= MIN due to saturating arithmetic
                assert!(score.get() <= PeerScore::MAX);
            }
        }

        #[rstest]
        #[case(0, 10, 0)]
        #[case(5, 10, 0)]
        #[case(100, 10, 90)]
        #[case(50, 200, 0)]
        fn test_decrease_saturating_behavior(
            #[case] initial: u16,
            #[case] decrease: u16,
            #[case] expected: u16,
        ) {
            let mut score = PeerScore::new(initial);
            score.decrease_by(decrease);
            assert_eq!(score.get(), expected);
        }

        #[rstest]
        #[case(90, 10, 100)]
        #[case(95, 10, 100)]
        #[case(50, 100, 100)]
        #[case(0, 100, 100)]
        fn test_increase_clamping_behavior(
            #[case] initial: u16,
            #[case] increase: u16,
            #[case] expected: u16,
        ) {
            let mut score = PeerScore::new(initial);
            score.increase_by(increase);
            assert_eq!(score.get(), expected);
        }

        #[test]
        fn test_decrease_operations_relative_impact() {
            let initial = 50_u16;
            let mut score1 = PeerScore::new(initial);
            let mut score2 = PeerScore::new(initial);
            let mut score3 = PeerScore::new(initial);

            score1.decrease_slow();
            score2.decrease_offline("test");
            score3.decrease_bogus_data("some error message");

            assert_eq!(score1.get(), 49);
            assert_eq!(score2.get(), 47);
            assert_eq!(score3.get(), 45);

            assert!(score1.get() > score2.get());
            assert!(score2.get() > score3.get());
        }
    }

    #[test]
    fn peer_list_item_compact_remainder_empty() {
        let item = PeerListItemInner::default();
        let mut buf = bytes::BytesMut::with_capacity(64);
        item.to_compact(&mut buf);
        let (_decoded, remainder) = PeerListItemInner::from_compact(&buf[..], buf.len());
        assert!(
            remainder.is_empty(),
            "expected no remainder after decoding full buffer"
        );
    }

    #[test]
    fn peer_list_item_from_compact_missing_is_online_and_protocol_version() {
        let item = PeerListItemInner {
            protocol_version: ProtocolVersion::V2,
            ..PeerListItemInner::default()
        };
        let mut buf = bytes::BytesMut::with_capacity(64);
        item.to_compact(&mut buf);
        // Remove the optional is_online byte and protocol version bytes
        assert!(!buf.is_empty());
        buf.truncate(buf.len() - 5);

        let (decoded, remainder) = PeerListItemInner::from_compact(&buf[..], buf.len());
        assert!(
            remainder.is_empty(),
            "expected no remainder after decoding without is_online byte"
        );

        assert_eq!(decoded.reputation_score, item.reputation_score);
        assert_eq!(decoded.response_time, item.response_time);
        assert_eq!(decoded.address, item.address);
        assert_eq!(decoded.last_seen, item.last_seen);
        assert!(!decoded.is_online);
        assert_ne!(decoded.protocol_version, item.protocol_version);
        // For the records that don't have a protocol version, it should default to V1
        assert_eq!(decoded.protocol_version, ProtocolVersion::V1);
    }
}
