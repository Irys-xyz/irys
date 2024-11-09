use crate::IrysBlockHeader;
use actix::Message;
use alloy_primitives::Address;
use alloy_rpc_types_engine::ExecutionPayloadEnvelopeV1Irys;

#[derive(Message)]
#[rtype(result = "Option<(IrysBlockHeader, ExecutionPayloadEnvelopeV1Irys)>")]
pub struct SolutionContext {
    pub partition_id: u64,
    pub chunk_index: u32,
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
