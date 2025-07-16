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
//! 2. Check if fully validated
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

use crate::{block_tree_service::BlockStateUpdated, services::ServiceSenders};
use irys_domain::BlockTreeReadGuard;
use irys_types::{BlockHash, U256};
use std::time::Duration;
use tokio::sync::broadcast::Receiver;
use tokio::time::Instant;
use tracing::{debug, info, trace, warn};

/// Tracks the state of block validation during parent block selection
pub struct BlockValidationTracker {
    state: ValidationState,
    timer: Timer,
    block_tree_guard: BlockTreeReadGuard,
    block_state_rx: Receiver<BlockStateUpdated>,
}

impl BlockValidationTracker {
    /// Creates a new tracker that automatically finds the highest cumulative difficulty block
    pub fn new(
        block_tree_guard: BlockTreeReadGuard,
        service_senders: ServiceSenders,
        wait_duration: Duration,
    ) -> Self {
        // Subscribe to block state updates
        let block_state_rx = service_senders.subscribe_block_state_updates();

        // Get initial blockchain state
        let tree = block_tree_guard.read();
        let (max_difficulty, target_block_hash) = tree.get_max_cumulative_difficulty_block();
        let target_block_height = tree
            .get_block(&target_block_hash)
            .map(|b| b.height)
            .unwrap_or(0);
        let (blocks_awaiting_validation, fallback_block_hash) =
            tree.get_validation_chain_status(&target_block_hash);
        let fallback_block = fallback_block_hash
            .and_then(|hash| tree.get_block(&hash).map(|b| (hash, b.height)))
            .expect("fallback block must be present");
        drop(tree);

        Self {
            state: ValidationState::new(
                target_block_hash,
                target_block_height,
                max_difficulty,
                blocks_awaiting_validation,
                fallback_block,
            ),
            timer: Timer::new(wait_duration),
            block_tree_guard,
            block_state_rx,
        }
    }

    /// Waits for a block to be fully validated, monitoring validation progress
    /// Returns the final block hash to use (either the target or fallback)
    pub async fn wait_for_validation(&mut self) -> eyre::Result<BlockHash> {
        let start_time = Instant::now();

        loop {
            // Handle terminal states first
            let (target_hash, target_height, current_awaiting) = match self.state {
                ValidationState::Validated {
                    block_hash,
                    block_height,
                } => {
                    let elapsed = start_time.elapsed();
                    info!(
                        block_hash = %block_hash,
                        block_height = block_height,
                        elapsed_ms = elapsed.as_millis(),
                        "Block validation completed"
                    );
                    return Ok(block_hash);
                }

                ValidationState::TimedOut {
                    fallback_block,
                    abandoned_target,
                    abandoned_target_height,
                } => {
                    let (fb_hash, fb_height) = fallback_block;
                    warn!(
                        target_block = %abandoned_target,
                        target_height = abandoned_target_height,
                        fallback_block = %fb_hash,
                        fallback_height = fb_height,
                        "Validation timeout - using fallback block"
                    );
                    return Ok(fb_hash);
                }

                ref mut state @ ValidationState::Tracking {
                    target,
                    awaiting_validation,
                    fallback,
                } => {
                    let target_hash = target.hash;
                    let target_height = target.height;
                    let current_awaiting = awaiting_validation;
                    if self.timer.is_expired() {
                        let fallback_block = fallback;
                        let abandoned_target = target_hash;
                        let abandoned_target_height = target_height;

                        *state = ValidationState::TimedOut {
                            fallback_block,
                            abandoned_target,
                            abandoned_target_height,
                        };

                        // Will handle in TimedOut arm
                        continue;
                    }

                    (target_hash, target_height, current_awaiting)
                }
            };

            // Get current blockchain state
            let snapshot = self.capture_blockchain_snapshot()?;
            // Check if we can build on the block
            if snapshot.can_build_upon {
                self.state = ValidationState::Validated {
                    block_hash: snapshot.max_block.hash,
                    block_height: snapshot.max_block.height,
                };
                continue;
            }
            // update our local state copy to keep track of this snapshot
            self.state = ValidationState::Tracking {
                target: snapshot.max_block,
                awaiting_validation: snapshot.awaiting_validation,
                fallback: snapshot.fallback_block,
            };

            // Check if max difficulty block changed
            if snapshot.max_block.hash != target_hash {
                debug!(
                    old_target = %target_hash,
                    old_height = target_height,
                    new_target = %snapshot.max_block.hash,
                    new_height = snapshot.max_block.height,
                    new_difficulty = %snapshot.max_block.cumulative_difficulty,
                    "Max difficulty block changed during validation wait"
                );

                // Update to new target
                self.state = ValidationState::Tracking {
                    target: snapshot.max_block,
                    awaiting_validation: snapshot.awaiting_validation,
                    fallback: snapshot.fallback_block,
                };

                // Reset timer for new target
                self.timer.reset();
                debug!("Timer reset due to new target block");
                continue;
            }

            // Check validation progress
            let blocks_validated = snapshot.awaiting_validation.abs_diff(current_awaiting);
            if blocks_validated == 0 {
                // we have made no progress of validating the blocks we actually care about.
                // Start the loop again.
                continue;
            }

            debug!(
                target_block = %target_hash,
                target_height = target_height,
                blocks_validated = blocks_validated,
                blocks_remaining = snapshot.awaiting_validation,
                blocks_were = current_awaiting,
                "Validation progress detected"
            );

            // Reset timer on progress
            self.timer.reset();
            trace!("Timer reset due to validation progress");

            // Wait for next event or timeout; restart the loop
            let _event = tokio::select! {
                event = self.block_state_rx.recv() => {
                    Some(event.expect("channel must not close"))
                }
                _ = self.timer.sleep_until_deadline() => {
                    trace!("Deadline sleep completed");
                    None
                }
            };
        }
    }

