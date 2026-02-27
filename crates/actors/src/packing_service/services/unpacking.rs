//! Unpacking service for decompressing entropy chunks
//!
//! - Unpack chunks using CPU thread pool (Rayon)
//! - Batch processing with automatic parallel/sequential mode selection
//! - Per-request response channels using oneshot
//! - Graceful shutdown via tokio::select! on shutdown signals

use crate::packing_service::{
    UnpackingInternals, UnpackingPriority,
    config::UnpackingConfig,
    types::{PackedChunks, UnpackingHandle},
};

use super::super::{errors::UnpackingError, types::UnpackingRequest};

use irys_packing::unpack;
use irys_types::{Config, PackedChunk, TokioServiceHandle, UnpackedChunk};
use rayon::prelude::*;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Default timeout for batch unpacking operations
const DEFAULT_UNPACKING_TIMEOUT: Duration = Duration::from_secs(30);

// Priority queue sized 10x larger to prevent blocking execution-critical requests
const MAX_BACKGROUND_CHANNEL_BOUND: usize = 100;
const MAX_PRIORITY_CHANNEL_BOUND: usize = 1000;

/// Internal unpacking service for CPU-bound chunk decompression.
///
/// Architecture: Two-stage channel design
/// 1. External channel (rx/sender params): Routes requests from multiple sources
/// 2. Internal background queue: Single-consumer processing with backpressure
///
/// Requests typically contain a single chunk, though the API supports batches.
/// Uses Rayon thread pool for parallel unpacking when batch size exceeds thread count.
#[derive(Clone)]
pub struct InternalUnpackingService {
    config: Arc<Config>,
    unpacking_config: UnpackingConfig,
    background_job_enqueue: mpsc::Sender<UnpackingRequest>,
    background_job_dequeue: Arc<tokio::sync::Mutex<mpsc::Receiver<UnpackingRequest>>>,
    priority_job_enqueue: mpsc::Sender<UnpackingRequest>,
    priority_job_dequeue: Arc<tokio::sync::Mutex<mpsc::Receiver<UnpackingRequest>>>,
    thread_pool: Arc<rayon::ThreadPool>,
    num_threads: usize,
    unpacking_timeout: Duration,
}

impl InternalUnpackingService {
    pub fn new(config: Arc<Config>) -> Self {
        let (background_job_enqueue, background_job_dequeue) =
            mpsc::channel(MAX_BACKGROUND_CHANNEL_BOUND);
        let (priority_job_enqueue, priority_job_dequeue) =
            mpsc::channel(MAX_PRIORITY_CHANNEL_BOUND);
        let unpacking_config = UnpackingConfig::new(&config);
        let num_threads = config.node_config.packing.local.cpu_unpacking_concurrency as usize;

        let thread_pool = match rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .thread_name(|idx| format!("unpack-cpu-{}", idx))
            .build()
        {
            Ok(pool) => pool,
            Err(e) => {
                warn!(
                    target: "irys::unpacking",
                    error = %e,
                    "Failed to create unpacking thread pool with {} threads, using global Rayon pool instead",
                    num_threads
                );
                rayon::ThreadPoolBuilder::new()
                    .num_threads(1)
                    .build()
                    .unwrap_or_else(|_| {
                        panic!("Critical: Unable to create even a single-threaded Rayon pool");
                    })
            }
        };

        info!(
            target: "irys::unpacking",
            "Initialized unpacking service with {} threads",
            num_threads
        );

        // Wrap receiver in Arc<Mutex> to enable Clone trait, required for spawning
        // multiple controller instances. Mutex ensures only one controller receives
        // each message (single-consumer pattern).
        Self {
            config,
            unpacking_config,
            background_job_enqueue,
            background_job_dequeue: Arc::new(tokio::sync::Mutex::new(background_job_dequeue)),
            priority_job_enqueue,
            priority_job_dequeue: Arc::new(tokio::sync::Mutex::new(priority_job_dequeue)),
            thread_pool: Arc::new(thread_pool),
            num_threads,
            unpacking_timeout: DEFAULT_UNPACKING_TIMEOUT,
        }
    }

