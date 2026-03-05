use irys_types::{IrysAddress, IrysPeerId, IrysSignature, PeerAddress, ProtocolVersion, H256};
use semver::Version;
use serde::{Deserialize, Serialize};

/// Sovereign wire type for HandshakeRequestV1.
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

/// Sovereign wire type for HandshakeRequestV2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeRequestV2 {
    pub version: Version,
    pub protocol_version: ProtocolVersion,
    pub mining_address: IrysAddress,
    pub peer_id: IrysPeerId,
    pub chain_id: u64,
    pub address: PeerAddress,
    pub timestamp: u64,
    pub user_agent: Option<String>,
    pub consensus_config_hash: H256,
    pub signature: IrysSignature,
}

/// Sovereign wire type for HandshakeResponseV1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeResponseV1 {
    pub version: Version,
    pub protocol_version: ProtocolVersion,
    pub peers: Vec<PeerAddress>,
    pub timestamp: u64,
    pub message: Option<String>,
}

/// Sovereign wire type for HandshakeResponseV2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeResponseV2 {
    pub version: Version,
    pub protocol_version: ProtocolVersion,
    pub peers: Vec<PeerAddress>,
    pub timestamp: u64,
    pub message: Option<String>,
    pub consensus_config_hash: H256,
}

// -- Conversions --

impl From<&irys_types::HandshakeRequestV1> for HandshakeRequestV1 {
    fn from(h: &irys_types::HandshakeRequestV1) -> Self {
        Self {
            version: h.version.clone(),
            protocol_version: h.protocol_version,
            mining_address: h.mining_address,
            chain_id: h.chain_id,
            address: h.address,
            timestamp: h.timestamp,
            user_agent: h.user_agent.clone(),
            signature: h.signature,
        }
    }
}

impl From<HandshakeRequestV1> for irys_types::HandshakeRequestV1 {
    fn from(h: HandshakeRequestV1) -> Self {
        Self {
            version: h.version,
            protocol_version: h.protocol_version,
            mining_address: h.mining_address,
            chain_id: h.chain_id,
            address: h.address,
            timestamp: h.timestamp,
            user_agent: h.user_agent,
            signature: h.signature,
        }
    }
}

impl From<&irys_types::HandshakeRequestV2> for HandshakeRequestV2 {
    fn from(h: &irys_types::HandshakeRequestV2) -> Self {
        Self {
            version: h.version.clone(),
            protocol_version: h.protocol_version,
            mining_address: h.mining_address,
            peer_id: h.peer_id,
            chain_id: h.chain_id,
            address: h.address,
            timestamp: h.timestamp,
            user_agent: h.user_agent.clone(),
            consensus_config_hash: h.consensus_config_hash,
            signature: h.signature,
        }
    }
}

impl From<HandshakeRequestV2> for irys_types::HandshakeRequestV2 {
    fn from(h: HandshakeRequestV2) -> Self {
        Self {
            version: h.version,
            protocol_version: h.protocol_version,
            mining_address: h.mining_address,
            peer_id: h.peer_id,
            chain_id: h.chain_id,
            address: h.address,
            timestamp: h.timestamp,
            user_agent: h.user_agent,
            consensus_config_hash: h.consensus_config_hash,
            signature: h.signature,
        }
    }
}

impl From<&irys_types::HandshakeResponseV1> for HandshakeResponseV1 {
    fn from(h: &irys_types::HandshakeResponseV1) -> Self {
        Self {
            version: h.version.clone(),
            protocol_version: h.protocol_version,
            peers: h.peers.clone(),
            timestamp: h.timestamp,
            message: h.message.clone(),
        }
    }
}

impl From<HandshakeResponseV1> for irys_types::HandshakeResponseV1 {
    fn from(h: HandshakeResponseV1) -> Self {
        Self {
            version: h.version,
            protocol_version: h.protocol_version,
            peers: h.peers,
            timestamp: h.timestamp,
            message: h.message,
        }
    }
}

impl From<&irys_types::HandshakeResponseV2> for HandshakeResponseV2 {
    fn from(h: &irys_types::HandshakeResponseV2) -> Self {
        Self {
            version: h.version.clone(),
            protocol_version: h.protocol_version,
            peers: h.peers.clone(),
            timestamp: h.timestamp,
            message: h.message.clone(),
            consensus_config_hash: h.consensus_config_hash,
        }
    }
}

impl From<HandshakeResponseV2> for irys_types::HandshakeResponseV2 {
    fn from(h: HandshakeResponseV2) -> Self {
        Self {
            version: h.version,
            protocol_version: h.protocol_version,
            peers: h.peers,
            timestamp: h.timestamp,
            message: h.message,
            consensus_config_hash: h.consensus_config_hash,
        }
    }
}
