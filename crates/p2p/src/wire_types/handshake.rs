use irys_types::{IrysAddress, IrysPeerId, IrysSignature, ProtocolVersion, H256};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

/// Wire type for [`irys_types::RethPeerInfo`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RethPeerInfo {
    pub peering_tcp_addr: SocketAddr,
    #[serde(default)]
    pub peer_id: reth::transaction_pool::PeerId,
}

/// Wire type for [`irys_types::PeerAddress`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerAddress {
    pub gossip: SocketAddr,
    pub api: SocketAddr,
    pub execution: RethPeerInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandshakeResponseV1 {
    pub version: Version,
    pub protocol_version: ProtocolVersion,
    pub peers: Vec<PeerAddress>,
    pub timestamp: u64,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandshakeResponseV2 {
    pub version: Version,
    pub protocol_version: ProtocolVersion,
    pub peers: Vec<PeerAddress>,
    pub timestamp: u64,
    pub message: Option<String>,
    pub consensus_config_hash: H256,
}

super::impl_mirror_from!(irys_types::RethPeerInfo => RethPeerInfo {
    peering_tcp_addr, peer_id,
});

super::impl_mirror_from!(irys_types::PeerAddress => PeerAddress {
    gossip, api,
} convert { execution });

super::impl_mirror_from!(irys_types::HandshakeRequestV1 => HandshakeRequestV1 {
    version, protocol_version, mining_address, chain_id, timestamp, user_agent, signature,
} convert { address });

super::impl_mirror_from!(irys_types::HandshakeRequestV2 => HandshakeRequestV2 {
    version, protocol_version, mining_address, peer_id, chain_id, timestamp,
    user_agent, consensus_config_hash, signature,
} convert { address });

super::impl_mirror_from!(irys_types::HandshakeResponseV1 => HandshakeResponseV1 {
    version, protocol_version, timestamp, message,
} convert_iter { peers });

super::impl_mirror_from!(irys_types::HandshakeResponseV2 => HandshakeResponseV2 {
    version, protocol_version, timestamp, message, consensus_config_hash,
} convert_iter { peers });
