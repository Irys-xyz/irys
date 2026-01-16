mod client_pool;
mod config;
mod constants;
mod errors;
mod guard;
pub mod services;
mod strategies;
pub mod types;

pub use config::PackingConfig;
pub(crate) use constants::REMOTE_STREAM_BUFFER_MULTIPLIER;
pub(crate) use errors::PackingError;
pub(crate) use services::packing::PackingResult;
pub use services::packing::{log_packing_progress, sync_with_warning};
pub use types::{
    PackingHandle, PackingIdleWaiter, PackingInternals, PackingQueues, PackingRequest,
    UnpackingHandle, UnpackingInternals, UnpackingPriority, UnpackingRequest,
};

use irys_types::Config;
use services::packing::InternalPackingService;
use services::unpacking::InternalUnpackingService;
use std::sync::Arc;

/// Main packing service that orchestrates all packing operations
#[derive(Clone)]
pub struct PackingService {
    pub internal_packing_service: InternalPackingService,
    pub internal_unpacking_service: InternalUnpackingService,
}

impl PackingService {
    pub fn new(config: Arc<Config>) -> Self {
        let internal_packing_service = InternalPackingService::new(config.clone());
        let internal_unpacking_service = InternalUnpackingService::new(config);
        Self {
            internal_packing_service,
            internal_unpacking_service,
        }
    }
}
