use crate::{
    decode_address, encode_address, serialization::string_u64, serialization::string_usize,
    Arbitrary, IrysPeerId, IrysSignature, RethPeerInfo, H256,
};
use crate::{IrysAddress, U256};
use alloy_primitives::keccak256;
use alloy_rlp::Encodable as _;
use bytes::Buf as _;
use reth_codecs::Compact;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::hash::Hash;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum PeerResponse {
    #[serde(rename = "accepted")]
    Accepted(HandshakeResponse),
    #[serde(rename = "rejected")]
    Rejected(RejectedResponse),
}

// Explicit integer protocol versions
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash, Arbitrary,
)]
#[repr(u32)]
pub enum ProtocolVersion {
    V1 = 1,
    V2 = 2,
    VersionPD = 9000,
}

impl Default for ProtocolVersion {
    fn default() -> Self {
        Self::current()
    }
}

impl From<u32> for ProtocolVersion {
    fn from(v: u32) -> Self {
        match v {
            1 => Self::V1,
            2 => Self::V2,
            9000 => Self::VersionPD,
            _ => Self::default(),
        }
    }
}

impl ProtocolVersion {
    pub const fn current() -> Self {
        Self::VersionPD
    }

    pub fn supported_versions() -> &'static [Self] {
        &[Self::V1, Self::V2, Self::VersionPD]
    }

    pub fn supported_versions_u32() -> &'static [u32] {
        &[Self::V1 as u32, Self::V2 as u32, Self::VersionPD as u32]
    }
}

/// We can't derive these impls directly due to the RLP not supporting repr structs/enums
impl alloy_rlp::Encodable for ProtocolVersion {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        (*self as u32).encode(out);
    }

    fn length(&self) -> usize {
        (*self as u32).length()
    }
}

impl alloy_rlp::Decodable for ProtocolVersion {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let value = u32::decode(buf)?;
        match value {
            1 => Ok(Self::V1),
            2 => Ok(Self::V2),
            9000 => Ok(Self::VersionPD),
            _ => Err(alloy_rlp::Error::Custom("unknown protocol version")),
        }
    }
}

/// Builds a user-agent string to identify this node implementation in the P2P network.
///
/// Format: "{name}/{version} ({os}/{arch})"
///
/// # Examples
/// ```
/// use irys_types::build_user_agent;
///
/// let ua = build_user_agent("my-node", "1.2.0");
/// //assert_eq!(ua, "my-node/1.2.0 (linux/x86_64)");
///
/// let ua = build_user_agent("irys-p2p", "0.1.0");
/// //assert_eq!(ua, "irys-p2p/0.1.0 (macos/aarch64)");
/// ```
///
/// The OS and architecture are automatically detected using std::env::consts.
pub fn build_user_agent(name: &str, version: &str) -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    format!("{}/{} ({}/{})", name, version, os, arch)
}

/// Parses a user-agent string into its component parts.
///
/// Input Format: "{name}/{version} ({os}/{arch})"
///
/// # Examples
/// ```
/// use irys_types::parse_user_agent;
///
/// let (name, version, os, arch) = parse_user_agent("my-node/1.2.0 (linux/x86_64)").unwrap();
/// assert_eq!(name, "my-node");
/// assert_eq!(version, "1.2.0");
/// assert_eq!(os, "linux");
/// assert_eq!(arch, "x86_64");
///
/// let (name, version, os, arch) = parse_user_agent("irys-p2p/0.1.0 (macos/aarch64)").unwrap();
/// assert_eq!(name, "irys-p2p");
/// assert_eq!(version, "0.1.0");
/// assert_eq!(os, "macos");
/// assert_eq!(arch, "aarch64");
/// ```
///
/// Returns None if the user-agent string doesn't match the expected format.
pub fn parse_user_agent(user_agent: &str) -> Option<(String, String, String, String)> {
    // Split into main parts and system info
    let parts: Vec<&str> = user_agent.split(" (").collect();
    if parts.len() != 2 {
        return None;
    }

    // Parse name/version
    let name_version: Vec<&str> = parts[0].split('/').collect();
    if name_version.len() != 2 {
        return None;
    }

    // Parse os/arch
    let system_info = parts[1].trim_end_matches(')');
    let system_parts: Vec<&str> = system_info.split('/').collect();
    if system_parts.len() != 2 {
        return None;
    }

    Some((
        name_version[0].to_string(),
        name_version[1].to_string(),
        system_parts[0].to_string(),
        system_parts[1].to_string(),
    ))
}

