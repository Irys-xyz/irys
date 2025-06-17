use crate::block_tree_service::{BlockTreeCache, BlockTreeReadGuard, ChainState};
use futures::future::poll_immediate;
use irys_types::BlockHash;
use priority_queue::PriorityQueue;
use std::cmp::Reverse;
use std::future::Future;
use std::pin::Pin;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum BlockPriority {
    /// Canonical extensions that extend from the canonical tip (highest priority)
    CanonicalExtension(u64),
    /// Canonical blocks already on chain (middle priority)  
    Canonical(u64),
    /// Fork blocks that don't extend the canonical tip (lowest priority)
    Fork(u64),
}

/// Wrapper around active validations with capacity management and priority ordering
pub(crate) struct ActiveValidations {
    /// Priority queue of (block_hash, future) with enum-based priority ordering
    pub(crate) validations: PriorityQueue<BlockHash, Reverse<BlockPriority>>,
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

    /// Calculate the priority for a block based on its chain position and canonical status
    pub(crate) fn calculate_priority(&self, block_hash: &BlockHash) -> Reverse<BlockPriority> {
        let block_tree = self.block_tree_guard.read();

        if let Some((block, chain_state)) = block_tree.get_block_and_status(block_hash) {
            let priority = match chain_state {
                ChainState::Onchain => {
                    // Canonical blocks: middle priority tier
                    BlockPriority::Canonical(block.height)
                }
                ChainState::NotOnchain(_) | ChainState::Validated(_) => {
                    if self.is_canonical_extension(block_hash, &block_tree) {
                        // Canonical extensions: highest priority tier
                        BlockPriority::CanonicalExtension(block.height)
                    } else {
                        // Fork blocks: lowest priority tier
                        BlockPriority::Fork(block.height)
                    }
                }
            };
            Reverse(priority)
        } else {
            // Use Fork with max height for unknown blocks (lowest priority)
            Reverse(BlockPriority::Fork(u64::MAX))
        }
    }

