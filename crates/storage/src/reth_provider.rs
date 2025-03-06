use std::{
    fmt::Debug,
    sync::{Arc, RwLock},
};

use reth_db::DatabaseEnv;

use crate::ChunkProvider;

#[derive(Debug, Clone)]
pub struct IrysRethProviderInner {
    pub chunk_provider: Arc<ChunkProvider>,
}

pub type IrysRethProvider = Arc<RwLock<Option<IrysRethProviderInner>>>;

pub fn create_provider() -> IrysRethProvider {
    Arc::new(RwLock::new(None))
}

pub fn cleanup_provider(provider: &IrysRethProvider) {
    if let Ok(mut inner) = provider.write() {
        // Replace the inner value with None to drop references
        *inner = None;
    }
}
