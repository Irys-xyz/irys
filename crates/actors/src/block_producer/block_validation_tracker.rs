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

use eyre::eyre;
use irys_types::{H256, U256};
use std::time::Duration;
use tokio::sync::broadcast::Receiver;
use tokio::time::Instant;
use tracing::{debug, info, trace, warn};

use crate::{block_producer::BlockProducerInner, block_tree_service::BlockStateUpdated};

/// Tracks the state of block validation during parent block selection
pub struct BlockValidationTracker<'a> {
    state: ValidationState,
    timer: Timer,
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
            state: ValidationState::new(
                target_block_hash,
                max_difficulty,
                blocks_awaiting_validation,
                fallback_block_hash,
            ),
            timer: Timer::new(wait_duration),
            inner,
        }
    }

    /// Waits for a block to be fully validated, monitoring validation progress
    /// Returns the final block hash to use (either the target or fallback)
    pub async fn wait_for_validation(
        &mut self,
        block_state_rx: &mut Receiver<BlockStateUpdated>,
    ) -> eyre::Result<H256> {
        let start_time = Instant::now();

        loop {
            // Handle terminal states first
            let (target_hash, current_awaiting) = match self.state {
                ValidationState::Validated { block_hash } => {
                    let elapsed = start_time.elapsed();
                    info!(
                        block_hash = %block_hash,
                        elapsed_ms = elapsed.as_millis(),
                        "Block validation completed"
                    );
                    return Ok(block_hash);
                }

                ValidationState::TimedOut {
                    fallback_block,
                    abandoned_target,
                } => {
                    return fallback_block.ok_or_else(|| {
                        eyre!(
                            "No validated blocks available (abandoned target: {})",
                            abandoned_target
                        )
                    });
                }

                ref mut state @ ValidationState::Tracking {
                    target,
                    awaiting_validation,
                    fallback,
                } => {
                    let target_hash = target.hash.clone();
                    let awaiting_validation = awaiting_validation;
                    if self.timer.is_expired() {
                        let fallback_block = fallback.clone();
                        let abandoned_target = target_hash;

                        *state = ValidationState::TimedOut {
                            fallback_block,
                            abandoned_target,
                        };

                        if let Some(fb) = fallback_block {
                            warn!(
                                target_block = %abandoned_target,
                                fallback_block = %fb,
                                "Validation timeout - using fallback block"
                            );
                        } else {
                            warn!(
                                target_block = %abandoned_target,
                                "Validation timeout - no fallback available"
                            );
                        }
                        // Will handle in TimedOut arm
                        continue;
                    }

                    (target_hash, awaiting_validation)
                }
            };

            // Get current blockchain state
            let snapshot = self.capture_blockchain_snapshot()?;
            // Check if we can build on the block
            if snapshot.can_build_upon {
                self.state = ValidationState::Validated {
                    block_hash: snapshot.max_block.hash,
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
                    new_target = %snapshot.max_block.hash,
                    new_difficulty = %snapshot.max_block.cumulative_difficulty,
                    "Max difficulty block changed during validation wait"
                );

                // Update to new target
                match &mut self.state {
                    ValidationState::Tracking {
                        target,
                        awaiting_validation,
                        fallback,
                    } => {
                        *target = snapshot.max_block;
                        *awaiting_validation = snapshot.awaiting_validation;
                        *fallback = snapshot.fallback_block;
                    }
                    _ => unreachable!(),
                }

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
                event = block_state_rx.recv() => {
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
        let tree = self.inner.block_tree_guard.read();

        let (max_hash, max_diff) = tree.get_max_cumulative_difficulty_block();
        let (awaiting, fallback) = tree.get_validation_chain_status(&max_hash);
        let can_build = tree.can_be_built_upon(&max_hash);

        Ok(BlockchainSnapshot {
            max_block: TargetBlock {
                hash: max_hash,
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
        fallback: Option<H256>,
    },

    /// Target block is fully validated and ready to use
    Validated { block_hash: H256 },

    /// Timed out waiting for validation
    TimedOut {
        /// The fallback block we'll use (if available)
        fallback_block: Option<H256>,
        /// The block we were waiting for (for logging)
        abandoned_target: H256,
    },
}

/// Information about a target block being tracked
#[derive(Debug, Clone, Copy)]
pub(super) struct TargetBlock {
    pub hash: H256,
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
    fallback_block: Option<H256>,
    can_build_upon: bool,
}

impl ValidationState {
    /// Initial state when starting validation tracking
    pub(super) fn new(
        target: H256,
        difficulty: U256,
        awaiting: usize,
        fallback: Option<H256>,
    ) -> Self {
        Self::Tracking {
            target: TargetBlock {
                hash: target,
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