    fn process_single_chunk(
        chunk: PackedChunk,
        entropy_iterations: u32,
        chunk_size: usize,
        chain_id: u64,
    ) -> Result<UnpackedChunk, UnpackingError> {
        let unpacked_chunk = unpack(&chunk, entropy_iterations, chunk_size, chain_id);
        Ok(unpacked_chunk)
    }

    /// Execute batch synchronously on thread pool
    fn execute_request(
        chunks: PackedChunks,
        num_threads: usize,
        entropy_iterations: u32,
        chunk_size: usize,
        chain_id: u64,
    ) -> Vec<Result<UnpackedChunk, UnpackingError>> {
        let batch_size = chunks.len();

        // Performance: Use sequential iteration when batch size ≤ thread count to avoid
        // Rayon's overhead. Parallel iteration only beneficial when work can be distributed
        // across multiple threads (batch_size > num_threads). Benchmarked crossover point.
        if batch_size <= num_threads {
            debug!(
                target: "irys::unpacking",
                "Using sequential unpacking for small batch (size: {} <= threads: {})",
                batch_size,
                num_threads
            );

            chunks
                .iter()
                .map(|chunk| {
                    Self::process_single_chunk(
                        chunk.clone(),
                        entropy_iterations,
                        chunk_size,
                        chain_id,
                    )
                })
                .collect()
        } else {
            debug!(
                target: "irys::unpacking",
                "Using parallel unpacking for large batch (size: {} > threads: {})",
                batch_size,
                num_threads
            );

            chunks
                .par_iter()
                .map(|chunk| {
                    Self::process_single_chunk(
                        chunk.clone(),
                        entropy_iterations,
                        chunk_size,
                        chain_id,
                    )
                })
                .collect()
        }
    }

    /// Process a unpacking request
    async fn unpack_request(&self, request: UnpackingRequest) {
        let response_tx = request.response_tx;

        if request.packed_chunks.is_empty() {
            let _ = response_tx.send(Err(UnpackingError::UnpackingFailed(
                "Empty request".to_string(),
            )));
            return;
        }

        let batch_size = request.packed_chunks.len();
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
        let packed_chunks = request.packed_chunks;

        let result = tokio::time::timeout(
            batch_timeout,
            tokio::task::spawn_blocking(move || {
                thread_pool.install(|| {
                    Self::execute_request(
                        packed_chunks,
                        num_threads,
                        entropy_iterations,
                        chunk_size,
                        chain_id,
                    )
                })
            }),
        )
        .await;

        let final_result = match result {
            Ok(Ok(results)) => {
                debug!(
                    target: "irys::unpacking",
                    "Completed unpacking batch of {} requests",
                    batch_size
                );
                results.into_iter().next().unwrap_or_else(|| {
                    Err(UnpackingError::UnpackingFailed(
                        "No results returned".to_string(),
                    ))
                })
            }
            Ok(Err(e)) => {
                tracing::error!(
                    target: "irys::unpacking::error",
                    "Unpacking task failed to complete: {} (batch size: {})",
                    e,
                    batch_size
                );
                Err(UnpackingError::UnpackingFailed(format!(
                    "Task panic: {}",
                    e
                )))
            }
            Err(_elapsed) => {
                tracing::error!(
                    target: "irys::unpacking::error",
                    "Unpacking batch timed out after {:?} (batch size: {}, mode: {})",
                    batch_timeout,
                    batch_size,
                    mode
                );
                Err(UnpackingError::Timeout(batch_timeout))
            }
        };

        let _ = response_tx.send(final_result);
    }

