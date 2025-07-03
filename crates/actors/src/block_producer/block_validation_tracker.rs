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
        _max_wait_time: Duration,
        block_state_rx: &mut Receiver<BlockStateUpdated>,
    ) -> eyre::Result<H256> {
        let start_time = Instant::now();

        debug!(
            target_block = ?self.state.get_current_target(),
            "Starting validation wait"
        );

        // Early exit if already in terminal state
        if self.state.is_terminal() {
            return self.state.get_result();
        }

        loop {
            // 1. Check for completion conditions
            if let Some(result) = self.check_completion()? {
                let elapsed = start_time.elapsed();
                info!(
                    block_hash = %result,
                    elapsed_ms = elapsed.as_millis(),
                    "Block validation completed"
                );
                return Ok(result);
            }

            // 2. Wait for next event or timeout
            let event = self.wait_for_next_event(block_state_rx).await?;

            // 3. Process the event and get progress info
            let progress = self.process_event(event)?;

            // 4. Update timer based on progress
            self.timer.update_based_on_progress(&progress);

            // 5. Check if we've reached a terminal state
            if self.state.is_terminal() {
                let elapsed = start_time.elapsed();
                let result = self.state.get_result()?;
                info!(
                    block_hash = %result,
                    elapsed_ms = elapsed.as_millis(),
                    "Block validation completed after processing event"
                );
                return Ok(result);
            }
        }
    }

    /// Check if we should complete (timeout or already validated)
    fn check_completion(&mut self) -> eyre::Result<Option<H256>> {
        // Check timeout first
        if self.timer.is_expired() {
            self.state.mark_timed_out()?;

            match &self.state {
                ValidationState::TimedOut {
                    fallback_block,
                    abandoned_target,
                } => {
                    if let Some(fallback) = fallback_block {
                        warn!(
                            target_block = %abandoned_target,
                            fallback_block = %fallback,
                            "Validation timeout - using fallback block"
                        );
                        return Ok(Some(*fallback));
                    } else {
                        warn!(
                            target_block = %abandoned_target,
                            "Validation timeout - no fallback available"
                        );
                        return Err(eyre!("No validated blocks found in the chain"));
                    }
                }
                _ => unreachable!(),
            }
        }

        // Check if current target is already validated
        if let Some(target_hash) = self.state.get_current_target() {
            if self.is_fully_validated(&target_hash)? {
                self.state.mark_validated()?;
                return Ok(Some(target_hash));
            }
        }

        Ok(None)
    }

    /// Wait for next relevant event
    async fn wait_for_next_event(
        &self,
        block_state_rx: &mut Receiver<BlockStateUpdated>,
    ) -> eyre::Result<Option<BlockStateUpdated>> {
        tokio::select! {
            event = block_state_rx.recv() => {
                Ok(Some(event.expect("channel must not close")))
            }
            _ = self.timer.sleep_until_deadline() => {
                trace!("Deadline sleep completed");
                Ok(None) // Timeout occurred
            }
        }
    }

    /// Process a block state event and return progress information
    fn process_event(&mut self, event: Option<BlockStateUpdated>) -> eyre::Result<Progress> {
        let Some(event) = event else {
            return Ok(Progress::none());
        };

        trace!(
            block_hash = %event.block_hash,
            discarded = event.discarded,
            "Processing block state event"
        );

        // Skip discarded blocks
        if event.discarded {
            return Ok(Progress::none());
        }

        // Get current blockchain state in one go
        let snapshot = self.capture_blockchain_snapshot()?;

        // Check if max difficulty block changed
        if let Some(current_target) = self.state.get_current_target() {
            if snapshot.max_block.hash != current_target {
                debug!(
                    old_target = %current_target,
                    new_target = %snapshot.max_block.hash,
                    new_difficulty = %snapshot.max_block.cumulative_difficulty,
                    "Max difficulty block changed during validation wait"
                );

                self.state.update_target(
                    snapshot.max_block,
                    snapshot.awaiting_validation,
                    snapshot.fallback_block,
                )?;

                return Ok(Progress::target_changed());
            }
        }

        // Check validation progress on current target
        self.handle_validation_progress(snapshot)
    }

    /// Handle validation progress on the current target
    fn handle_validation_progress(
        &mut self,
        snapshot: BlockchainSnapshot,
    ) -> eyre::Result<Progress> {
        let current_awaiting = match &self.state {
            ValidationState::Tracking {
                awaiting_validation,
                ..
            } => *awaiting_validation,
            _ => return Ok(Progress::none()),
        };

        let blocks_validated = if snapshot.awaiting_validation < current_awaiting {
            current_awaiting - snapshot.awaiting_validation
        } else {
            0
        };

        if blocks_validated > 0 || snapshot.awaiting_validation == 0 {
            debug!(
                blocks_validated = blocks_validated,
                blocks_remaining = snapshot.awaiting_validation,
                blocks_were = current_awaiting,
                "Validation progress detected"
            );

            self.state
                .update_progress(snapshot.awaiting_validation, snapshot.fallback_block)?;

            let became_fully_validated =
                snapshot.awaiting_validation == 0 && snapshot.can_build_upon;
            Ok(Progress::validation_progress(
                blocks_validated,
                became_fully_validated,
            ))
        } else {
            Ok(Progress::none())
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

    /// Check if a block is fully validated (all ancestors validated)
    fn is_fully_validated(&self, block_hash: &H256) -> eyre::Result<bool> {
        let tree = self.inner.block_tree_guard.read();
        Ok(tree.can_be_built_upon(block_hash))
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
#[derive(Debug, Clone)]
pub(super) struct TargetBlock {
    pub hash: H256,
    pub cumulative_difficulty: U256,
}

/// Tracks validation progress information
#[derive(Debug)]
pub(super) struct Progress {
    pub target_changed: bool,
    pub blocks_validated: usize,
    pub became_fully_validated: bool,
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

    /// Transition: Target block becomes validated
    pub(super) fn mark_validated(&mut self) -> eyre::Result<()> {
        match self {
            Self::Tracking { target, .. } => {
                let block_hash = target.hash;
                *self = Self::Validated { block_hash };
                Ok(())
            }
            _ => Err(eyre!("Cannot mark as validated from state: {:?}", self)),
        }
    }

    /// Transition: Timeout occurred
    pub(super) fn mark_timed_out(&mut self) -> eyre::Result<()> {
        match self {
            Self::Tracking {
                target, fallback, ..
            } => {
                let fallback_block = *fallback;
                let abandoned_target = target.hash;
                *self = Self::TimedOut {
                    fallback_block,
                    abandoned_target,
                };
                Ok(())
            }
            _ => Err(eyre!("Cannot timeout from state: {:?}", self)),
        }
    }

    /// Transition: New max difficulty block appeared
    pub(super) fn update_target(
        &mut self,
        new_target: TargetBlock,
        awaiting: usize,
        fallback: Option<H256>,
    ) -> eyre::Result<()> {
        match self {
            Self::Tracking { .. } => {
                *self = Self::Tracking {
                    target: new_target,
                    awaiting_validation: awaiting,
                    fallback,
                };
                Ok(())
            }
            _ => Err(eyre!("Cannot update target from state: {:?}", self)),
        }
    }

    /// Transition: Validation progress made (some blocks validated)
    pub(super) fn update_progress(
        &mut self,
        new_awaiting: usize,
        new_fallback: Option<H256>,
    ) -> eyre::Result<()> {
        match self {
            Self::Tracking { target, .. } => {
                if new_awaiting == 0 {
                    // All validated!
                    let block_hash = target.hash;
                    *self = Self::Validated { block_hash };
                } else {
                    let target_clone = target.clone();
                    *self = Self::Tracking {
                        target: target_clone,
                        awaiting_validation: new_awaiting,
                        fallback: new_fallback,
                    };
                }
                Ok(())
            }
            _ => Err(eyre!("Cannot update progress from state: {:?}", self)),
        }
    }

    pub(super) fn is_terminal(&self) -> bool {
        matches!(self, Self::Validated { .. } | Self::TimedOut { .. })
    }

    pub(super) fn get_result(&self) -> eyre::Result<H256> {
        match self {
            Self::Validated { block_hash, .. } => Ok(*block_hash),
            Self::TimedOut {
                fallback_block,
                abandoned_target,
            } => fallback_block.ok_or_else(|| {
                eyre!(
                    "No validated blocks available (abandoned target: {})",
                    abandoned_target
                )
            }),
            Self::Tracking { .. } => Err(eyre!("Still tracking, no result available")),
        }
    }

    /// Get the current target block hash if tracking
    pub(super) fn get_current_target(&self) -> Option<H256> {
        match self {
            Self::Tracking { target, .. } => Some(target.hash),
            _ => None,
        }
    }
}

impl Progress {
    fn none() -> Self {
        Self {
            target_changed: false,
            blocks_validated: 0,
            became_fully_validated: false,
        }
    }

    fn target_changed() -> Self {
        Self {
            target_changed: true,
            blocks_validated: 0,
            became_fully_validated: false,
        }
    }

    fn validation_progress(blocks_validated: usize, became_fully_validated: bool) -> Self {
        Self {
            target_changed: false,
            blocks_validated,
            became_fully_validated,
        }
    }

    fn made_progress(&self) -> bool {
        self.target_changed || self.blocks_validated > 0 || self.became_fully_validated
    }
}

impl Timer {
    fn new(duration: Duration) -> Self {
        Self {
            deadline: Instant::now() + duration,
            base_duration: duration,
        }
    }

    pub(super) fn update_based_on_progress(&mut self, progress: &Progress) {
        if progress.made_progress() {
            self.reset();
            trace!(
                "Timer reset due to progress: target_changed={}, blocks_validated={}, became_fully_validated={}",
                progress.target_changed,
                progress.blocks_validated,
                progress.became_fully_validated
            );
        }
    }

    pub(super) fn reset(&mut self) {
        self.deadline = Instant::now() + self.base_duration;
    }

    pub(super) fn is_expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub(super) async fn sleep_until_deadline(&self) {
        tokio::time::sleep_until(self.deadline).await
    }
}
