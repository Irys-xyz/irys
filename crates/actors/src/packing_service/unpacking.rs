use irys_packing::unpack;
use irys_types::{Config, UnpackedChunk};
use rayon::prelude::*;
use std::any::Any;
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;

use super::errors::UnpackingError;
use super::types::UnpackingRequest;

/// CPU-based unpacking using rayon thread pool
pub struct UnpackingService {
    config: Arc<Config>,
    thread_pool: Arc<rayon::ThreadPool>,
    num_threads: usize,
    unpacking_timeout: Duration,
}

/// Default timeout for batch unpacking operations
pub const DEFAULT_UNPACKING_TIMEOUT: Duration = Duration::from_secs(30);

impl UnpackingService {
    pub fn new(config: Arc<Config>, num_threads: usize) -> eyre::Result<Self> {
        let thread_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .thread_name(|idx| format!("unpack-cpu-{}", idx))
            .build()
            .map_err(|e| eyre::eyre!("Failed to create unpacking thread pool: {}", e))?;

        Ok(Self {
            config,
            thread_pool: Arc::new(thread_pool),
            num_threads,
            unpacking_timeout: DEFAULT_UNPACKING_TIMEOUT,
        })
    }

    /// Extract panic message from panic payload
    fn extract_panic_message(panic_err: &Box<dyn Any + Send>) -> String {
        if let Some(s) = panic_err.downcast_ref::<String>() {
            s.clone()
        } else if let Some(s) = panic_err.downcast_ref::<&str>() {
            (*s).to_string()
        } else {
            "Unknown panic during unpacking".to_string()
        }
    }

    /// Handle unpacking result with proper error handling and logging
    fn handle_unpack_result(
        result: Result<UnpackedChunk, Box<dyn Any + Send>>,
        elapsed: Duration,
    ) -> Result<UnpackedChunk, UnpackingError> {
        match result {
            Ok(unpacked_chunk) => {
                debug!(
                    target: "irys::unpacking::done",
                    "Unpacked chunk in {:?}",
                    elapsed
                );
                Ok(unpacked_chunk)
            }
            Err(panic_err) => {
                let msg = Self::extract_panic_message(&panic_err);
                tracing::error!(
                    target: "irys::unpacking::error",
                    "Unpacking failed: {}",
                    msg
                );
                Err(UnpackingError::UnpackingFailed(msg))
            }
        }
    }

    /// Process a single unpacking request
    fn process_single_request(
        request: UnpackingRequest,
        entropy_iterations: u32,
        chunk_size: usize,
        chain_id: u64,
    ) -> Result<(), UnpackingError> {
        let start = std::time::Instant::now();

        let result = std::panic::catch_unwind(|| {
            unpack(
                &request.packed_chunk,
                entropy_iterations,
                chunk_size,
                chain_id,
            )
        });

        let elapsed = start.elapsed();
        let response = Self::handle_unpack_result(result, elapsed);

        request
            .response_tx
            .send(response)
            .map_err(|_| UnpackingError::ChannelClosed)
    }
    /// Execute batch synchronously on thread pool
    fn execute_batch_sync(
        requests: Vec<UnpackingRequest>,
        num_threads: usize,
        entropy_iterations: u32,
        chunk_size: usize,
        chain_id: u64,
    ) {
        let batch_size = requests.len();

        if batch_size <= num_threads {
            debug!(
                target: "irys::unpacking",
                "Using sequential unpacking for small batch (size: {} <= threads: {})",
                batch_size,
                num_threads
            );

            for request in requests {
                if let Err(e) =
                    Self::process_single_request(request, entropy_iterations, chunk_size, chain_id)
                {
                    tracing::warn!(
                        target: "irys::unpacking::error",
                        "Failed to send unpacking response: {}",
                        e
                    );
                }
            }
        } else {
            debug!(
                target: "irys::unpacking",
                "Using parallel unpacking for large batch (size: {} > threads: {})",
                batch_size,
                num_threads
            );

            requests.into_par_iter().for_each(|request| {
                if let Err(e) =
                    Self::process_single_request(request, entropy_iterations, chunk_size, chain_id)
                {
                    tracing::warn!(
                        target: "irys::unpacking::error",
                        "Failed to send unpacking response: {}",
                        e
                    );
                }
            });
        }
    }

    /// For small batches (batch_size <= num_threads), unpacks sequentially to avoid
    /// parallel processing overhead. For larger batches, uses rayon for parallel processing.
    pub async fn unpack(&self, requests: Vec<UnpackingRequest>) {
        if requests.is_empty() {
            return;
        }

        let batch_size = requests.len();
        let chunk_size = self.config.consensus.chunk_size as usize;
        let entropy_iterations = self.config.consensus.entropy_packing_iterations;
        let chain_id = self.config.consensus.chain_id;
        let num_threads = self.num_threads;

        let mode = if batch_size <= num_threads {
            "sequential"
        } else {
            "parallel"
        };

        debug!(
            target: "irys::unpacking::metrics",
            batch_size = %batch_size,
            num_threads = %num_threads,
            mode = %mode,
            "Processing unpacking batch"
        );

        let thread_pool = self.thread_pool.clone();
        let batch_timeout = self.unpacking_timeout;

        match tokio::time::timeout(
            batch_timeout,
            tokio::task::spawn_blocking(move || {
                thread_pool.install(|| {
                    Self::execute_batch_sync(
                        requests,
                        num_threads,
                        entropy_iterations,
                        chunk_size,
                        chain_id,
                    )
                })
            }),
        )
        .await
        {
            Ok(Ok(_)) => {
                debug!(
                    target: "irys::unpacking",
                    "Completed unpacking batch of {} requests",
                    batch_size
                );
            }
            Ok(Err(e)) => {
                tracing::error!(
                    target: "irys::unpacking::error",
                    "Unpacking task failed to complete: {} (batch size: {})",
                    e,
                    batch_size
                );
            }
            Err(_elapsed) => {
                tracing::error!(
                    target: "irys::unpacking::error",
                    "Unpacking batch timed out after {:?} (batch size: {}, mode: {})",
                    batch_timeout,
                    batch_size,
                    mode
                );
            }
        }
    }
}