    /// Check if a block is a canonical extension (extends from the canonical tip)
    fn is_canonical_extension(&self, block_hash: &BlockHash, block_tree: &BlockTreeCache) -> bool {
        let (canonical_chain, _) = block_tree.get_canonical_chain();
        let canonical_tip = canonical_chain.last().unwrap().block_hash;

        // Walk up from the block to see if we reach the canonical tip
        let mut current_hash = *block_hash;
        while let Some((block, _)) = block_tree.get_block_and_status(&current_hash) {
            if current_hash == canonical_tip {
                return true;
            }
            current_hash = block.previous_block_hash;

            // Stop if we reach a canonical block (avoid infinite walking)
            if let Some((_, ChainState::Onchain)) = block_tree.get_block_and_status(&current_hash) {
                // Check if this canonical block is the canonical tip
                return current_hash == canonical_tip;
            }
        }
        false
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
    /// returns `true` if any of the block validatoin tasks succeeded
    pub(crate) async fn process_completed(&mut self) -> bool {
        let mut completed_blocks = Vec::new();

        assert_eq!(
            self.validations.len(),
            self.futures.len(),
            "validations and futures out of sync"
        );

        if self.validations.is_empty() {
            return false;
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
        !completed_blocks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_tree_service::test_utils::genesis_tree;
    use crate::block_tree_service::{BlockState, ChainState};
    use futures::future::{pending, ready};
    use irys_types::{IrysBlockHeader, H256};
    use itertools::Itertools;
    use std::collections::HashMap;
    use test_log::test;
    use tokio::time::{sleep, Duration};

    /// Create a mock future that completes immediately
    fn create_ready_future() -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(ready(()))
    }

    /// Create a mock future that never completes
    fn create_pending_future() -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(pending())
    }

    /// Create a mock future that completes after a delay (unused but kept for potential future tests)
    #[allow(dead_code)]
    fn create_delayed_future(delay_ms: u64) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            sleep(Duration::from_millis(delay_ms)).await;
        })
    }

    /// Setup a canonical chain scenario for testing
    fn setup_canonical_chain_scenario(max_height: u64) -> BlockTreeReadGuard {
        let mut blocks = (0..=max_height)
            .map(|height| {
                let mut header = IrysBlockHeader::new_mock_header();
                header.height = height;
                (header, ChainState::Onchain)
            })
            .collect::<Vec<_>>();
        genesis_tree(&mut blocks)
    }

    /// Tests priority ordering with blocks added in sequential height order.
    /// Setup: Canonical chain (0-50), add blocks at heights [10, 20, 30, 40] in order.
    /// Expected: Priority queue returns blocks in same order (lowest height first).
    /// Verifies: Basic priority ordering works correctly with sequential input.
    #[test(tokio::test)]
    async fn test_priority_ordering_sequential_input() {
        // Setup canonical chain with blocks at different heights
        let block_tree_guard = setup_canonical_chain_scenario(50);
        let mut active_validations = ActiveValidations::new(block_tree_guard.clone());

        // Add blocks in sequential order
        let heights = vec![10, 20, 30, 40];
        let mut expected_hashes = Vec::new();

        for &height in &heights {
            let tree = block_tree_guard.read();
            let (chain, _) = tree.get_canonical_chain();
            let block_hash = chain
                .iter()
                .find(|entry| entry.height == height)
                .map(|entry| entry.block_hash)
                .expect("Block should exist");

            expected_hashes.push(block_hash);
            active_validations.push(block_hash, create_pending_future());
        }

        // Verify priority ordering - lower heights should have higher priority
        let mut actual_order = Vec::new();
        while let Some((hash, _priority)) = active_validations.validations.pop() {
            actual_order.push(hash);
        }

        // Should be in order: height 10, 20, 30, 40 (lowest height first)
        assert_eq!(actual_order, expected_hashes);
    }

    /// Tests priority ordering with blocks added in reverse height order.
    /// Setup: Canonical chain (0-50), add blocks at heights [40, 30, 20, 10] in reverse order.
    /// Expected: Priority queue returns blocks in ascending height order [10, 20, 30, 40].
    /// Verifies: Priority ordering is independent of insertion order.
    #[test(tokio::test)]
    async fn test_priority_ordering_reverse_input() {
        // Setup canonical chain
        let block_tree_guard = setup_canonical_chain_scenario(50);
        let mut active_validations = ActiveValidations::new(block_tree_guard.clone());

        // Add blocks in reverse order
        let heights = vec![40, 30, 20, 10];
        let mut block_hashes = Vec::new();

        for &height in &heights {
            let tree = block_tree_guard.read();
            let (chain, _) = tree.get_canonical_chain();
            let block_hash = chain
                .iter()
                .find(|entry| entry.height == height)
                .map(|entry| entry.block_hash)
                .expect("Block should exist");

            block_hashes.push(block_hash);
            active_validations.push(block_hash, create_pending_future());
        }

        // Verify priority ordering - should still be by height regardless of input order
        let mut actual_order = Vec::new();
        while let Some((hash, _priority)) = active_validations.validations.pop() {
            actual_order.push(hash);
        }

        // Should be in order: height 10, 20, 30, 40 (lowest height first)
        // Which corresponds to reverse of input order
        block_hashes.reverse();
        assert_eq!(actual_order, block_hashes);
    }

    /// Tests priority ordering with blocks added in random height order.
    /// Setup: Canonical chain (0-25), add 10 blocks at random heights in arbitrary order.
    /// Expected: Priority queue returns blocks sorted by ascending height.
    /// Verifies: Priority ordering handles random insertion patterns correctly.
    #[test(tokio::test)]
    async fn test_priority_ordering_random_input() {
        // Setup canonical chain
        let block_tree_guard = setup_canonical_chain_scenario(25);
        let mut active_validations = ActiveValidations::new(block_tree_guard.clone());

        // Create blocks at random heights
        let heights = vec![15, 3, 22, 8, 12, 1, 18, 25, 7, 14];
        let mut height_to_hash = HashMap::new();

        // Add blocks in random order
        for &height in &heights {
            let tree = block_tree_guard.read();
            let (chain, _) = tree.get_canonical_chain();
            let block_hash = chain
                .iter()
                .find(|entry| entry.height == height)
                .map(|entry| entry.block_hash)
                .expect("Block should exist");

            height_to_hash.insert(height, block_hash);
            active_validations.push(block_hash, create_pending_future());
        }

        // Verify blocks come out in height order
        let mut sorted_heights = heights.clone();
        sorted_heights.sort();

        let mut actual_order = Vec::new();
        while let Some((hash, _priority)) = active_validations.validations.pop() {
            actual_order.push(hash);
        }

        let expected_order: Vec<BlockHash> = sorted_heights
            .iter()
            .map(|&height| height_to_hash[&height])
            .collect();

        assert_eq!(actual_order, expected_order);
    }

    /// Tests priority ordering with fork blocks vs canonical extensions.
    /// Setup: Canonical chain (0-20), fork blocks (11,12) from block 10, extensions (21,22) from block 20.
    /// Expected: Extensions have higher priority than forks: [21, 22, 11, 12].
    /// Verifies: CanonicalExtension > Fork priority, and height ordering within each type.
    #[test(tokio::test)]
    async fn test_priority_with_fork_scenarios() {
        // Setup scenario with main chain (0-20 canonical)
        let block_tree_guard = setup_canonical_chain_scenario(20);

        // Manually create fork blocks (11, 12) and extension blocks (21, 22)
        let (fork_block_11, fork_block_12, extension_block_21, extension_block_22) = {
            let mut tree = block_tree_guard.write();
            let (canonical_chain, _) = tree.get_canonical_chain();

            // Get block 10 as fork parent and block 20 as extension parent
            let fork_parent = canonical_chain
                .iter()
                .find(|entry| entry.height == 10)
                .expect("Block 10 should exist");
            let extension_parent = canonical_chain
                .iter()
                .find(|entry| entry.height == 20)
                .expect("Block 20 should exist");

            // Create fork block 11 (child of block 10)
            let mut fork_block_11 = IrysBlockHeader::new_mock_header();
            fork_block_11.height = 11;
            fork_block_11.previous_block_hash = fork_parent.block_hash;
            fork_block_11.block_hash = H256::random();
            fork_block_11.cumulative_diff = 50.into(); // Lower than canonical to stay as fork

            // Create fork block 12 (child of fork block 11)
            let mut fork_block_12 = IrysBlockHeader::new_mock_header();
            fork_block_12.height = 12;
            fork_block_12.previous_block_hash = fork_block_11.block_hash;
            fork_block_12.block_hash = H256::random();
            fork_block_12.cumulative_diff = 60.into(); // Lower than canonical to stay as fork

            // Create extension block 21 (child of block 20)
            let mut extension_block_21 = IrysBlockHeader::new_mock_header();
            extension_block_21.height = 21;
            extension_block_21.previous_block_hash = extension_parent.block_hash;
            extension_block_21.block_hash = H256::random();
            extension_block_21.cumulative_diff = 10000.into(); // Higher than canonical for extension

            // Create extension block 22 (child of extension block 21)
            let mut extension_block_22 = IrysBlockHeader::new_mock_header();
            extension_block_22.height = 22;
            extension_block_22.previous_block_hash = extension_block_21.block_hash;
            extension_block_22.block_hash = H256::random();
            extension_block_22.cumulative_diff = 10001.into(); // Higher than canonical for extension

            // Add blocks to tree as NotOnchain
            tree.add_common(
                fork_block_11.block_hash,
                &fork_block_11,
                ChainState::NotOnchain(BlockState::ValidationScheduled),
            )
            .unwrap();
            tree.add_common(
                fork_block_12.block_hash,
                &fork_block_12,
                ChainState::NotOnchain(BlockState::ValidationScheduled),
            )
            .unwrap();
            tree.add_common(
                extension_block_21.block_hash,
                &extension_block_21,
                ChainState::NotOnchain(BlockState::ValidationScheduled),
            )
            .unwrap();
            tree.add_common(
                extension_block_22.block_hash,
                &extension_block_22,
                ChainState::NotOnchain(BlockState::ValidationScheduled),
            )
            .unwrap();

            (
                fork_block_11,
                fork_block_12,
                extension_block_21,
                extension_block_22,
            )
        };

        let mut active_validations = ActiveValidations::new(block_tree_guard.clone());

        // Use the known block hashes from creation
        let fork_blocks = vec![
            (fork_block_11.block_hash, 11),
            (fork_block_12.block_hash, 12),
        ];
        let extension_blocks = vec![
            (extension_block_21.block_hash, 21),
            (extension_block_22.block_hash, 22),
        ];

        // Add blocks to active validations in mixed order to test priority sorting
        for &(hash, _) in &fork_blocks {
            active_validations.push(hash, create_pending_future());
        }
        for &(hash, _) in &extension_blocks {
            active_validations.push(hash, create_pending_future());
        }

        // Verify priority ordering
        let mut actual_order = Vec::new();
        while let Some((hash, priority)) = active_validations.validations.pop() {
            actual_order.push((hash, priority.0));
        }

        // Expected: extensions first (21, 22), then forks (11, 12)
        let expected_order = vec![
            (extension_blocks[0].0, BlockPriority::CanonicalExtension(21)),
            (extension_blocks[1].0, BlockPriority::CanonicalExtension(22)),
            (fork_blocks[0].0, BlockPriority::Fork(11)),
            (fork_blocks[1].0, BlockPriority::Fork(12)),
        ];

        assert_eq!(actual_order.len(), expected_order.len());
        for (i, ((actual_hash, actual_priority), (expected_hash, expected_priority))) in
            actual_order
                .iter()
                .zip_eq(expected_order.iter())
                .enumerate()
        {
            assert_eq!(
                actual_hash, expected_hash,
                "Hash mismatch at position {}",
                i
            );
            assert_eq!(
                actual_priority, expected_priority,
                "Priority mismatch at position {}",
                i
            );
        }
    }

    /// Tests priority ordering with large number of concurrent validations.
    /// Setup: Canonical chain (0-100), add 50 blocks in reverse order to simulate load.
    /// Expected: All blocks returned in ascending height order despite reverse insertion.
    /// Verifies: Priority queue scales correctly with many concurrent validations.
    #[test(tokio::test)]
    async fn test_many_pending_tasks_capacity_management() {
        // Setup large canonical chain
        let block_tree_guard = setup_canonical_chain_scenario(100);
        let mut active_validations = ActiveValidations::new(block_tree_guard.clone());

        // Use the available heights from the chain
        let mut height_to_hash = HashMap::new();
        let heights: Vec<u64>;

        // Collect all block hashes first
        {
            let tree = block_tree_guard.read();
            let (chain, _) = tree.get_canonical_chain();

            // Use the first 50 heights from the actual chain
            heights = chain.iter().take(50).map(|entry| entry.height).collect();

            for entry in chain.iter().take(50) {
                height_to_hash.insert(entry.height, entry.block_hash);
            }
        }

        // Add blocks in shuffled order
        let mut shuffled_heights = heights.clone();
        shuffled_heights.reverse(); // Simple reverse as shuffle

        for &height in &shuffled_heights {
            let block_hash = height_to_hash[&height];
            active_validations.push(block_hash, create_pending_future());
        }

        // Verify all blocks are present
        assert_eq!(active_validations.len(), heights.len());

        // Verify they come out in correct priority order
        let mut actual_order = Vec::new();
        while let Some((hash, _priority)) = active_validations.validations.pop() {
            actual_order.push(hash);
        }

        let expected_order: Vec<BlockHash> = heights
            .iter()
            .map(|&height| height_to_hash[&height])
            .collect();

        assert_eq!(actual_order, expected_order);
    }

    /// Tests that completed validation removal preserves priority ordering.
    /// Setup: Add 4 blocks with alternating ready/pending futures at heights [5, 10, 15, 20].
    /// Expected: Ready futures removed (heights 5, 15), remaining blocks [10, 20] in priority order.
    /// Verifies: process_completed() maintains priority ordering for remaining validations.
    #[test(tokio::test)]
    async fn test_process_completed_preserves_priority() {
        // Setup canonical chain
        let block_tree_guard = setup_canonical_chain_scenario(30);
        let mut active_validations = ActiveValidations::new(block_tree_guard.clone());

        let tree = block_tree_guard.read();
        let (chain, _) = tree.get_canonical_chain();

        // Add mix of ready and pending futures
        let heights = vec![5, 10, 15, 20];
        let mut height_to_hash = HashMap::new();

        for (i, &height) in heights.iter().enumerate() {
            let block_hash = chain
                .iter()
                .find(|entry| entry.height == height)
                .map(|entry| entry.block_hash)
                .expect("Block should exist");

            height_to_hash.insert(height, block_hash);

            // Alternate between ready and pending futures
            let future = if i % 2 == 0 {
                create_ready_future()
            } else {
                create_pending_future()
            };

            active_validations.push(block_hash, future);
        }

        // Process completed validations
        active_validations.process_completed().await;

        // Should have removed the ready futures (heights 5 and 15)
        assert_eq!(active_validations.len(), 2);

        // Remaining blocks should still be in priority order
        let mut remaining_order = Vec::new();
        while let Some((hash, _priority)) = active_validations.validations.pop() {
            remaining_order.push(hash);
        }

        // Should be height 10, then height 20
        assert_eq!(remaining_order[0], height_to_hash[&10]);
        assert_eq!(remaining_order[1], height_to_hash[&20]);
    }

    /// Tests edge cases: empty queue and genesis block handling.
    /// Setup: Empty queue, then add genesis block (height 0).
    /// Expected: Empty operations succeed, genesis gets Canonical(0) priority.
    /// Verifies: Edge cases handled gracefully without panics or errors.
    #[test(tokio::test)]
    async fn test_edge_cases() {
        // Test with empty validation queue
        let block_tree_guard = setup_canonical_chain_scenario(10);
        let mut active_validations = ActiveValidations::new(block_tree_guard.clone());

        assert!(active_validations.is_empty());
        assert_eq!(active_validations.len(), 0);

        // Process completed on empty queue should not panic
        active_validations.process_completed().await;
        assert!(active_validations.is_empty());

        // Test with genesis block
        let tree = block_tree_guard.read();
        let (chain, _) = tree.get_canonical_chain();
        let genesis_hash = chain[0].block_hash;

        active_validations.push(genesis_hash, create_pending_future());

        // Genesis block should have priority based on height 0 and Canonical status
        let priority = active_validations.calculate_priority(&genesis_hash);
        assert_eq!(priority, std::cmp::Reverse(BlockPriority::Canonical(0)));

        assert_eq!(active_validations.len(), 1);
        assert!(!active_validations.is_empty());
    }

    /// Tests BlockPriority enum ordering and Reverse wrapper behavior.
    /// Setup: Compare different BlockPriority variants and heights.
    /// Expected: CanonicalExtension < Canonical < Fork, lower heights < higher heights.
    /// Verifies: Enum derives correct Ord implementation for priority queue usage.
    #[test]
    fn test_block_priority_ordering() {
        // Test that enum variants have correct ordering
        assert!(BlockPriority::CanonicalExtension(10) < BlockPriority::Canonical(5));
        assert!(BlockPriority::Canonical(10) < BlockPriority::Fork(5));
        assert!(BlockPriority::CanonicalExtension(10) < BlockPriority::Fork(5));

        // Test within same variant, lower heights have higher priority
        assert!(BlockPriority::CanonicalExtension(10) < BlockPriority::CanonicalExtension(11));
        assert!(BlockPriority::Canonical(10) < BlockPriority::Canonical(11));
        assert!(BlockPriority::Fork(10) < BlockPriority::Fork(11));

        // Test with Reverse wrapper to ensure priority queue ordering is correct
        assert!(
            Reverse(BlockPriority::CanonicalExtension(10)) > Reverse(BlockPriority::Canonical(10))
        );
        assert!(Reverse(BlockPriority::Canonical(10)) > Reverse(BlockPriority::Fork(10)));
        assert!(Reverse(BlockPriority::CanonicalExtension(10)) > Reverse(BlockPriority::Fork(10)));

        // In a priority queue, lower Reverse values have higher priority
        assert!(
            Reverse(BlockPriority::CanonicalExtension(10))
                > Reverse(BlockPriority::CanonicalExtension(11))
        );
        assert!(Reverse(BlockPriority::Canonical(10)) > Reverse(BlockPriority::Canonical(11)));
        assert!(Reverse(BlockPriority::Fork(10)) > Reverse(BlockPriority::Fork(11)));
    }
}