    /// Main worker loop processing unpacking requests
    async fn process_jobs(self) {
        loop {
            let msg = tokio::select! {
                // Always check priority queue first to prevent starvation of execution-critical requests
                biased;

                msg = async {
                    let mut rx = self.priority_job_dequeue.lock().await;
                    rx.recv().await
                } => msg,

                msg = async {
                    let mut rx = self.background_job_dequeue.lock().await;
                    rx.recv().await
                } => msg,
            };

            match msg {
                Some(msg) => {
                    self.unpack_request(msg).await;
                }
                None => break,
            }
        }
    }

    /// Create a detached channel for unpacking requests
    pub fn channel(
        bound: usize,
    ) -> (
        mpsc::Sender<UnpackingRequest>,
        mpsc::Receiver<UnpackingRequest>,
    ) {
        mpsc::channel::<UnpackingRequest>(bound)
    }

    /// Attach a previously created receiver (setup only, doesn't spawn worker)
    pub fn attach_receiver_loop(
        &mut self,
        runtime_handle: tokio::runtime::Handle,
        mut rx: mpsc::Receiver<UnpackingRequest>,
        sender: mpsc::Sender<UnpackingRequest>,
    ) -> UnpackingHandle {
        let internals = UnpackingInternals {
            config: self.unpacking_config.clone(),
        };

        let background_enqueue = self.background_job_enqueue.clone();
        let priority_enqueue = self.priority_job_enqueue.clone();

        runtime_handle.spawn(async move {
            loop {
                tokio::select! {
                    Some(msg) = rx.recv() => {
                        use UnpackingPriority::*;
                        if let Err(e) =  match msg.priority {
                            High => priority_enqueue.send(msg).await,
                            Background => background_enqueue.send(msg).await,
                        } {
                            warn!(target: "irys::unpacking", "Failed to enqueue unpacking job: {:?}", e);
                        }
                    },
                    else => break,
                }
            }
        });

        UnpackingHandle::new(sender, internals)
    }