/// Example handshake request JSON:
/// ```json
/// {
///   "version": "1.2.0",             // Node version using semver
///   "protocol_version": "1",        // Supported protocol version (V1, V2, etc)
///   "mining_address": "0x11111...", // Mining address as hex
///   "chain_id": 1270,               // Network chain identifier
///   "address": "203.0.113.1:8333",  // External listening address/port
///   "timestamp": 1645567124437,     // Current timestamp in milliseconds
///   "user_agent": "my-node/1.2.0"   // Optional identification string
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeRequestV1 {
    pub version: Version,
    pub protocol_version: ProtocolVersion,
    pub mining_address: IrysAddress,
    pub chain_id: u64,
    pub address: PeerAddress,
    pub timestamp: u64,
    pub user_agent: Option<String>,
    pub signature: IrysSignature,
}

/// V2 HandshakeRequest - includes peer_id for P2P identification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeRequestV2 {
    pub version: Version,
    pub protocol_version: ProtocolVersion,
    pub mining_address: IrysAddress, // Still used for signing/staking
    pub peer_id: IrysPeerId,         // NEW: Required for V2
    pub chain_id: u64,
    pub address: PeerAddress,
    pub timestamp: u64,
    pub user_agent: Option<String>,
    pub consensus_config_hash: H256,
    pub signature: IrysSignature,
}

/// Legacy type alias for backward compatibility - maps to V1
pub type HandshakeRequest = HandshakeRequestV1;

impl Default for HandshakeRequestV1 {
    fn default() -> Self {
        Self {
            version: Version::new(0, 1, 0),
            mining_address: IrysAddress::ZERO,
            protocol_version: ProtocolVersion::V1,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            chain_id: 0,
            address: PeerAddress::default(),
            user_agent: None,
            signature: IrysSignature::default(),
        }
    }
}

impl Default for HandshakeRequestV2 {
    fn default() -> Self {
        Self {
            version: Version::new(0, 1, 0), // Default to 0.1.0
            mining_address: IrysAddress::ZERO,
            peer_id: IrysPeerId::ZERO,
            protocol_version: ProtocolVersion::current(),
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            chain_id: 0,
            address: PeerAddress::default(),
            user_agent: None,
            consensus_config_hash: H256::zero(),
            signature: IrysSignature::default(),
        }
    }
}

impl HandshakeRequestV1 {
    fn encode_for_signing<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let mut size = 0;
        size += encode_version_for_signing(&self.version, buf);
        size += (self.protocol_version as u32).to_compact(buf);
        size += self.mining_address.to_compact(buf);
        size += self.chain_id.to_compact(buf);
        size += self.address.to_compact(buf);
        size += self.timestamp.to_compact(buf);
        size += self.user_agent.to_compact(buf);
        size
    }

    pub fn signature_hash(&self) -> [u8; 32] {
        let mut bytes = Vec::new();
        self.encode_for_signing(&mut bytes);
        keccak256(&bytes).0
    }

    pub fn verify_signature(&self) -> bool {
        self.signature
            .validate_signature(self.signature_hash(), self.mining_address)
    }
}

impl HandshakeRequestV2 {
    /// Rely on RLP encoding for signing
    fn encode_for_signing(&self, out: &mut dyn bytes::BufMut) {
        // Manually encode using RLP, excluding the signature field
        let payload_length = encode_version_rlp_length(&self.version)
            + encode_protocol_version_rlp_length(&self.protocol_version)
            + self.mining_address.length()
            + self.peer_id.length()
            + self.chain_id.length()
            + encode_peer_address_rlp_length(&self.address)
            + self.timestamp.length()
            + self
                .user_agent
                .as_ref()
                .map_or(1, alloy_rlp::Encodable::length) // empty string for None
            + self.consensus_config_hash.length();

        alloy_rlp::Header {
            list: true,
            payload_length,
        }
        .encode(out);
        encode_version_rlp(&self.version, out);
        encode_protocol_version_rlp(&self.protocol_version, out);
        self.mining_address.encode(out);
        self.peer_id.encode(out);
        self.chain_id.encode(out);
        encode_peer_address_rlp(&self.address, out);
        self.timestamp.encode(out);
        if let Some(ua) = &self.user_agent {
            ua.encode(out);
        } else {
            "".encode(out);
        }
        self.consensus_config_hash.encode(out);
    }

    pub fn signature_hash(&self) -> [u8; 32] {
        let mut bytes = Vec::new();
        self.encode_for_signing(&mut bytes);
        keccak256(&bytes).0
    }

    pub fn verify_signature(&self) -> bool {
        self.signature
            .validate_signature(self.signature_hash(), self.mining_address)
    }
}

