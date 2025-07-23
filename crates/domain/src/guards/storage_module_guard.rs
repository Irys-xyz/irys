use crate::StorageModule;
use std::sync::{Arc, RwLock, RwLockReadGuard};

/// Wraps the internal Arc<`RwLock`<>> to make the reference readonly
#[derive(Debug, Clone)]
pub struct StorageModulesReadGuard {
    storage_module_data: Arc<RwLock<Vec<Arc<StorageModule>>>>,
}

impl StorageModulesReadGuard {
    /// Creates a new `ReadGuard` for StorageModules list
    pub const fn new(storage_module_data: Arc<RwLock<Vec<Arc<StorageModule>>>>) -> Self {
        Self {
            storage_module_data,
        }
    }

    /// Accessor method to get a read guard for the StorageModules list
    pub fn read(&self) -> RwLockReadGuard<'_, Vec<Arc<StorageModule>>> {
        self.storage_module_data.read().unwrap()
    }
}