    /// Capture all needed blockchain state in one read lock
    fn capture_blockchain_snapshot(&self) -> eyre::Result<BlockchainSnapshot> {
        let tree = self.block_tree_guard.read();

        let (max_diff, max_hash) = tree.get_max_cumulative_difficulty_block();
        let max_height = tree.get_block(&max_hash).map(|b| b.height).unwrap_or(0);
        let (awaiting, fallback_hash) = tree.get_validation_chain_status(&max_hash);
        let fallback = fallback_hash
            .and_then(|hash| tree.get_block(&hash).map(|b| (hash, b.height)))
            .expect("fallback block must be present");
        let can_build = tree.can_be_built_upon(&max_hash);

        Ok(BlockchainSnapshot {
            max_block: TargetBlock {
                hash: max_hash,
                height: max_height,
                cumulative_difficulty: max_diff,
            },
            awaiting_validation: awaiting,
            fallback_block: fallback,
            can_build_upon: can_build,
        })
    }
}

/// Represents the current state of block validation tracking
#[derive(Debug, Clone)]
pub(super) enum ValidationState {
    /// Actively tracking a target block that needs validation
    Tracking {
        /// The block we're trying to use as parent (highest cumulative difficulty)
        target: TargetBlock,
        /// Number of blocks in the chain awaiting validation
        awaiting_validation: usize,
        /// Fallback option if we timeout (latest fully validated block)
        fallback: (BlockHash, u64),
    },

    /// Target block is fully validated and ready to use
    Validated {
        block_hash: BlockHash,
        block_height: u64,
    },

    /// Timed out waiting for validation
    TimedOut {
        /// The fallback block we'll use
        fallback_block: (BlockHash, u64),
        /// The block we were waiting for (for logging)
        abandoned_target: BlockHash,
        abandoned_target_height: u64,
    },
}

/// Information about a target block being tracked
#[derive(Debug, Clone, Copy)]
pub(super) struct TargetBlock {
    pub hash: BlockHash,
    pub height: u64,
    pub cumulative_difficulty: U256,
}

/// Manages timeout and deadline logic
pub(super) struct Timer {
    deadline: Instant,
    base_duration: Duration,
}

/// Snapshot of blockchain state to reduce lock contention
struct BlockchainSnapshot {
    max_block: TargetBlock,
    awaiting_validation: usize,
    fallback_block: (BlockHash, u64),
    can_build_upon: bool,
}

impl ValidationState {
    /// Initial state when starting validation tracking
    pub(super) fn new(
        target: BlockHash,
        target_height: u64,
        difficulty: U256,
        awaiting: usize,
        fallback: (BlockHash, u64),
    ) -> Self {
        Self::Tracking {
            target: TargetBlock {
                hash: target,
                height: target_height,
                cumulative_difficulty: difficulty,
            },
            awaiting_validation: awaiting,
            fallback,
        }
    }
}

impl Timer {
    fn new(duration: Duration) -> Self {
        Self {
            deadline: Instant::now() + duration,
            base_duration: duration,
        }
    }

    fn reset(&mut self) {
        self.deadline = Instant::now() + self.base_duration;
    }

    fn is_expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    async fn sleep_until_deadline(&self) {
        tokio::time::sleep_until(self.deadline).await
    }
}