    /// Spawn unpacking controller with shutdown signal support
    pub fn spawn_unpacking_controllers(
        &self,
        runtime_handle: tokio::runtime::Handle,
    ) -> Vec<TokioServiceHandle> {
        let mut handles = Vec::new();
        let controller_count = std::cmp::max(
            1_usize,
            self.unpacking_config.unpacking_concurrency as usize,
        );

        for i in 0..controller_count {
            let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
            let self_clone = self.clone();

            let handle = runtime_handle.spawn(async move {
                tokio::select! {
                    _ = Self::process_jobs(self_clone) => {},
                    _ = shutdown_rx => {
                        tracing::info!("Unpacking controller {} received shutdown signal", i);
                    }
                }
            });

            handles.push(TokioServiceHandle {
                name: format!("unpacking_controller_{}", i),
                handle,
                shutdown_signal: shutdown_tx,
            });
        }

        handles
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{ConsensusConfig, NodeConfig, StorageSyncConfig};

    fn create_test_config(tmp_dir: &tempfile::TempDir, unpacking_concurrency: u16) -> Config {
        let base_path = tmp_dir.path().to_path_buf();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                entropy_packing_iterations: 100000,
                num_chunks_in_partition: 50,
                chunk_size: 32,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 10,
            },
            packing: irys_types::PackingConfig {
                local: irys_types::LocalPackingConfig {
                    cpu_packing_concurrency: 2,
                    cpu_unpacking_concurrency: unpacking_concurrency,
                    gpu_packing_batch_size: 10,
                },
                remote: Default::default(),
            },
            base_directory: base_path,
            ..NodeConfig::testing()
        };
        Config::new_with_random_peer_id(node_config)
    }

    #[tokio::test]
    async fn test_spawn_unpacking_controllers() {
        let tmp_dir = setup_tracing_and_temp_dir(Some("test_controllers"), false);
        let config = create_test_config(&tmp_dir, 3);

        let service = InternalUnpackingService::new(Arc::new(config));

        let handles = service.spawn_unpacking_controllers(tokio::runtime::Handle::current());

        assert_eq!(
            handles.len(),
            3,
            "Should spawn 3 controllers for concurrency of 3"
        );

        drop(handles);
    }

    #[tokio::test]
    async fn test_empty_request_handling() {
        let tmp_dir = setup_tracing_and_temp_dir(Some("test_empty"), false);
        let config = create_test_config(&tmp_dir, 1);

        let mut service = InternalUnpackingService::new(Arc::new(config));
        let (tx, rx) = mpsc::channel(10);
        let handle =
            service.attach_receiver_loop(tokio::runtime::Handle::current(), rx, tx.clone());
        let _controllers = service.spawn_unpacking_controllers(tokio::runtime::Handle::current());

        let (req, rx) = UnpackingRequest::new(Arc::new(vec![]), UnpackingPriority::High);

        handle.send(req).await.expect("Should send request");

        let result = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("Should not timeout")
            .expect("Should receive response");

        assert!(
            matches!(result, Err(UnpackingError::UnpackingFailed(_))),
            "Empty request should return UnpackingFailed error"
        );
    }

    // Helper to create minimal valid packed chunk for testing
    fn create_minimal_packed_chunk(config: &Config, chunk_index: u32) -> irys_types::PackedChunk {
        use irys_packing::{capacity_single::compute_entropy_chunk, packing_xor_vec_u8};
        use irys_types::Base64;

        let mining_address = config.node_config.miner_address();
        let partition_hash = irys_types::partition::PartitionHash::zero();
        let chunk_size = config.consensus.chunk_size as usize;
        let entropy_iterations = config.consensus.entropy_packing_iterations;
        let chain_id = config.consensus.chain_id;

        // Generate entropy (same as cpu.rs)
        let mut out = Vec::with_capacity(chunk_size);
        compute_entropy_chunk(
            mining_address,
            chunk_index as u64,
            partition_hash.0,
            entropy_iterations,
            chunk_size,
            &mut out,
            chain_id,
        );

        // Create test data and pack it
        let test_data = vec![0xAB; chunk_size];
        let packed_data = packing_xor_vec_u8(out, &test_data);

        irys_types::PackedChunk {
            data_root: Default::default(),
            data_size: config.consensus.chunk_size,
            data_path: Default::default(),
            bytes: Base64(packed_data),
            packing_address: mining_address,
            partition_offset: irys_types::PartitionChunkOffset(chunk_index),
            tx_offset: irys_types::TxChunkOffset(0),
            partition_hash,
        }
    }

    #[tokio::test]
    async fn test_unpacking_chunks() {
        let tmp_dir = setup_tracing_and_temp_dir(Some("test_actual_unpack"), false);
        let config = create_test_config(&tmp_dir, 4);

        // Create a packed chunk
        let packed_chunk = create_minimal_packed_chunk(&config, 0);

        // Create unpacking service
        let mut service = InternalUnpackingService::new(Arc::new(config.clone()));
        let (tx, rx) = mpsc::channel(10);
        let handle =
            service.attach_receiver_loop(tokio::runtime::Handle::current(), rx, tx.clone());
        let _controllers = service.spawn_unpacking_controllers(tokio::runtime::Handle::current());

        // Send unpacking request
        let (req, result_rx) = UnpackingRequest::from_chunk(packed_chunk, UnpackingPriority::High);

        handle.send(req).await.expect("Should send request");

        // Wait for result
        let unpacked = tokio::time::timeout(Duration::from_secs(5), result_rx)
            .await
            .expect("Should not timeout")
            .expect("Should receive response")
            .expect("Unpacking should succeed");

        // Verify size matches expected chunk size
        assert_eq!(
            unpacked.bytes.0.len(),
            config.consensus.chunk_size as usize,
            "Unpacked chunk should match expected size"
        );

        // Verify unpacked data matches original test data (we packed 0xAB bytes)
        assert!(
            unpacked.bytes.0.iter().all(|&b| b == 0xAB),
            "All bytes should be 0xAB after unpacking"
        );
    }

    #[tokio::test]
    async fn test_priority_queue_ordering() {
        let tmp_dir = setup_tracing_and_temp_dir(Some("test_priority_order"), false);
        let config = create_test_config(&tmp_dir, 1); // Single worker to serialize

        let mut service = InternalUnpackingService::new(Arc::new(config.clone()));
        let (tx, rx) = mpsc::channel(100);
        let handle =
            service.attach_receiver_loop(tokio::runtime::Handle::current(), rx, tx.clone());
        let _controllers = service.spawn_unpacking_controllers(tokio::runtime::Handle::current());

        let processed_order = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let mut result_receivers = Vec::new();

        // Send 20 background requests to ensure queue has pending work
        for i in 0..20 {
            let packed_chunk = create_minimal_packed_chunk(&config, i);
            let (req, result_rx) =
                UnpackingRequest::from_chunk(packed_chunk, UnpackingPriority::Background);
            handle.send(req).await.expect("Should send request");

            let order = processed_order.clone();
            let handle = tokio::spawn(async move {
                let _ = result_rx.await;
                order.lock().await.push("bg");
            });
            result_receivers.push(handle);
        }

        // Very small delay to let first request start processing
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Send 5 high-priority requests
        for i in 0..5 {
            let packed_chunk = create_minimal_packed_chunk(&config, i + 100);
            let (req, result_rx) =
                UnpackingRequest::from_chunk(packed_chunk, UnpackingPriority::High);
            handle.send(req).await.expect("Should send request");

            let order = processed_order.clone();
            let handle = tokio::spawn(async move {
                let _ = result_rx.await;
                order.lock().await.push("high");
            });
            result_receivers.push(handle);
        }

        // Wait for all to complete
        for handle in result_receivers {
            handle.await.expect("Should complete");
        }

        let order = processed_order.lock().await;

        // Verify all requests were processed
        let high_count = order.iter().filter(|x| **x == "high").count();
        let bg_count = order.iter().filter(|x| **x == "bg").count();

        assert_eq!(
            high_count, 5,
            "All high-priority requests should be processed"
        );
        assert_eq!(bg_count, 20, "All background requests should be processed");

        // Find first high-priority position
        if let Some(first_high_pos) = order.iter().position(|x| *x == "high") {
            // With 20 background and 5 high, high priority should have jumped ahead
            // It should appear before the last 10 background requests at minimum
            assert!(
                first_high_pos < 15,
                "High priority should jump ahead significantly, but was at position: {} out of 25",
                first_high_pos
            );
        }
    }

    #[tokio::test]
    async fn test_concurrent_unpacking_multiple_chunks() {
        let tmp_dir = setup_tracing_and_temp_dir(Some("test_concurrent"), false);
        let config = create_test_config(&tmp_dir, 4); // Multiple workers

        let mut service = InternalUnpackingService::new(Arc::new(config.clone()));
        let (tx, rx) = mpsc::channel(100);
        let handle =
            service.attach_receiver_loop(tokio::runtime::Handle::current(), rx, tx.clone());
        let _controllers = service.spawn_unpacking_controllers(tokio::runtime::Handle::current());

        let success_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handles = Vec::new();

        // Send 20 concurrent requests
        for i in 0..20 {
            let packed_chunk = create_minimal_packed_chunk(&config, i);
            let (req, result_rx) =
                UnpackingRequest::from_chunk(packed_chunk, UnpackingPriority::Background);
            handle.send(req).await.expect("Should send request");

            let count = success_count.clone();
            let h = tokio::spawn(async move {
                if let Ok(Ok(_)) = result_rx.await {
                    count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            });
            handles.push(h);
        }

        // Wait for all
        for h in handles {
            h.await.expect("Should complete");
        }

        let successes = success_count.load(std::sync::atomic::Ordering::Relaxed);

        assert!(
            successes == 20,
            "All concurrent requests should succeed, got {}/20",
            successes
        );
    }
}
