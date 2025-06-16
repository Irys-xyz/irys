use crate::block_tree_service::{BlockTreeReadGuard, ChainState};
use futures::future::poll_immediate;
use irys_types::BlockHash;
use priority_queue::PriorityQueue;
use std::cmp::Reverse;
use std::future::Future;
use std::pin::Pin;

/// Wrapper around active validations with capacity management and priority ordering
pub(crate) struct ActiveValidations {
    /// Priority queue of (block_hash, future) with priority based on canonical chain distance
    pub(crate) validations: PriorityQueue<BlockHash, Reverse<u64>>,
    /// Map from block hash to the actual future
    pub(crate) futures:
        std::collections::HashMap<BlockHash, Pin<Box<dyn Future<Output = ()> + Send>>>,
    pub(crate) block_tree_guard: BlockTreeReadGuard,
}

impl ActiveValidations {
    pub(crate) fn new(block_tree_guard: BlockTreeReadGuard) -> Self {
        Self {
            validations: PriorityQueue::new(),
            futures: std::collections::HashMap::new(),
            block_tree_guard,
        }
    }

    /// Calculate the priority for a block based on its nearest canonical chain ancestor
    pub(crate) fn calculate_priority(&self, block_hash: &BlockHash) -> Reverse<u64> {
        let block_tree = self.block_tree_guard.read();

        let mut current_hash = *block_hash;
        let mut current_height = 0u64;

        // Walk up the chain to find the nearest canonical (Onchain) ancestor
        while let Some((block, chain_state)) = block_tree.get_block_and_status(&current_hash) {
            current_height = block.height;

            // Check if this block is on the canonical chain
            if matches!(chain_state, ChainState::Onchain) {
                break;
            }

            // Move to parent block
            current_hash = block.previous_block_hash;

            // Safety check to prevent infinite loops
            if block.height == 0 {
                break;
            }
        }

        // Lower heights get higher priority (processed first)
        Reverse(current_height)
    }

    pub(crate) fn push(
        &mut self,
        block_hash: BlockHash,
        future: Pin<Box<dyn Future<Output = ()> + Send>>,
    ) {
        let priority = self.calculate_priority(&block_hash);
        self.futures.insert(block_hash, future);
        self.validations.push(block_hash, priority);
    }

    pub(crate) fn len(&self) -> usize {
        self.validations.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.validations.is_empty()
    }

    /// Process completed validations and remove them from the active set
    pub(crate) async fn process_completed(&mut self) {
        let mut completed_blocks = Vec::new();

        assert_eq!(
            self.validations.len(),
            self.futures.len(),
            "validations and futures out of sync"
        );

        if self.validations.is_empty() {
            return;
        }

        // Check futures in priority order using poll_immediate for non-blocking check
        for (block_hash, _priority) in self.validations.clone().iter() {
            if let Some(future) = self.futures.get_mut(block_hash) {
                // Use poll_immediate to check if future is ready without blocking
                if poll_immediate(future).await.is_some() {
                    completed_blocks.push(*block_hash);
                }
            }
        }

        // Remove completed validations
        for block_hash in &completed_blocks {
            self.validations.remove(block_hash);
            self.futures.remove(block_hash);
        }
    }
}
