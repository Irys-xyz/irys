use irys_types::{IrysAddress, H256, U256};
use serde::{Deserialize, Serialize};

/// Wire type for [`irys_types::version::NodeInfo`].
///
/// Mirrors the canonical type's JSON layout (`camelCase` keys, numeric
/// fields serialized as strings) so that the gossip wire format is
/// decoupled from upstream serde-attribute changes in `irys_types`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    pub version: String,
    pub peer_count: usize,
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
    #[serde(with = "irys_types::serialization::string_usize")]
    pub current_sync_height: usize,
    #[serde(with = "irys_types::string_u64")]
    pub uptime_secs: u64,
    pub mining_address: IrysAddress,
    pub cumulative_difficulty: U256,
}

super::impl_mirror_from!(irys_types::version::NodeInfo => NodeInfo {
    version, peer_count, chain_id, height, block_hash,
    block_index_height, block_index_hash, pending_blocks,
    is_syncing, current_sync_height, uptime_secs,
    mining_address, cumulative_difficulty,
});
