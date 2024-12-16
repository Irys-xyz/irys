use crate::{partition::PartitionHash, ChunkDataPath, IrysBlockHeader, TxPath};
use actix::Message;
use alloy_primitives::Address;
use alloy_rpc_types_engine::ExecutionPayloadEnvelopeV1Irys;
use std::sync::Arc;

#[derive(Message, Debug, PartialEq)]
#[rtype(result = "Option<(Arc<IrysBlockHeader>, ExecutionPayloadEnvelopeV1Irys)>")]
pub struct SolutionContext {
    pub partition_hash: PartitionHash,
    pub chunk_offset: u32,
    pub mining_address: Address,
    pub tx_path: Option<TxPath>, // capacity partitions have no tx_path nor data_path
    pub data_path: Option<ChunkDataPath>,
    pub chunk: Vec<u8>,
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
