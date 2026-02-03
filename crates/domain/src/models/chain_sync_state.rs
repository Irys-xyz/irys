use irys_types::BlockHash;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};
use tracing::{debug, warn};

const MAX_PROCESSING_BLOCKS_QUEUE_SIZE: usize = 100;

const MAX_LAST_BLOCK_VALIDATION_ERRORS: usize = 10;

/// Diagnostic information about chain synchronization errors and events.
/// This struct is public with private fields and provides public methods for recording
/// and retrieving diagnostic information.
#[derive(Clone, Debug, Default)]
pub struct SyncDiagnosticInfo {
    total_block_validation_errors: usize,
    last_block_validation_errors: Vec<(String, SystemTime)>,
    last_vdf_step: Option<(u64, SystemTime)>,
    total_block_processing_errors: usize,
    last_block_processing_errors: Vec<(String, SystemTime)>,
    total_data_pull_errors: usize,
    last_data_pull_errors: Vec<(String, SystemTime)>,
    latest_successfully_processed_block_info: (BlockHash, Option<SystemTime>),
}

impl SyncDiagnosticInfo {
    pub fn new() -> Self {
        Self {
            total_block_validation_errors: 0,
            last_block_validation_errors: Vec::new(),
            last_vdf_step: None,
            last_block_processing_errors: Vec::new(),
            total_block_processing_errors: 0,
            total_data_pull_errors: 0,
            last_data_pull_errors: Vec::new(),
            latest_successfully_processed_block_info: (BlockHash::default(), None),
        }
    }

    pub fn record_block_validation_error(&mut self, error: String) {
        self.total_block_validation_errors += 1;
        let now = SystemTime::now();
        self.last_block_validation_errors.push((error, now));
        if self.last_block_validation_errors.len() > MAX_LAST_BLOCK_VALIDATION_ERRORS {
            self.last_block_validation_errors.remove(0);
        }
    }

    pub fn record_vdf_step(&mut self, step_number: u64) {
        let now = SystemTime::now();
        self.last_vdf_step = Some((step_number, now));
    }

    pub fn record_block_processing_error(&mut self, error: String) {
        let now = SystemTime::now();
        self.total_block_processing_errors += 1;
        self.last_block_processing_errors.push((error, now));
        if self.last_block_processing_errors.len() > MAX_LAST_BLOCK_VALIDATION_ERRORS {
            self.last_block_processing_errors.remove(0);
        }
    }

    pub fn record_data_pull_error(&mut self, error: String) {
        let now = SystemTime::now();
        self.total_data_pull_errors += 1;
        self.last_data_pull_errors.push((error, now));
        if self.last_data_pull_errors.len() > MAX_LAST_BLOCK_VALIDATION_ERRORS {
            self.last_data_pull_errors.remove(0);
        }
    }

    pub fn record_latest_processed_block_success(&mut self, block_hash: BlockHash) {
        let now = SystemTime::now();
        self.latest_successfully_processed_block_info = (block_hash, Some(now));
    }

