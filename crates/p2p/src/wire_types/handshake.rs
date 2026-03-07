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

super::impl_mirror_from!(irys_types::HandshakeRequestV1 => HandshakeRequestV1 {
    version, protocol_version, mining_address, chain_id, address, timestamp, user_agent, signature,
});

super::impl_mirror_from!(irys_types::HandshakeRequestV2 => HandshakeRequestV2 {
    version, protocol_version, mining_address, peer_id, chain_id, address, timestamp,
    user_agent, consensus_config_hash, signature,
});

super::impl_mirror_from!(irys_types::HandshakeResponseV1 => HandshakeResponseV1 {
    version, protocol_version, peers, timestamp, message,
});

super::impl_mirror_from!(irys_types::HandshakeResponseV2 => HandshakeResponseV2 {
    version, protocol_version, peers, timestamp, message, consensus_config_hash,
});
