//! Shared context for PD precompile execution.

use alloy_eips::eip2930::AccessListItem;
use irys_types::chunk_provider::RethChunkProvider;
use parking_lot::RwLock;
use std::sync::Arc;

pub struct PdContext {
    // Arc<RwLock> allows sharing within a single EVM instance
    // Custom Clone creates NEW Arc (not Arc::clone) for each EVM instance
    access_list: Arc<RwLock<Vec<AccessListItem>>>,
    chunk_provider: Arc<dyn RethChunkProvider>,
}

// Default Clone uses Arc::clone to share the same access_list storage
// This is needed so precompile and EVM share the same Arc<RwLock>
impl Clone for PdContext {
    fn clone(&self) -> Self {
        Self {
            access_list: Arc::clone(&self.access_list),
            chunk_provider: Arc::clone(&self.chunk_provider),
        }
    }
}

impl PdContext {
    pub fn new(chunk_provider: Arc<dyn RethChunkProvider>) -> Self {
        Self {
            access_list: Arc::new(RwLock::new(Vec::new())),
            chunk_provider,
        }
    }

    /// Creates an independent clone with a NEW Arc<RwLock> for use in a new EVM instance.
    /// This ensures each EVM has its own access list storage (no cross-EVM contamination).
    #[must_use = "cloned context should be used for new EVM instance"]
    pub fn clone_for_new_evm(&self) -> Self {
        Self {
            access_list: Arc::new(RwLock::new(self.access_list.read().clone())),
            chunk_provider: Arc::clone(&self.chunk_provider),
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
