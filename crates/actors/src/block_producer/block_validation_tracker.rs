//! Block validation tracking for parent selection in block production.
//!
//! # Problem
//! Block producer must select the block with highest cumulative difficulty as parent,
//! but only if that block and all its ancestors are fully validated. Validation is
//! asynchronous and lags behind block discovery. But we don't want to create unnecessary forks.
//!
//! # Solution
//! Monitor validation progress with a timeout. Track the best candidate
//! (highest difficulty) while waiting for validation. If timeout occurs, fall back
//! to the latest fully validated block to ensure production continues.
//!
//! # Algorithm
//! 1. Identify block with maximum cumulative difficulty
//! 2. Check if fully validated (including all ancestors)
//! 3. If not validated:
//!    - Monitor block state updates
//!    - Track if a new max-difficulty block appears
//!    - Track validation progress
//!    - Reset timer on significant changes
//! 4. Return selected block or fallback on timeout
//!
//! # Timer Resets
//! The timer resets when:
//! - A new block with higher cumulative difficulty appears
//! - Validation progresses (fewer blocks awaiting validation)
//!
//! This ensures each significant change gets a fresh timeout window, preventing
//! premature fallbacks during active network progress.

use eyre::eyre;
use irys_types::{H256, U256};
use std::time::Duration;
use tokio::sync::broadcast::Receiver;
use tokio::time::Instant;
use tracing::{debug, info, trace, warn};

use crate::{block_producer::BlockProducerInner, block_tree_service::BlockStateUpdated};

/// Tracks the state of block validation during parent block selection
#[derive(Debug)]
pub struct BlockValidationTracker<'a> {
    /// The block with maximum cumulative difficulty
    pub target_block_hash: H256,
    /// The maximum cumulative difficulty value
    pub max_difficulty: U256,
    /// Number of blocks in the chain awaiting validation
    pub blocks_awaiting_validation: usize,
    /// Fallback block hash (latest fully validated block)
    pub fallback_block_hash: Option<H256>,
    /// Deadline for waiting on validation
    pub deadline: Instant,
    /// Reference to the block producer inner state
    inner: &'a BlockProducerInner,
}

impl<'a> BlockValidationTracker<'a> {
    /// Creates a new tracker with initial state
    pub fn new(
        target_block_hash: H256,
        max_difficulty: U256,
        blocks_awaiting_validation: usize,
        fallback_block_hash: Option<H256>,
        wait_duration: Duration,
        inner: &'a BlockProducerInner,
    ) -> Self {
        Self {
            target_block_hash,
            max_difficulty,
            blocks_awaiting_validation,
            fallback_block_hash,
            deadline: Instant::now() + wait_duration,
            inner,
        }
    }

    /// Resets the deadline timer
    pub fn reset_deadline(&mut self, wait_duration: Duration) {
        self.deadline = Instant::now() + wait_duration;
    }

