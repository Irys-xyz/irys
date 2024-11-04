use irys_types::H256;

use crate::provider::StorageProvider;

pub struct Partition {
    pub id: u64,
    pub mining_addr: H256,
    pub storage_provider: StorageProvider,
}

impl Default for Partition {
    fn default() -> Self {
        Self {
            id: 0,
            mining_addr: H256::random(),
            ..Default::default()
        }
    }
}
