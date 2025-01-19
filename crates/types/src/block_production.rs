use crate::{partition::PartitionHash, ChunkDataPath, H256List, IrysBlockHeader, TxPath, H256};
use actix::Message;
use alloy_primitives::Address;
use alloy_rpc_types_engine::ExecutionPayloadEnvelopeV1Irys;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Seed(pub H256);

impl Seed {
    pub fn into_inner(self) -> H256 {
        self.0
    }
}

#[derive(Message, Debug, Clone, PartialEq, Default)]
#[rtype(result = "Option<(Arc<IrysBlockHeader>, ExecutionPayloadEnvelopeV1Irys)>")]
pub struct SolutionContext {
    pub partition_hash: PartitionHash,
    pub chunk_offset: u32,
    pub recall_chunk_index: u32,
    pub mining_address: Address,
    pub tx_path: Option<TxPath>, // capacity partitions have no tx_path nor data_path
    pub data_path: Option<ChunkDataPath>,
    pub chunk: Vec<u8>,
    pub vdf_step: u64,
    pub checkpoints: H256List,
    pub seed: Seed,
    pub solution_hash: H256,
}
#[derive(Debug, Clone)]
pub struct Partition {
    pub id: PartitionId,
    pub mining_address: Address,
}

pub type PartitionId = u64;

impl Default for Partition {
    fn default() -> Self {
        Self {
            id: 0,
            mining_address: Address::random(),
        }
    }
}

impl Partition {
    pub fn random_with_id(id: u64) -> Self {
        Self {
            id,
            mining_address: Address::random(),
        }
    }
}
