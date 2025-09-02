use irys_types::BlockHash;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

const MAX_PROCESSING_BLOCKS_QUEUE_SIZE: usize = 100;

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
}

impl ChainSyncState {
    /// Creates a new SyncState with a given syncing flag and sync_height = 0
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

    /// [`crate::block_pool::BlockPool`] marks block as processed once the
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

    /// Highest pre-validated block height. Set by the [`crate::block_pool::BlockPool`]
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
    /// marked by [`crate::block_pool::BlockPool`] after pre-validation
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
}