pub fn encode_version_for_signing<B>(version: &Version, buf: &mut B) -> usize
where
    B: bytes::BufMut + AsMut<[u8]>,
{
    let mut size = 0;
    size += version.major.to_compact(buf);
    size += version.minor.to_compact(buf);
    size += version.patch.to_compact(buf);
    // size += version.pre.to_string().to_compact(buf);
    // size += version.build.to_string().to_compact(buf);
    size
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialOrd, Ord, Hash, Eq, PartialEq, Arbitrary,
)]
#[serde(deny_unknown_fields)]
pub struct PeerAddress {
    pub gossip: SocketAddr,
    pub api: SocketAddr,
    pub execution: RethPeerInfo,
}

impl Default for PeerAddress {
    fn default() -> Self {
        Self {
            gossip: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
            api: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8081),
            execution: RethPeerInfo::default(),
        }
    }
}

impl Compact for PeerAddress {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let mut size = 0;
        size += encode_address(&self.gossip, buf);
        size += encode_address(&self.api, buf);
        size += self.execution.to_compact(buf);
        size
    }

    fn from_compact(buf: &[u8], _: usize) -> (Self, &[u8]) {
        let mut buf = buf;
        let (gossip, consumed) = decode_address(buf);
        buf.advance(consumed);
        let (api, consumed) = decode_address(buf);
        buf.advance(consumed);
        let (execution, buf) = RethPeerInfo::from_compact(buf, buf.len());
        (
            Self {
                gossip,
                api,
                execution,
            },
            buf,
        )
    }
}

/// Example serialized JSON AcceptedResponse V1:
/// ```json
/// {
///   "status": "accepted",         // comes from PeerResponse Enum
///   "version": "1.2.0",           // semver formatted
///   "protocol_version": "1",      // V1 protocol version
///   "peers": [
///     "203.0.113.1:8333",         // IPv4 address:port
///     "203.0.113.2:8333",
///     "[2001:db8::1]:8333",       // IPv6 addresses use [] notation
///     "[2001:db8::2]:8333"
///   ],
///   "timestamp": 1645567124437,   // Number of milliseconds since UNIX epoch
///   "message": "Welcome to the network"  // or null if None
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeResponseV1 {
    pub version: Version,
    pub protocol_version: ProtocolVersion,
    // pub features: Vec<Feature>,  // perhaps something like "features": ["DHT", "NAT"], in the future
    pub peers: Vec<PeerAddress>,
    pub timestamp: u64,
    pub message: Option<String>,
}

impl Default for HandshakeResponseV1 {
    fn default() -> Self {
        Self {
            version: Version::new(0, 1, 0), // Default to 0.1.0
            protocol_version: ProtocolVersion::V1,
            peers: Vec::new(),
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            message: None,
        }
    }
}

/// Example serialized JSON AcceptedResponse V2:
/// ```json
/// {
///   "status": "accepted",         // comes from PeerResponse Enum
///   "version": "1.2.0",           // semver formatted
///   "protocol_version": "2",      // V2 protocol version
///   "peers": [
///     "203.0.113.1:8333",         // IPv4 address:port
///     "203.0.113.2:8333",
///     "[2001:db8::1]:8333",       // IPv6 addresses use [] notation
///     "[2001:db8::2]:8333"
///   ],
///   "timestamp": 1645567124437,   // Number of milliseconds since UNIX epoch
///   "message": "Welcome to the network",  // or null if None
///   "consensus_config_hash": "0x..."  // Hash of consensus config
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeResponseV2 {
    pub version: Version,
    pub protocol_version: ProtocolVersion,
    // pub features: Vec<Feature>,  // perhaps something like "features": ["DHT", "NAT"], in the future
    pub peers: Vec<PeerAddress>,
    pub timestamp: u64,
    pub message: Option<String>,
    pub consensus_config_hash: H256,
}

impl Default for HandshakeResponseV2 {
    fn default() -> Self {
        Self {
            version: Version::new(0, 1, 0), // Default to 0.1.0
            protocol_version: ProtocolVersion::current(),
            peers: Vec::new(),
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            message: None,
            consensus_config_hash: H256::zero(),
        }
    }
}

/// Legacy type alias for backward compatibility - maps to V2
pub type HandshakeResponse = HandshakeResponseV2;

