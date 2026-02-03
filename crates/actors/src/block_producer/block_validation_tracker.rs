//! Block validation tracking for parent selection in block production.
//!
//! # Problem
//! Block producer must select the block with highest cumulative difficulty as parent,
//! but only if that block and all its ancestors are fully validated. Validation is
//! asynchronous and lags behind block discovery.
//!
//! # Solution
//! Wait indefinitely for validation while monitoring for new target blocks with higher
//! cumulative difficulty. Switch targets when a better block appears.
//!
//! # Algorithm
//! 1. Identify block with maximum cumulative difficulty
//! 2. Check if fully validated (can be built upon)
//! 3. If not validated:
//!    - Monitor block state updates
//!    - Switch to new max-difficulty block if one appears
//! 4. Return block hash when validation completes

use crate::{block_tree_service::BlockStateUpdated, services::ServiceSenders};
use eyre::bail;
use irys_domain::BlockTreeReadGuard;
use irys_types::BlockHash;
use tokio::sync::broadcast::{self, Receiver};
use tracing::{debug, info, instrument, trace, warn};

/// Tracks the state of block validation during parent block selection
pub struct BlockValidationTracker {
    state: ValidationState,
    block_tree_guard: BlockTreeReadGuard,
    block_state_rx: Receiver<BlockStateUpdated>,
}

impl BlockValidationTracker {
    /// Creates a new tracker that automatically finds the highest cumulative difficulty block
    pub fn new(block_tree_guard: BlockTreeReadGuard, service_senders: ServiceSenders) -> Self {
        // Subscribe to block state updates
        let block_state_rx = service_senders.subscribe_block_state_updates();

        // Get initial blockchain state
        let (target_hash, target_height) = {
            let tree = block_tree_guard.read();
            let (hash, height, _) = tree.get_max_block_info();
            (hash, height)
        };

        Self {
            state: ValidationState::Tracking {
                target_hash,
                target_height,
            },
            block_tree_guard,
            block_state_rx,
        }
    }

    /// Waits for a block to be fully validated, monitoring for target changes.
    /// Returns the validated block hash to use as parent.
    #[instrument(skip_all)]
    pub async fn wait_for_validation(&mut self) -> eyre::Result<BlockHash> {
        loop {
            // Extract current target (or return if already validated)
            let (target_hash, target_height) = match &self.state {
                ValidationState::Validated {
                    block_hash,
                    block_height,
                } => {
                    info!(
                        block.hash = %block_hash,
                        block.height = block_height,
                        "Validation completed"
                    );
                    return Ok(*block_hash);
                }
                ValidationState::Tracking {
                    target_hash,
                    target_height,
                    ..
                } => (*target_hash, *target_height),
            };

            // Check current blockchain state
            let (max_hash, max_height, can_build) = {
                let tree = self.block_tree_guard.read();
                tree.get_max_block_info()
            };

            // If best block is validated, done
            if can_build {
                self.state = ValidationState::Validated {
                    block_hash: max_hash,
                    block_height: max_height,
                };
                continue;
            }

            // Switch target if max-difficulty block changed
            if max_hash != target_hash {
                debug!(
                    block_validation.old_target = %target_hash,
                    block_validation.old_height = target_height,
                    block_validation.new_target = %max_hash,
                    block_validation.new_height = max_height,
                    "Target block changed during validation wait"
                );
                self.state = ValidationState::Tracking {
                    target_hash: max_hash,
                    target_height: max_height,
                };
            }

            // Wait for next event
            match self.block_state_rx.recv().await {
                Ok(_) => trace!("Received BlockStateUpdated"),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "Events lagged")
                }
                Err(broadcast::error::RecvError::Closed) => bail!("Block state channel closed"),
            }
        }
    }
}

/// Represents the current state of block validation tracking
#[derive(Debug, Clone)]
pub(super) enum ValidationState {
    /// Tracking a target block that needs validation
    Tracking {
        target_hash: BlockHash,
        target_height: u64,
    },

    /// Target block is fully validated and ready
    Validated {
        block_hash: BlockHash,
        block_height: u64,
    },
}