    /// Checks if the deadline has been exceeded
    pub fn is_timeout(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Updates the target block if a new max difficulty block is found
    pub fn update_target(
        &mut self,
        new_target: H256,
        new_max_diff: U256,
        blocks_awaiting: usize,
        fallback: Option<H256>,
    ) {
        self.target_block_hash = new_target;
        self.max_difficulty = new_max_diff;
        self.blocks_awaiting_validation = blocks_awaiting;
        self.fallback_block_hash = fallback;
    }

    /// Selects the block with maximum cumulative difficulty from the block tree
    fn select_max_difficulty_block(&self) -> (H256, U256) {
        let read = self.inner.block_tree_guard.read();
        read.get_max_cumulative_difficulty_block()
    }

    /// Checks the validation status of a block and its ancestors
    /// Returns (blocks_awaiting_validation, fallback_block_hash)
    fn check_block_validation_status(&self, block_hash: &H256) -> (usize, Option<H256>) {
        let read = self.inner.block_tree_guard.read();
        read.get_validation_chain_status(block_hash)
    }

    /// Checks if a block can be used as a parent for new block production
    fn can_build_upon(&self, block_hash: &H256) -> bool {
        let read = self.inner.block_tree_guard.read();
        read.can_be_built_upon(block_hash)
    }

    /// Waits for a block to be fully validated, monitoring validation progress
    /// Returns the final block hash to use (either the target or fallback)
    pub async fn wait_for_validation(
        &mut self,
        max_wait_time: Duration,
        block_state_rx: &mut Receiver<BlockStateUpdated>,
    ) -> eyre::Result<H256> {
        let start_time = Instant::now();

        debug!(
            target_block = %self.target_block_hash,
            blocks_awaiting = self.blocks_awaiting_validation,
            fallback_block = ?self.fallback_block_hash,
            "Starting validation wait"
        );

        loop {
            // Check timeout
            if self.is_timeout() {
                let elapsed = start_time.elapsed();

                if let Some(fallback) = self.fallback_block_hash {
                    warn!(
                        target_block = %self.target_block_hash,
                        fallback_block = %fallback,
                        elapsed_ms = elapsed.as_millis(),
                        blocks_still_awaiting = self.blocks_awaiting_validation,
                        "Validation timeout - using fallback block"
                    );
                    return Ok(fallback);
                } else {
                    warn!(
                        target_block = %self.target_block_hash,
                        elapsed_ms = elapsed.as_millis(),
                        blocks_still_awaiting = self.blocks_awaiting_validation,
                        "Validation timeout - no fallback available"
                    );
                    return Err(eyre!("No validated blocks found in the chain"));
                }
            }

            // Wait for either a block state event or the deadline
            let event = tokio::select! {
                event = block_state_rx.recv() => {
                    event.expect("channel must never be closed")
                }
                _ = tokio::time::sleep_until(self.deadline) => {
                    trace!("Deadline sleep completed, checking timeout");
                    continue; // Will trigger timeout check on next iteration
                }
            };

            trace!(
                block_hash = %event.block_hash,
                discarded = event.discarded,
                "Received block state event"
            );

            // Skip discarded blocks
            if event.discarded {
                continue;
            }

            // Check if max difficulty has changed
            let (new_max_block, new_max_diff) = self.select_max_difficulty_block();

            if new_max_block != self.target_block_hash {
                debug!(
                    old_target = %self.target_block_hash,
                    new_target = %new_max_block,
                    old_difficulty = %self.max_difficulty,
                    new_difficulty = %new_max_diff,
                    elapsed_ms = start_time.elapsed().as_millis(),
                    "Max difficulty block changed during validation wait"
                );

                // Re-check validation status for the new max block
                let (new_blocks_awaiting, new_fallback) =
                    self.check_block_validation_status(&new_max_block);

                self.update_target(
                    new_max_block,
                    new_max_diff,
                    new_blocks_awaiting,
                    new_fallback,
                );

                // Reset timer when target block changes
                self.reset_deadline(max_wait_time);
                debug!(
                    new_blocks_awaiting = self.blocks_awaiting_validation,
                    "Timer reset due to new target block"
                );
            }

            // Check if the current target block can be used as a parent
            if self.can_build_upon(&self.target_block_hash) && self.blocks_awaiting_validation == 0
            {
                let elapsed = start_time.elapsed();
                info!(
                    target_block = %self.target_block_hash,
                    elapsed_ms = elapsed.as_millis(),
                    "Target block validated successfully"
                );
                return Ok(self.target_block_hash);
            }

            // Re-check validation status
            let (current_blocks_awaiting, current_fallback) =
                self.check_block_validation_status(&self.target_block_hash);

            if current_blocks_awaiting < self.blocks_awaiting_validation {
                let validated_count = self.blocks_awaiting_validation - current_blocks_awaiting;
                debug!(
                    blocks_validated = validated_count,
                    blocks_remaining = current_blocks_awaiting,
                    blocks_were = self.blocks_awaiting_validation,
                    elapsed_ms = start_time.elapsed().as_millis(),
                    "Validation progress detected"
                );

                // Reset timer when validation makes progress
                self.reset_deadline(max_wait_time);
                trace!("Timer reset due to validation progress");
            }

            self.blocks_awaiting_validation = current_blocks_awaiting;
            self.fallback_block_hash = current_fallback;

            if self.blocks_awaiting_validation == 0 {
                let elapsed = start_time.elapsed();
                info!(
                    target_block = %self.target_block_hash,
                    elapsed_ms = elapsed.as_millis(),
                    "All blocks validated successfully"
                );
                return Ok(self.target_block_hash);
            }
        }
    }
}
