use crate::{partition::PartHash, IrysBlockHeader, H256};
use actix::Message;
use alloy_primitives::Address;
use alloy_rpc_types_engine::ExecutionPayloadEnvelopeV1Irys;
use std::sync::Arc;

#[derive(Message, Debug)]
#[rtype(result = "Option<(Arc<IrysBlockHeader>, ExecutionPayloadEnvelopeV1Irys)>")]
pub struct SolutionContext {
    pub partition_hash: PartHash,
    pub chunk_offset: u32,
    pub mining_address: Address,
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