    /// Format diagnostic information as a human-readable string
    pub fn format_summary(&self) -> String {
        fn format_timestamp(time: &SystemTime) -> String {
            match time.duration_since(SystemTime::UNIX_EPOCH) {
                Ok(duration) => {
                    let secs = duration.as_secs();
                    let millis = duration.subsec_millis();
                    // Format as Unix timestamp with milliseconds for precision
                    format!("{}.{:03}", secs, millis)
                }
                Err(_) => "invalid_time".to_string(),
            }
        }

        let mut output = String::new();
        output.push_str("=== Chain Sync Diagnostics ===\n");

        // Block validation errors
        output.push_str(&format!(
            "Total block validation errors: {}\n",
            self.total_block_validation_errors
        ));
        if !self.last_block_validation_errors.is_empty() {
            output.push_str(&format!(
                "Last validation errors (most recent {}):\n",
                MAX_LAST_BLOCK_VALIDATION_ERRORS
            ));
            for (error, timestamp) in self.last_block_validation_errors.iter().rev() {
                output.push_str(&format!(
                    "  - [{}] {}\n",
                    format_timestamp(timestamp),
                    error
                ));
            }
        }
        output.push('\n');

        // Block processing errors
        output.push_str(&format!(
            "Total block processing errors: {}\n",
            self.total_block_processing_errors
        ));
        if !self.last_block_processing_errors.is_empty() {
            output.push_str(&format!(
                "Last processing errors (most recent {}):\n",
                MAX_LAST_BLOCK_VALIDATION_ERRORS
            ));
            for (error, timestamp) in self.last_block_processing_errors.iter().rev() {
                output.push_str(&format!(
                    "  - [{}] {}\n",
                    format_timestamp(timestamp),
                    error
                ));
            }
        }
        output.push('\n');

        // Data pull errors
        output.push_str(&format!(
            "Total data pull errors: {}\n",
            self.total_data_pull_errors
        ));
        if !self.last_data_pull_errors.is_empty() {
            output.push_str(&format!(
                "Last data pull errors (most recent {}):\n",
                MAX_LAST_BLOCK_VALIDATION_ERRORS
            ));
            for (error, timestamp) in self.last_data_pull_errors.iter().rev() {
                output.push_str(&format!(
                    "  - [{}] {}\n",
                    format_timestamp(timestamp),
                    error
                ));
            }
        }
        output.push('\n');

        // VDF status
        output.push_str("VDF Status:\n");
        if let Some((step_number, timestamp)) = self.last_vdf_step {
            output.push_str(&format!(
                "  - Last recorded step: {} at {}\n",
                step_number,
                format_timestamp(&timestamp)
            ));
        } else {
            output.push_str("  - No VDF steps recorded yet\n");
        }
        output.push('\n');

        // Last successful block
        let (block_hash, timestamp_opt) = &self.latest_successfully_processed_block_info;
        if let Some(timestamp) = timestamp_opt {
            output.push_str(&format!(
                "Last successful block: {} at {}\n",
                block_hash,
                format_timestamp(timestamp)
            ));
        } else {
            output.push_str("Last successful block: None\n");
        }

        output.push_str("===");
        output
    }
}

/// State tracking for chain synchronization.
/// Tracks sync progress, validation queue, and diagnostic information.
#[derive(Clone, Debug, Default)]
pub struct ChainSyncState {
    syncing: Arc<AtomicBool>,
    trusted_sync: Arc<AtomicBool>,
    sync_target_height: Arc<AtomicUsize>,
    highest_processed_block: Arc<AtomicUsize>,
    last_synced_block_hash: Arc<RwLock<Option<BlockHash>>>,
    switch_to_full_validation_at_height: Arc<RwLock<Option<usize>>>,
    gossip_broadcast_enabled: Arc<AtomicBool>,
    gossip_reception_enabled: Arc<AtomicBool>,
    diagnostic_info: Arc<RwLock<SyncDiagnosticInfo>>,
}

impl ChainSyncState {
    /// Creates a new ChainSyncState with a given syncing flag and sync_height = 0
    pub fn new(is_syncing: bool, is_trusted_sync: bool) -> Self {
        Self {
            syncing: Arc::new(AtomicBool::new(is_syncing)),
            trusted_sync: Arc::new(AtomicBool::new(is_trusted_sync)),
            sync_target_height: Arc::new(AtomicUsize::new(0)),
            highest_processed_block: Arc::new(AtomicUsize::new(0)),
            last_synced_block_hash: Arc::new(RwLock::new(None)),
            switch_to_full_validation_at_height: Arc::new(RwLock::new(None)),
            gossip_broadcast_enabled: Arc::new(AtomicBool::new(true)),
            gossip_reception_enabled: Arc::new(AtomicBool::new(true)),
            diagnostic_info: Arc::new(RwLock::new(SyncDiagnosticInfo::new())),
        }
    }

    pub fn set_is_syncing(&self, is_syncing: bool) {
        self.syncing.store(is_syncing, Ordering::Relaxed);
        self.set_gossip_broadcast_enabled(!is_syncing);
    }

    pub fn set_syncing_from(&self, height: usize) {
        self.set_sync_target_height(height);
        self.mark_processed(height.saturating_sub(1));
    }

    pub fn finish_sync(&self) {
        self.set_is_syncing(false);
    }

    /// Returns whether the gossip service is currently syncing
    pub fn is_syncing(&self) -> bool {
        self.syncing.load(Ordering::Relaxed)
    }

