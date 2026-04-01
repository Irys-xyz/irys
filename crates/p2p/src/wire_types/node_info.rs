use irys_types::{H256, IrysAddress, U256};
use serde::{Deserialize, Serialize};

/// V1 wire type for [`irys_types::version::NodeInfo`].
///
/// `peer_count` is serialized as a bare integer for backward compatibility
/// with V1 gossip peers. All other numeric fields use string encoding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfoV1 {
    pub version: String,
    pub peer_count: u64,
    #[serde(with = "irys_types::string_u64")]
    pub chain_id: u64,
    #[serde(with = "irys_types::string_u64")]
    pub height: u64,
    pub block_hash: H256,
    #[serde(with = "irys_types::string_u64")]
    pub block_index_height: u64,
    pub block_index_hash: H256,
    #[serde(with = "irys_types::string_u64")]
    pub pending_blocks: u64,
    pub is_syncing: bool,
    #[serde(with = "irys_types::string_u64")]
    pub current_sync_height: u64,
    #[serde(with = "irys_types::string_u64")]
    pub uptime_secs: u64,
    pub mining_address: IrysAddress,
    pub cumulative_difficulty: U256,
}

super::impl_mirror_from!(irys_types::version::NodeInfo => NodeInfoV1 {
    version, peer_count, chain_id, height, block_hash,
    block_index_height, block_index_hash, pending_blocks,
    is_syncing, current_sync_height, uptime_secs,
    mining_address, cumulative_difficulty,
});

/// V2 wire type for [`irys_types::version::NodeInfo`].
///
/// All numeric fields — including `peer_count` — are serialized as strings
/// for consistency and JavaScript `Number` safety.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfoV2 {
    pub version: String,
    #[serde(with = "irys_types::string_u64")]
    pub peer_count: u64,
    #[serde(with = "irys_types::string_u64")]
    pub chain_id: u64,
    #[serde(with = "irys_types::string_u64")]
    pub height: u64,
    pub block_hash: H256,
    #[serde(with = "irys_types::string_u64")]
    pub block_index_height: u64,
    pub block_index_hash: H256,
    #[serde(with = "irys_types::string_u64")]
    pub pending_blocks: u64,
    pub is_syncing: bool,
    #[serde(with = "irys_types::string_u64")]
    pub current_sync_height: u64,
    #[serde(with = "irys_types::string_u64")]
    pub uptime_secs: u64,
    pub mining_address: IrysAddress,
    pub cumulative_difficulty: U256,
}

super::impl_mirror_from!(irys_types::version::NodeInfo => NodeInfoV2 {
    version, peer_count, chain_id, height, block_hash,
    block_index_height, block_index_hash, pending_blocks,
    is_syncing, current_sync_height, uptime_secs,
    mining_address, cumulative_difficulty,
});