/// Example serialized JSON RejectedResponse:
/// ```json
/// {
///   "status":"rejected",                // comes from PeerResponse Enum
///   "reason": "max_peers_reached",      // snake_case of RejectionReason enum variant
///   "message": "Node is at capacity",   // Optional string message, null if None
///   "retry_after": 3600                 // Optional seconds to wait before retry, null if None
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedResponse {
    pub reason: RejectionReason,
    pub message: Option<String>,
    pub retry_after: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    Accepted,     // Peer accepts the connection
    Rejected,     // Peer explicitly declines the connection
    Busy,         // Peer is at capacity and can't accept new connections
    Incompatible, // Protocol/version mismatch prevents connection
    Maintenance,  // Peer is temporarily unavailable for maintenance
    Redirected,   // Peer suggests connecting to another node instead
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionReason {
    MaxPeersReached,    // Node is at capacity for peer connections
    VersionMismatch,    // Incompatible software versions
    ProtocolMismatch,   // Incompatible protocol versions
    InvalidCredentials, // If the network requires authentication
    BlackListed,        // Requesting peer's address is blacklisted
    InvalidFeatures,    // Requesting peer's features are incompatible
    RegionRestricted,   // Geographical restrictions (if applicable)
    MaintenanceMode,    // Node is in maintenance mode
    RateLimited,        // Too many connection attempts
    NetworkMismatch,    // Wrong network (e.g. testnet vs mainnet)
    BadHandshake,       // Malformed or invalid handshake request
    Untrusted,          // Peer doesn't meet trust requirements
    InternalError,      // Unable to complete request
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    pub version: String,
    pub peer_count: usize,
    #[serde(with = "string_u64")]
    pub chain_id: u64,
    #[serde(with = "string_u64")]
    pub height: u64,
    pub block_hash: H256,
    #[serde(with = "string_u64")]
    pub block_index_height: u64,
    pub block_index_hash: H256,
    #[serde(with = "string_u64")]
    pub pending_blocks: u64,
    pub is_syncing: bool,
    #[serde(with = "string_usize")]
    pub current_sync_height: usize,
    #[serde(with = "string_u64")]
    pub uptime_secs: u64,
    // #[serde(with = "address_base58_stringify")]
    pub mining_address: IrysAddress,
    pub cumulative_difficulty: U256,
}

// RLP encoding implementations for HandshakeRequestV2 types

// Helper functions for RLP encoding (avoiding orphan rule violations)
fn encode_version_rlp(version: &Version, out: &mut dyn bytes::BufMut) {
    alloy_rlp::Header {
        list: true,
        payload_length: version.major.length() + version.minor.length() + version.patch.length(),
    }
    .encode(out);
    version.major.encode(out);
    version.minor.encode(out);
    version.patch.encode(out);
}

fn encode_version_rlp_length(version: &Version) -> usize {
    let payload_length = version.major.length() + version.minor.length() + version.patch.length();
    payload_length + alloy_rlp::length_of_length(payload_length)
}

fn encode_protocol_version_rlp(pv: &ProtocolVersion, out: &mut dyn bytes::BufMut) {
    (*pv as u32).encode(out);
}

fn encode_protocol_version_rlp_length(pv: &ProtocolVersion) -> usize {
    (*pv as u32).length()
}

fn encode_socket_addr_rlp(addr: &SocketAddr, out: &mut dyn bytes::BufMut) {
    match addr {
        SocketAddr::V4(v4) => {
            let ip_bytes = v4.ip().octets();
            let port = v4.port();
            let payload_length = 4_u8.length() + ip_bytes.as_slice().length() + port.length();
            alloy_rlp::Header {
                list: true,
                payload_length,
            }
            .encode(out);
            4_u8.encode(out); // IPv4 tag
            ip_bytes.as_slice().encode(out);
            port.encode(out);
        }
        SocketAddr::V6(v6) => {
            let ip_bytes = v6.ip().octets();
            let port = v6.port();
            let payload_length = 6_u8.length() + ip_bytes.as_slice().length() + port.length();
            alloy_rlp::Header {
                list: true,
                payload_length,
            }
            .encode(out);
            6_u8.encode(out); // IPv6 tag
            ip_bytes.as_slice().encode(out);
            port.encode(out);
        }
    }
}

fn encode_socket_addr_rlp_length(addr: &SocketAddr) -> usize {
    match addr {
        SocketAddr::V4(v4) => {
            let ip_bytes = v4.ip().octets();
            let port = v4.port();
            let payload_length = 4_u8.length() + ip_bytes.as_slice().length() + port.length();
            payload_length + alloy_rlp::length_of_length(payload_length)
        }
        SocketAddr::V6(v6) => {
            let ip_bytes = v6.ip().octets();
            let port = v6.port();
            let payload_length = 6_u8.length() + ip_bytes.as_slice().length() + port.length();
            payload_length + alloy_rlp::length_of_length(payload_length)
        }
    }
}

fn encode_peer_address_rlp(addr: &PeerAddress, out: &mut dyn bytes::BufMut) {
    let payload_length = encode_socket_addr_rlp_length(&addr.gossip)
        + encode_socket_addr_rlp_length(&addr.api)
        + encode_reth_peer_info_rlp_length(&addr.execution);
    alloy_rlp::Header {
        list: true,
        payload_length,
    }
    .encode(out);
    encode_socket_addr_rlp(&addr.gossip, out);
    encode_socket_addr_rlp(&addr.api, out);
    encode_reth_peer_info_rlp(&addr.execution, out);
}

fn encode_peer_address_rlp_length(addr: &PeerAddress) -> usize {
    let payload_length = encode_socket_addr_rlp_length(&addr.gossip)
        + encode_socket_addr_rlp_length(&addr.api)
        + encode_reth_peer_info_rlp_length(&addr.execution);
    payload_length + alloy_rlp::length_of_length(payload_length)
}

fn encode_reth_peer_info_rlp(info: &RethPeerInfo, out: &mut dyn bytes::BufMut) {
    let payload_length =
        encode_socket_addr_rlp_length(&info.peering_tcp_addr) + info.peer_id.0.as_slice().length();
    alloy_rlp::Header {
        list: true,
        payload_length,
    }
    .encode(out);
    encode_socket_addr_rlp(&info.peering_tcp_addr, out);
    info.peer_id.0.as_slice().encode(out);
}

fn encode_reth_peer_info_rlp_length(info: &RethPeerInfo) -> usize {
    let payload_length =
        encode_socket_addr_rlp_length(&info.peering_tcp_addr) + info.peer_id.0.as_slice().length();
    payload_length + alloy_rlp::length_of_length(payload_length)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, HandshakeRequest, HandshakeRequestV2, IrysSignature, NodeConfig, H256};
    use crate::{IrysAddress, U256};
    use serde_json;

    #[test]
    fn should_sign_and_verify_signature() {
        let mut version_request = HandshakeRequest::default();
        let testing_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(testing_config);
        let signer = config.irys_signer();

        signer.sign_p2p_handshake_v1(&mut version_request).unwrap();
        assert!(
            version_request.verify_signature(),
            "Signature should be valid"
        );

        version_request.signature = IrysSignature::default();
        assert!(
            !version_request.verify_signature(),
            "Signature should be invalid after reset"
        );
    }

    #[test]
    fn should_sign_and_verify_signature_v2() {
        let mut version_request = HandshakeRequestV2::default();
        let testing_config = NodeConfig::testing();
        let peer_id = crate::IrysPeerId::from([2_u8; 20]);
        let config = Config::new(testing_config, peer_id);
        let signer = config.irys_signer();

        signer.sign_p2p_handshake_v2(&mut version_request).unwrap();
        assert!(
            version_request.verify_signature(),
            "Signature should be valid"
        );

        version_request.signature = IrysSignature::default();
        assert!(
            !version_request.verify_signature(),
            "Signature should be invalid after reset"
        );
    }

    #[test]
    fn test_large_u64_serialization() {
        let large_value = u64::MAX;
        let node_info = NodeInfo {
            version: "1.0.0".to_string(),
            peer_count: 10,
            chain_id: large_value,
            height: large_value,
            block_hash: H256::zero(),
            block_index_height: large_value,
            block_index_hash: H256::zero(),
            pending_blocks: large_value,
            is_syncing: false,
            current_sync_height: 0,
            uptime_secs: 0,
            mining_address: IrysAddress::ZERO,
            cumulative_difficulty: U256::zero(),
        };

        let json = serde_json::to_string(&node_info).unwrap();

        // Verify all u64 fields are serialized as strings
        assert!(json.contains(&format!("\"chainId\":\"{}\"", large_value)));
        assert!(json.contains(&format!("\"height\":\"{}\"", large_value)));
        assert!(json.contains(&format!("\"blockIndexHeight\":\"{}\"", large_value)));
        assert!(json.contains(&format!("\"pendingBlocks\":\"{}\"", large_value)));

        // Verify no numeric serialization for large values
        assert!(!json.contains(&format!("\"chainId\":{}", large_value)));

        // Verify deserialization
        let deserialized: NodeInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.chain_id, large_value);
        assert_eq!(deserialized.height, large_value);
    }

    // Test JavaScript parsing would  work, this tries to simulate what would happen in a JavaScript environment
    #[test]
    fn test_javascript_max_safe_integer() {
        const JS_MAX_SAFE_INTEGER: u64 = (1_u64 << 53) - 1; // 2^53 - 1
        let above_safe_limit = JS_MAX_SAFE_INTEGER + 1;

        let node_info = NodeInfo {
            height: above_safe_limit,
            chain_id: above_safe_limit,
            ..Default::default()
        };

        let json = serde_json::to_string(&node_info).unwrap();

        // Should be string, not number
        assert!(json.contains(&format!("\"height\":\"{}\"", above_safe_limit)));
        assert!(json.contains(&format!("\"chainId\":\"{}\"", above_safe_limit)));

        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        if let Some(height_str) = parsed.get("height").and_then(|v| v.as_str()) {
            let parsed_height: u64 = height_str.parse().unwrap();
            assert_eq!(parsed_height, above_safe_limit);
        } else {
            panic!("height should be serialized as string");
        }
    }

    #[test]
    fn test_info_serde_roundtrip() -> eyre::Result<()> {
        let old_json = r#"{"version":"1.0.0","peerCount":10,"chainId":"12345","height":"67890","blockHash":"5TLJx8LqeDGxJ6b6R4JWfZFmPunoM9VgpGDVo9fHexKD","blockIndexHeight":"0","blockIndexHash":"5TLJx8LqeDGxJ6b6R4JWfZFmPunoM9VgpGDVo9fHexKD","pendingBlocks":"0","isSyncing":false,"currentSyncHeight":"0","uptimeSecs":"0","miningAddress":"11111111111111111111","cumulativeDifficulty":"123"}"#;
        // Test that we can still deserialize old numeric format for small values
        // TODO: remove this at some point?
        let node_info: NodeInfo = serde_json::from_str(old_json)?;
        assert_eq!(node_info.chain_id, 12345);
        assert_eq!(node_info.height, 67890);

        // this should ensure that we don't break U64 as string serialisation
        let reenc_node_info = serde_json::to_string(&node_info)?;
        assert_eq!(old_json, reenc_node_info.as_str());
        Ok(())
    }

    #[test]
    fn test_handshake_v2_rlp_encoding() {
        use alloy_rlp::{Decodable as _, Encodable as _};
        use semver::Version;

        // Create a test handshake request with all fields populated
        let handshake = HandshakeRequestV2 {
            version: Version::new(1, 2, 3),
            protocol_version: ProtocolVersion::V2,
            mining_address: IrysAddress::from([1_u8; 20]),
            peer_id: crate::IrysPeerId::from([2_u8; 20]),
            chain_id: 1270,
            address: crate::PeerAddress::default(),
            timestamp: 1234567890,
            user_agent: Some("test-agent".to_string()),
            consensus_config_hash: H256::zero(),
            signature: IrysSignature::default(),
        };

        // Test 1: Verify encoding produces valid RLP list
        let mut encoded = Vec::new();
        handshake.encode_for_signing(&mut encoded);

        let mut buf = &encoded[..];
        let header = alloy_rlp::Header::decode(&mut buf).expect("Should decode valid RLP header");
        assert!(header.list, "Encoded data should be an RLP list");
        assert!(
            header.payload_length > 0,
            "Payload length should be positive"
        );

        // Test 2: Verify length calculation matches actual payload
        let calculated_length = encode_version_rlp_length(&handshake.version)
            + encode_protocol_version_rlp_length(&handshake.protocol_version)
            + handshake.mining_address.length()
            + handshake.peer_id.length()
            + handshake.chain_id.length()
            + encode_peer_address_rlp_length(&handshake.address)
            + handshake.timestamp.length()
            + handshake
                .user_agent
                .as_ref()
                .map_or(1, alloy_rlp::Encodable::length)
            + handshake.consensus_config_hash.length();

        assert_eq!(
            calculated_length, header.payload_length,
            "Calculated length should match header payload length"
        );

        // Test 3: Verify None user_agent encodes as empty string
        let mut handshake_no_ua = handshake.clone();
        handshake_no_ua.user_agent = None;

        let mut encoded_no_ua = Vec::new();
        handshake_no_ua.encode_for_signing(&mut encoded_no_ua);

        // Verify empty string encoding appears in the output
        let empty_string_rlp = {
            let mut buf = Vec::new();
            "".encode(&mut buf);
            buf
        };
        assert_eq!(
            empty_string_rlp.len(),
            1,
            "Empty string should encode to 1 byte (0x80)"
        );
        assert_eq!(empty_string_rlp[0], 0x80, "Empty string should be 0x80");

        // Test 4: Verify Some("") and None produce identical encodings
        let mut handshake_empty_ua = handshake.clone();
        handshake_empty_ua.user_agent = Some(String::new());

        let mut encoded_empty_ua = Vec::new();
        handshake_empty_ua.encode_for_signing(&mut encoded_empty_ua);

        assert_eq!(
            encoded_empty_ua, encoded_no_ua,
            "Empty string and None should produce identical RLP encodings"
        );

        // Test 5: Verify field order by decoding individual components
        let mut buf = &encoded[..];
        let _header = alloy_rlp::Header::decode(&mut buf).unwrap();

        // Decode and verify each field in order
        let version_header = alloy_rlp::Header::decode(&mut buf).unwrap();
        assert!(version_header.list, "Version should be encoded as a list");

        let major = u64::decode(&mut buf).unwrap();
        let minor = u64::decode(&mut buf).unwrap();
        let patch = u64::decode(&mut buf).unwrap();
        assert_eq!((major, minor, patch), (1, 2, 3), "Version should be 1.2.3");

        let protocol_version = u32::decode(&mut buf).unwrap();
        assert_eq!(protocol_version, 2, "Protocol version should be 2");

        let mining_address = IrysAddress::decode(&mut buf).unwrap();
        assert_eq!(
            mining_address, handshake.mining_address,
            "Mining address should match"
        );

        let peer_id = crate::IrysPeerId::decode(&mut buf).unwrap();
        assert_eq!(peer_id, handshake.peer_id, "Peer ID should match");

        let chain_id = u64::decode(&mut buf).unwrap();
        assert_eq!(chain_id, 1270, "Chain ID should be 1270");

        // Skip peer address (complex structure)
        let _peer_address_header = alloy_rlp::Header::decode(&mut buf).unwrap();
        // We won't fully decode the peer address structure here, just skip past it

        // Test 6: Verify that changing any field changes the encoding
        let mut handshake_different = handshake.clone();
        handshake_different.chain_id = 1271;

        let mut encoded_different = Vec::new();
        handshake_different.encode_for_signing(&mut encoded_different);

        assert_ne!(
            encoded, encoded_different,
            "Different field values should produce different encodings"
        );
    }

    #[test]
    fn test_handshake_v2_rlp_signature_stability() {
        use semver::Version;

        // Verify that the signature hash is stable across multiple encodings
        let handshake = HandshakeRequestV2 {
            version: Version::new(1, 0, 0),
            protocol_version: ProtocolVersion::V2,
            mining_address: IrysAddress::from([42_u8; 20]),
            peer_id: crate::IrysPeerId::from([43_u8; 20]),
            chain_id: 1270,
            address: crate::PeerAddress::default(),
            timestamp: 1000000000,
            user_agent: Some("stable-test".to_string()),
            consensus_config_hash: H256::zero(),
            signature: IrysSignature::default(),
        };

        let hash1 = handshake.signature_hash();
        let hash2 = handshake.signature_hash();

        assert_eq!(hash1, hash2, "Signature hash should be deterministic");

        // Verify that the hash changes when a field changes
        let mut handshake_modified = handshake;
        handshake_modified.timestamp = 1000000001;

        let hash_modified = handshake_modified.signature_hash();
        assert_ne!(
            hash1, hash_modified,
            "Signature hash should change when fields change"
        );
    }

    #[test]
    fn test_handshake_v2_rlp_hex_verification() {
        use semver::Version;

        // Create a simple handshake with known values for manual hex verification
        let handshake = HandshakeRequestV2 {
            version: Version::new(1, 2, 3),
            protocol_version: ProtocolVersion::V2,
            mining_address: IrysAddress::ZERO,
            peer_id: crate::IrysPeerId::ZERO,
            chain_id: 100,
            address: crate::PeerAddress::default(),
            timestamp: 1000,
            user_agent: Some("test".to_string()),
            consensus_config_hash: H256::zero(),
            signature: IrysSignature::default(),
        };

        let mut encoded = Vec::new();
        handshake.encode_for_signing(&mut encoded);

        // Verify it's a valid RLP list that starts with list marker
        // Short list: 0xc0..=0xf6, Long list: 0xf7..=0xff
        // Both ranges combine to >= 0xc0 (since u8 max is 0xff)
        assert!(encoded[0] >= 0xc0, "Should start with list marker");

        // Verify basic RLP structure by checking we can decode the header
        let mut buf = &encoded[..];
        let header = alloy_rlp::Header::decode(&mut buf).expect("Should decode valid RLP header");

        assert!(header.list, "Should be a list");
        assert_eq!(
            header.payload_length + alloy_rlp::length_of_length(header.payload_length),
            encoded.len(),
            "Total encoded length should match header + payload"
        );

        // Verify protocol version appears in the encoding (should be 0x02 for V2)
        // This is a simplified check - the actual position depends on Version encoding
        assert!(
            encoded.contains(&0x02),
            "Encoded data should contain protocol version byte"
        );

        // Verify chain_id (100 = 0x64) appears in the encoding
        assert!(
            encoded.contains(&0x64),
            "Encoded data should contain chain_id byte"
        );
    }

    #[test]
    fn test_v2_signature_includes_consensus_hash() {
        let req_a = HandshakeRequestV2 {
            consensus_config_hash: H256::from([0xAA; 32]),
            ..HandshakeRequestV2::default()
        };
        let mut req_b = HandshakeRequestV2 {
            consensus_config_hash: H256::from([0xBB; 32]),
            ..HandshakeRequestV2::default()
        };
        // Force identical timestamps so only the hash field differs
        req_b.timestamp = req_a.timestamp;

        assert_ne!(
            req_a.signature_hash(),
            req_b.signature_hash(),
            "different consensus_config_hash values should produce different signature hashes"
        );
    }

    /// A helper to generate a new raw JSON for the version endpoint test.
    /// Run `cargo test -p irys-types -- generate_raw_handshake_json --nocapture --ignored`
    /// to produce the raw JSON needed by the peer_discovery integration test.
    #[test]
    #[ignore]
    fn generate_raw_handshake_json() {
        use crate::irys::IrysSigner;
        use crate::ConsensusConfig;

        let mut config = ConsensusConfig::testing();

        // NodeConfig::testing() sets these genesis addresses
        let testing_key = k256::ecdsa::SigningKey::from_slice(
            &hex::decode(b"db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0")
                .expect("valid hex"),
        )
        .expect("valid key");
        let testing_signer = IrysSigner {
            signer: testing_key,
            chain_id: 0,
            chunk_size: 0,
        };
        config.genesis.miner_address = testing_signer.address();
        config.genesis.reward_address = testing_signer.address();

        // Match the consensus config from the test
        config.chunk_size = 32;
        config.num_chunks_in_partition = 10;
        config.num_chunks_in_recall_range = 2;
        config.num_partitions_per_slot = 1;
        config.entropy_packing_iterations = 1_000;
        config.block_migration_depth = 1;

        // Hardcoded key matching the test node's genesis signer
        let genesis_key_bytes: [u8; 32] = [
            0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55,
            0x66, 0x77, 0x88, 0x99,
        ];
        let genesis_signing_key =
            k256::ecdsa::SigningKey::from_bytes((&genesis_key_bytes).into()).unwrap();
        let genesis_signer = IrysSigner {
            signer: genesis_signing_key,
            chain_id: config.chain_id,
            chunk_size: config.chunk_size,
        };

        // Add genesis account to match test
        use alloy_core::primitives::U256;
        use alloy_genesis::GenesisAccount;
        let genesis_account = GenesisAccount {
            balance: U256::from(690000000000000000_u128),
            ..Default::default()
        };
        config
            .reth
            .alloc
            .insert(genesis_signer.address().into(), genesis_account);

        // Fixed private key for the peer handshake (different from genesis)
        let key_bytes: [u8; 32] = [
            0x1f, 0x2e, 0x3d, 0x4c, 0x5b, 0x6a, 0x79, 0x88, 0x97, 0xa6, 0xb5, 0xc4, 0xd3, 0xe2,
            0xf1, 0x00, 0x0f, 0x1e, 0x2d, 0x3c, 0x4b, 0x5a, 0x69, 0x78, 0x87, 0x96, 0xa5, 0xb4,
            0xc3, 0xd2, 0xe1, 0xf0,
        ];
        let signing_key = k256::ecdsa::SigningKey::from_bytes((&key_bytes).into()).unwrap();
        let signer = IrysSigner {
            signer: signing_key,
            chain_id: config.chain_id,
            chunk_size: config.chunk_size,
        };

        let mining_addr = signer.address();

        eprintln!("mining_address = {}", mining_addr);
        eprintln!("consensus_config_hash = {}", config.keccak256_hash());

        let peer_id_addr: IrysAddress = "4JaNfJ1tQ2TCLREq6opq6pWGmCJW".parse().unwrap();
        let mut req = HandshakeRequestV2 {
            version: semver::Version::new(0, 1, 0),
            protocol_version: ProtocolVersion::V2,
            mining_address: mining_addr,
            peer_id: peer_id_addr.into(),
            chain_id: 1270,
            address: crate::PeerAddress {
                gossip: "127.0.0.2:8080".parse().unwrap(),
                api: "127.0.0.2:8081".parse().unwrap(),
                execution: crate::RethPeerInfo {
                    peering_tcp_addr: "127.0.0.2:8082".parse().unwrap(),
                    peer_id: "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".parse().unwrap(),
                },
            },
            timestamp: 0,
            user_agent: Some("miner2/0.1.0 (macos/aarch64)".to_string()),
            consensus_config_hash: config.keccak256_hash(),
            signature: IrysSignature::default(),
        };

        signer.sign_p2p_handshake_v2(&mut req).unwrap();

        eprintln!("=== Signed HandshakeRequestV2 JSON ===");
        eprintln!("{}", serde_json::to_string_pretty(&req).unwrap());
    }
}