    /// Waits for the sync flag to be set to false.
    #[must_use]
    pub async fn wait_for_sync(&self) {
        // If already synced, return immediately
        if !self.is_syncing() {
            return;
        }

        // Create a future that polls the sync state
        let syncing = Arc::clone(&self.syncing);
        tokio::spawn(async move {
            while syncing.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("Sync checking task failed");
    }

    /// Sets the current sync height. During syncing, the gossip won't
    /// accept blocks higher than this height
    pub fn set_sync_target_height(&self, height: usize) {
        self.sync_target_height.store(height, Ordering::Relaxed);
    }

    /// Returns the current sync height
    pub fn sync_target_height(&self) -> usize {
        self.sync_target_height.load(Ordering::Relaxed)
    }

    /// irys_p2p::block_pool::BlockPool marks block as processed once the
    /// BlockDiscovery finished the pre-validation and scheduled the block for full validation
    pub fn mark_processed(&self, height: usize) {
        let current_height = self.highest_processed_block.load(Ordering::Relaxed);
        if height > current_height {
            self.highest_processed_block
                .store(height, Ordering::Relaxed);

            if let Some(switch_height) = *self.switch_to_full_validation_at_height.read().unwrap() {
                if self.is_trusted_sync() && height >= switch_height {
                    self.set_trusted_sync(false)
                }
            }
        }
    }

    /// Mark a block as processed with its hash
    pub fn mark_processed_with_hash(&self, height: usize, block_hash: BlockHash) {
        self.mark_processed(height);
        let mut hash_lock = self.last_synced_block_hash.write().unwrap();
        *hash_lock = Some(block_hash);
    }

    /// Get the last synced block hash
    pub fn last_synced_block_hash(&self) -> Option<BlockHash> {
        *self.last_synced_block_hash.read().unwrap()
    }

    /// Sets the height at which the node should switch to full validation.
    pub fn set_switch_to_full_validation_at_height(&self, height: Option<usize>) {
        let mut lock = self.switch_to_full_validation_at_height.write().unwrap();
        *lock = height;
    }

    /// Returns the height at which the node should switch to full validation.
    pub fn full_validation_switch_height(&self) -> Option<usize> {
        *self.switch_to_full_validation_at_height.read().unwrap()
    }

    pub fn is_in_trusted_sync_range(&self, height: usize) -> bool {
        if let Some(switch_height) = self.full_validation_switch_height() {
            self.is_trusted_sync() && switch_height >= height
        } else {
            false
        }
    }

    /// Highest pre-validated block height. Set by the irys_p2p::block_pool::BlockPool
    pub fn highest_processed_block(&self) -> usize {
        self.highest_processed_block.load(Ordering::Relaxed)
    }

    /// Sets whether gossip broadcast is enabled
    pub fn set_gossip_broadcast_enabled(&self, enabled: bool) {
        self.gossip_broadcast_enabled
            .store(enabled, Ordering::Relaxed);
    }

    /// Returns whether gossip broadcast is enabled
    pub fn is_gossip_broadcast_enabled(&self) -> bool {
        self.gossip_broadcast_enabled.load(Ordering::Relaxed)
    }

    /// Sets whether gossip reception is enabled
    pub fn set_gossip_reception_enabled(&self, enabled: bool) {
        self.gossip_reception_enabled
            .store(enabled, Ordering::Relaxed);
    }

    /// Returns whether gossip reception is enabled
    pub fn is_gossip_reception_enabled(&self) -> bool {
        self.gossip_reception_enabled.load(Ordering::Relaxed)
    }

    /// Checks if more blocks can be scheduled for validation by checking the
    /// number of blocks scheduled for validation so far versus the highest block
    /// marked by irys_p2p::block_pool::BlockPool after pre-validation
    pub fn is_queue_full(&self) -> bool {
        // We already past the sync target height, so there's nothing in the queue
        //  scheduled by the sync task specifically (gossip still can schedule blocks)
        if self.highest_processed_block() > self.sync_target_height() {
            return false;
        }

        self.sync_target_height() - self.highest_processed_block()
            >= MAX_PROCESSING_BLOCKS_QUEUE_SIZE
    }

    /// Waits until the length of the validation queue is less than the maximum
    /// allowed size. Cancels after 30 seconds.
    pub async fn wait_for_an_empty_queue_slot(&self) -> Result<(), tokio::time::error::Elapsed> {
        self.wait_for_an_empty_queue_slot_with_timeout(Duration::from_secs(30))
            .await
    }

    /// Waits until the length of the validation queue is less than the maximum
    /// allowed size, with a custom timeout.
    pub async fn wait_for_an_empty_queue_slot_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<(), tokio::time::error::Elapsed> {
        tokio::time::timeout(timeout, async {
            while self.is_queue_full() {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
        })
        .await
    }

    /// Waits for the highest pre-validated block to reach target sync height
    /// This has a progress/time based early out - if we don't make at least a block's worth of progress in `progress_timeout`, we return early
    pub async fn wait_for_processed_block_to_reach_target(&self) {
        // If already synced, return immediately
        if !self.is_syncing() {
            return;
        }

        // Create a future that polls the sync state
        let target = Arc::clone(&self.sync_target_height);
        let highest_processed_block = Arc::clone(&self.highest_processed_block);
        tokio::spawn(async move {
            let progress_timeout = Duration::from_secs(60);
            let mut last_made_progress = Instant::now();
            let mut prev_hpb = 0;
            loop {
                let target = target.load(Ordering::Relaxed);
                let hpb = highest_processed_block.load(Ordering::Relaxed);
                let made_progress = hpb > prev_hpb;

                // We need to add 1 to the highest processed block. For the cases when the node
                // starts fully caught up, no new blocks are added to the index, and the
                // target is always going to be one more than the highest processed block.
                // If this function never resolves, no new blocks can arrive over gossip in that case.

                if hpb + 1 >= target {
                    // synchronised
                    break;
                } else if !made_progress && last_made_progress.elapsed() > progress_timeout {
                    // didn't make any progress in the last `progress_timeout` duration
                    warn!(
                        "Did not make sync process from {} in {}ms",
                        &hpb,
                        &progress_timeout.as_millis()
                    );
                    break; // progression timeout
                } else {
                    if made_progress {
                        debug!(
                            "Progressed: {} -> {} (target: {})",
                            &prev_hpb, &hpb, &target
                        );
                        last_made_progress = Instant::now();
                        prev_hpb = hpb;
                    };
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        })
        .await
        .expect("Sync checking task failed");
    }

    pub fn set_trusted_sync(&self, is_trusted_sync: bool) {
        self.trusted_sync.store(is_trusted_sync, Ordering::Relaxed);
    }

    pub fn is_trusted_sync(&self) -> bool {
        self.trusted_sync.load(Ordering::Relaxed)
    }

    pub fn is_syncing_from_a_trusted_peer(&self) -> bool {
        self.is_syncing() && self.is_trusted_sync()
    }

    // =========================================================================
    // Diagnostic Recording Methods
    // =========================================================================

    /// Record a block validation error
    pub fn record_block_validation_error(&self, error: String) {
        let mut diagnostic = self.diagnostic_info.write().unwrap();
        diagnostic.record_block_validation_error(error);
    }

    /// Record a block processing error
    pub fn record_block_processing_error(&self, error: String) {
        let mut diagnostic = self.diagnostic_info.write().unwrap();
        diagnostic.record_block_processing_error(error);
    }

    /// Record a VDF step
    pub fn record_vdf_step(&self, step_number: u64) {
        let mut diagnostic = self.diagnostic_info.write().unwrap();
        diagnostic.record_vdf_step(step_number);
    }

    /// Record a data pull error
    pub fn record_data_pull_error(&self, error: String) {
        let mut diagnostic = self.diagnostic_info.write().unwrap();
        diagnostic.record_data_pull_error(error);
    }

    /// Record successful block processing
    pub fn record_successful_block_processing(&self, block_hash: BlockHash) {
        let mut diagnostic = self.diagnostic_info.write().unwrap();
        diagnostic.record_latest_processed_block_success(block_hash);
    }

    /// Get a formatted diagnostic summary
    pub fn get_diagnostic_summary(&self) -> String {
        let diagnostic = self.diagnostic_info.read().unwrap();
        diagnostic.format_summary()
    }
}
