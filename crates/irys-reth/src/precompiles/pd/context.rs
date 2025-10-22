//! Shared context for PD precompile execution.

use alloy_eips::eip2930::AccessListItem;
use irys_primitives::chunk_provider::RethChunkProvider;
use parking_lot::RwLock;
use std::sync::Arc;

#[derive(Clone)]
pub struct PdContext {
    access_list: Arc<RwLock<Vec<AccessListItem>>>,
    chunk_provider: Arc<dyn RethChunkProvider>,
}

impl PdContext {
    pub fn new(chunk_provider: Arc<dyn RethChunkProvider>) -> Self {
        Self {
            access_list: Arc::new(RwLock::new(Vec::new())),
            chunk_provider,
        }
    }

    pub fn chunk_provider(&self) -> &dyn RethChunkProvider {
        &*self.chunk_provider
    }

    pub fn update_access_list(&self, access_list: Vec<AccessListItem>) {
        *self.access_list.write() = access_list;
    }

    pub fn read_access_list(&self) -> Vec<AccessListItem> {
        self.access_list.read().clone()
    }
}

impl std::fmt::Debug for PdContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PdContext")
            .field("access_list", &self.access_list)
            .field("chunk_provider", &self.chunk_provider)
            .finish()
    }
}
