use crate::{
    decode_address, encode_address, serialization::string_u64, Arbitrary, IrysSignature,
    RethPeerInfo, H256,
};
use alloy_primitives::{keccak256, Address};
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
    Accepted(AcceptedResponse),
    #[serde(rename = "rejected")]
    Rejected(RejectedResponse),
}

// Explicit integer protocol versions
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u32)]
pub enum ProtocolVersion {
    V1 = 1,
    // V2 = 2,
    // V3 = 3,
}

impl Default for ProtocolVersion {
    fn default() -> Self {
        Self::V1
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
pub struct VersionRequest {
    pub version: Version,
    pub protocol_version: ProtocolVersion,
    pub mining_address: Address,
    pub chain_id: u64,
    pub address: PeerAddress,
    pub timestamp: u64,
    pub user_agent: Option<String>,
    pub signature: IrysSignature,
}

impl Default for VersionRequest {
    fn default() -> Self {
        Self {
            version: Version::new(0, 1, 0), // Default to 0.1.0
            mining_address: Address::ZERO,
            protocol_version: ProtocolVersion::default(),
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

impl VersionRequest {
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

/// Example serialized JSON AcceptedResponse:
/// ```json
/// {
///   "status": "accepted",         // comes from PeerResponse Enum
///   "version": "1.2.0",           // semver formatted
///   "protocol_version": "2",      // or however ProtocolVersion is configured to serialize
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
pub struct AcceptedResponse {
    pub version: Version,
    pub protocol_version: ProtocolVersion,
    // pub features: Vec<Feature>,  // perhaps something like "features": ["DHT", "NAT"], in the future
    pub peers: Vec<PeerAddress>,
    pub timestamp: u64,
    pub message: Option<String>,
}

impl Default for AcceptedResponse {
    fn default() -> Self {
        Self {
            version: Version::new(0, 1, 0), // Default to 0.1.0
            protocol_version: ProtocolVersion::default(),
            peers: Vec::new(),
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            message: None,
        }
    }
}

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
    pub current_sync_height: usize,
}

#[cfg(test)]
mod tests {
    use super::NodeInfo;
    use crate::{Config, IrysSignature, NodeConfig, VersionRequest, H256};
    use serde_json;

    #[test]
    fn should_sign_and_verify_signature() {
        let mut version_request = VersionRequest::default();
        let testing_config = NodeConfig::testing();
        let config = Config::new(testing_config);
        let signer = config.irys_signer();

        signer.sign_p2p_handshake(&mut version_request).unwrap();
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
    fn test_backwards_compatibility() {
        // Test that we can still deserialize old numeric format for small values
        let old_json = r#"{"version":"1.0.0","peerCount":10,"chainId":"12345","height":"67890","blockHash":"5TLJx8LqeDGxJ6b6R4JWfZFmPunoM9VgpGDVo9fHexKD","blockIndexHeight":"0","blockIndexHash":"5TLJx8LqeDGxJ6b6R4JWfZFmPunoM9VgpGDVo9fHexKD","pendingBlocks":"0","isSyncing":false,"currentSyncHeight":0}"#;

        let result: Result<NodeInfo, _> = serde_json::from_str(old_json);
        assert!(result.is_ok());

        let node_info = result.unwrap();
        assert_eq!(node_info.chain_id, 12345);
        assert_eq!(node_info.height, 67890);
    }
}
