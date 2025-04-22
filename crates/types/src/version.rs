use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use crate::{Arbitrary, RethPeerInfo};
use alloy_primitives::Address;
use semver::Version;
use serde::{Deserialize, Serialize};
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
        }
    }
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialOrd, Ord, Hash, Eq, PartialEq, Arbitrary,
)]
pub struct PeerAddress {
    pub gossip: SocketAddr,
    pub api: SocketAddr,
    pub execution: RethPeerInfo,
    pub mining_address: Address,
}

impl Default for PeerAddress {
    fn default() -> Self {
        Self {
            gossip: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
            api: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8081),
            execution: RethPeerInfo::default(),
            mining_address: Address::ZERO,
        }
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
