use actix::Message;

use crate::H256;

#[derive(Message)]
#[rtype(result = "()")]
pub struct SolutionContext {
    pub partition_id: u64,
    pub chunk_index: u64,
    pub mining_address: H256,
}

pub struct Partition {
    pub id: u64,
    pub mining_addr: H256,
}

impl Default for Partition {
    fn default() -> Self {
        Self {
            id: 0,
            mining_addr: H256::random(),
        }
    }
}
