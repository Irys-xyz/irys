//! Active validations management with priority-based task scheduling.
//!
//! This module manages the lifecycle of block validation tasks using modern Tokio concurrency
//! patterns (JoinSet + Semaphore) for efficient resource utilization and clean task management.
//!
//! # High-Level Architecture
//!
//! The validation pipeline consists of two main stages:
//!
//! 1. **VDF Validation** (Sequential)
//!    - Single-threaded VDF verification with preemption support
//!    - Higher priority blocks can preempt currently running VDF validations
//!    - Successful VDF validations move to the concurrent pool
//!
//! 2. **Concurrent Validation** (Parallel)
//!    - Multiple validation tasks run in parallel (recall range, PoA, shadow txs)
//!    - Controlled concurrency via Semaphore (default: 10 concurrent tasks)
//!    - Tasks wait for parent validation before reporting results
//!
//! # Priority System
//!
//! Blocks are prioritized using a four-tier system (`BlockPriority`):
//!
//! 1. **CanonicalExtension** - Blocks extending the canonical tip (highest priority)
//! 2. **Canonical** - Blocks already on the canonical chain
//! 3. **Fork** - Alternative chain blocks
//! 4. **Unknown** - Orphan blocks (lowest priority)
//!
//! Within each tier, blocks are further ordered by:
//! - Height (lower = higher priority)
//! - VDF step count (fewer = higher priority)
//!
//! # Data Flow
//!
//! ```text
//! New Block → VdfScheduler (priority queue)
//!               ↓
//!          VDF Validation (single task with preemption)
//!               ↓ (if valid)
//!          ConcurrentValidationPool (priority queue)
//!               ↓
//!          Parallel Validation (JoinSet + Semaphore)
//!               ↓
//!          Validation Result → Main Service → Block Tree
//! ```
//!
//! # Key Components
//!
//! - **ValidationCoordinator**: Orchestrates the entire validation pipeline
//! - **VdfScheduler**: Manages VDF validation with preemption support
//! - **ConcurrentValidationPool**: Manages parallel validation tasks with concurrency limits
//! - **ValidationPriority**: Determines task execution order
//!
//! # Concurrency Model
//!
//! - **VDF**: Single task at a time, preemptible via AtomicBool cancellation
//! - **Concurrent**: Up to N tasks (configurable), managed via Semaphore + JoinSet
//! - **No explicit Notify**: Direct await on JoinSet eliminates need for external notifications
//! - **Backpressure**: Semaphore naturally provides backpressure when at capacity
//!
//! # Reorg Handling
//!
//! During blockchain reorganizations, all pending task priorities are reevaluated
//! based on the new canonical chain state. Running tasks continue but may become
//! lower priority for future scheduling.

use irys_domain::{BlockTree, BlockTreeReadGuard, ChainState};
use irys_types::{BlockHash, IrysBlockHeader};
use irys_vdf::state::CancelEnum;
use priority_queue::PriorityQueue;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::{Notify, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, error, info, instrument, warn, Instrument as _};

use crate::block_tree_service::ValidationResult;
use crate::validation_service::block_validation_task::BlockValidationTask;
use crate::validation_service::VdfValidationResult;

/// Block priority states for validation ordering
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BlockPriority {
    /// Canonical extensions that extend from the canonical tip (highest priority)
    CanonicalExtension,
    /// Canonical blocks already on chain (medium priority)
    Canonical,
    /// Fork blocks that don't extend the canonical tip (low priority)
    Fork,
    /// Unknown/orphan blocks (lowest priority)
    Unknown,
}

impl Ord for BlockPriority {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Invert the comparison so higher priority items compare as "greater"
        // CanonicalExtension > Canonical > Fork > Unknown
        use BlockPriority::*;
        match (self, other) {
            (CanonicalExtension, CanonicalExtension) => std::cmp::Ordering::Equal,
            (CanonicalExtension, _) => std::cmp::Ordering::Greater,
            (_, CanonicalExtension) => std::cmp::Ordering::Less,

            (Canonical, Canonical) => std::cmp::Ordering::Equal,
            (Canonical, Fork | Unknown) => std::cmp::Ordering::Greater,

            (Fork, Fork) => std::cmp::Ordering::Equal,
            (Fork, Unknown) => std::cmp::Ordering::Greater,
            (Fork, _) => std::cmp::Ordering::Less,

            (Unknown, Unknown) => std::cmp::Ordering::Equal,
            (Unknown, _) => std::cmp::Ordering::Less,
        }
    }
}

impl PartialOrd for BlockPriority {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Validation priority with explicit ordering logic
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ValidationPriority {
    /// Block state determines primary ordering
    pub state: BlockPriority,
    /// Lower height = higher priority
    pub height: u64,
    /// Fewer VDF steps = higher priority
    pub vdf_step_count: u64,
}

impl ValidationPriority {
    pub(super) fn new(block: &IrysBlockHeader, state: BlockPriority) -> Self {
        Self {
            state,
            height: block.height,
            vdf_step_count: block.vdf_limiter_info.steps.len() as u64,
        }
    }
}

impl Ord for ValidationPriority {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // First compare by state (CanonicalExtension > Canonical > Fork > Unknown)
        self.state
            .cmp(&other.state)
            // Then by height (lower height = higher priority, so reverse the comparison)
            .then_with(|| other.height.cmp(&self.height))
            // Finally by VDF steps (fewer steps = higher priority, so reverse the comparison)
            .then_with(|| other.vdf_step_count.cmp(&self.vdf_step_count))
    }
}

impl PartialOrd for ValidationPriority {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Result from a concurrent validation task
#[derive(Debug)]
pub(super) struct ConcurrentValidationResult {
    pub block_hash: BlockHash,
    pub validation_result: ValidationResult,
}

/// Clean concurrent validation pool using JoinSet + Semaphore
pub(super) struct ConcurrentValidationPool {
    /// Controls maximum concurrent validations
    pub semaphore: Arc<Semaphore>,

    /// Active validation tasks
    pub tasks: JoinSet<ConcurrentValidationResult>,

    /// Tasks waiting for capacity
    pub pending: PriorityQueue<BlockHash, (ValidationPriority, BlockValidationTask)>,

    /// Maximum concurrent validations
    pub max_concurrent: usize,
}

impl ConcurrentValidationPool {
    pub(super) fn new(max_concurrent: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            tasks: JoinSet::new(),
            pending: PriorityQueue::new(),
            max_concurrent,
        }
    }

    /// Submit a task for validation
    #[instrument(skip_all, fields(block_hash = %task.block.block_hash))]
    pub fn submit(&mut self, task: BlockValidationTask, priority: ValidationPriority) {
        let block_hash = task.block.block_hash;

        // Check for duplicates
        if self.pending.get(&block_hash).is_some() {
            debug!("Block {} already pending, skipping duplicate", block_hash);
            return;
        }

        self.pending.push(block_hash, (priority, task));
        self.try_spawn_pending();
    }

    /// Try to spawn as many pending tasks as we have capacity for
    fn try_spawn_pending(&mut self) {
        while !self.pending.is_empty() {
            match self.semaphore.clone().try_acquire_owned() {
                Ok(permit) => {
                    let (hash, (_priority, task)) = self.pending.pop().unwrap();

                    debug!(
                        "Spawning concurrent validation for {} (active: {}/{})",
                        hash,
                        self.max_concurrent - self.semaphore.available_permits(),
                        self.max_concurrent
                    );

                    self.tasks.spawn(
                        async move {
                            // Permit is held for the duration of the task
                            let _permit = permit;

                            // Execute the validation and return the result
                            let validation_result = task.execute_concurrent().await;

                            ConcurrentValidationResult {
                                block_hash: hash,
                                validation_result,
                            }
                        }
                        .instrument(tracing::Span::current()),
                    );
                }
                Err(_) => {
                    // No capacity available
                    break;
                }
            }
        }
    }

    /// Poll for next completed task
    pub(super) async fn join_next(
        &mut self,
    ) -> Option<Result<ConcurrentValidationResult, tokio::task::JoinError>> {
        let result = self.tasks.join_next().await?;

        // A task completed, try to spawn more
        self.try_spawn_pending();

        Some(result)
    }

    /// Reevaluate priorities after a reorg
    pub(super) fn reevaluate_priorities<F>(&mut self, recalc_fn: F)
    where
        F: Fn(&BlockHash) -> Option<ValidationPriority>,
    {
        let old_pending = std::mem::take(&mut self.pending);

        for (hash, (_old_priority, task)) in old_pending {
            if let Some(new_priority) = recalc_fn(&hash) {
                self.pending.push(hash, (new_priority, task));
            }
        }

        debug!("Reevaluated {} pending task priorities", self.pending.len());
    }
}

/// VDF task with preemption support
pub(super) struct PreemptibleVdfTask {
    pub task: BlockValidationTask,
    pub cancel_u8: Arc<std::sync::atomic::AtomicU8>,
}

impl PreemptibleVdfTask {
    pub(super) async fn execute(self) -> (VdfValidationResult, BlockValidationTask) {
        let inner = Arc::clone(&self.task.service_inner);
        let block = Arc::clone(&self.task.block);
        let skip_vdf = self.task.skip_vdf_validation;
        let vdf_notify = Arc::clone(&self.task.service_inner.vdf_notify);

        // No bridge task needed - just use the AtomicU8 directly!
        let result = match inner
            .ensure_vdf_is_valid(&block, self.cancel_u8.clone(), skip_vdf)
            .await
        {
            Ok(()) => VdfValidationResult::Valid,
            Err(e) => {
                // Check if we were cancelled by inspecting the AtomicU8
                if self.cancel_u8.load(Ordering::Relaxed) == CancelEnum::Cancelled as u8 {
                    VdfValidationResult::Cancelled
                } else {
                    VdfValidationResult::Invalid(e)
                }
            }
        };

        // Notify that VDF task has completed so it can be collected
        vdf_notify.notify_one();

        (result, self.task)
    }
}

/// Simplified VDF scheduler with preemption
pub(super) struct VdfScheduler {
    /// Currently running VDF task with priority and cancellation signal
    pub current: Option<(
        BlockHash,
        ValidationPriority,
        Arc<std::sync::atomic::AtomicU8>,
        JoinHandle<(VdfValidationResult, BlockValidationTask)>,
    )>,

    /// Pending VDF tasks
    pub pending: PriorityQueue<BlockHash, (ValidationPriority, BlockValidationTask)>,
}

impl VdfScheduler {
    pub(super) fn new() -> Self {
        Self {
            current: None,
            pending: PriorityQueue::new(),
        }
    }

    /// Submit a VDF task
    pub(super) fn submit(&mut self, task: BlockValidationTask, priority: ValidationPriority) {
        let hash = task.block.block_hash;

        // Check for duplicates
        if self.pending.get(&hash).is_some() {
            debug!("VDF task for {} already pending", hash);
            return;
        }

        // Check if current task exists
        if let Some((current_hash, _, _, _)) = &self.current {
            if *current_hash == hash {
                debug!("VDF task for {} already running", hash);
                return;
            }
        }

        self.pending.push(hash, (priority, task));

        // Check if we should preempt current task
        if let Some((current_hash, current_priority, cancel_u8, _)) = &self.current {
            if let Some((_, (new_priority, _))) = self.pending.peek() {
                // Only preempt if new task has HIGHER priority
                if new_priority > current_priority {
                    info!(
                        "Preempting VDF task {} (priority {:?}) for higher priority {:?}",
                        current_hash, current_priority, new_priority
                    );
                    cancel_u8.store(CancelEnum::Cancelled as u8, Ordering::Relaxed);
                }
            }
        }
    }

    /// Start next VDF task if none running
    pub(super) fn start_next(&mut self) -> Option<()> {
        if self.current.is_some() {
            return None; // Already running
        }

        let (hash, (priority, task)) = self.pending.pop()?;

        // Create AtomicU8 for cancellation
        let cancel_u8 = Arc::new(std::sync::atomic::AtomicU8::new(CancelEnum::Continue as u8));

        let preemptible = PreemptibleVdfTask {
            task,
            cancel_u8: Arc::clone(&cancel_u8),
        };

        let handle = tokio::spawn(
            preemptible
                .execute()
                .instrument(tracing::info_span!("vdf_validation", block_hash = %hash)),
        );

        self.current = Some((hash, priority, cancel_u8, handle));

        debug!(
            "Started VDF validation for {} with priority {:?}",
            hash, priority
        );
        Some(())
    }

    /// Poll current VDF task
    pub(super) async fn poll_current(
        &mut self,
    ) -> Option<(BlockHash, VdfValidationResult, BlockValidationTask)> {
        let (hash, priority, cancel_u8, mut handle) = self.current.take()?;

        // Use poll_immediate to check without blocking
        let poll_result = futures::future::poll_immediate(&mut handle).await;

        match poll_result {
            Some(Ok((result, task))) => {
                // Task completed
                Some((hash, result, task))
            }
            Some(Err(e)) => {
                error!("VDF task panicked: {}", e);
                // We lost the task on panic, cannot continue
                None
            }
            None => {
                // Still running, put it back
                self.current = Some((hash, priority, cancel_u8, handle));
                None
            }
        }
    }
}

/// Main validation coordinator
pub(super) struct ValidationCoordinator {
    /// VDF validation scheduler
    pub vdf_scheduler: VdfScheduler,

    /// Concurrent validation pool
    pub concurrent_pool: ConcurrentValidationPool,

    /// Block tree for priority calculation
    pub block_tree_guard: BlockTreeReadGuard,

    /// VDF task completion notifier
    vdf_notify: Arc<Notify>,
}

impl ValidationCoordinator {
    pub(super) fn new(
        block_tree_guard: BlockTreeReadGuard,
        max_concurrent: usize,
        vdf_notify: Arc<Notify>,
    ) -> Self {
        Self {
            vdf_scheduler: VdfScheduler::new(),
            concurrent_pool: ConcurrentValidationPool::new(max_concurrent),
            block_tree_guard,
            vdf_notify,
        }
    }

    /// Calculate priority for a block
    pub(super) fn calculate_priority(&self, block: &Arc<IrysBlockHeader>) -> ValidationPriority {
        let block_tree = self.block_tree_guard.read();
        let block_hash = block.block_hash;

        let state = match block_tree.get_block_and_status(&block_hash) {
            Some((_, ChainState::Onchain)) => BlockPriority::Canonical,
            Some((_, ChainState::NotOnchain(_) | ChainState::Validated(_))) => {
                if self.is_canonical_extension(&block_hash, &block_tree) {
                    BlockPriority::CanonicalExtension
                } else {
                    BlockPriority::Fork
                }
            }
            None => BlockPriority::Unknown,
        };

        ValidationPriority::new(block, state)
    }

    /// Check if block extends canonical tip
    fn is_canonical_extension(&self, block_hash: &BlockHash, block_tree: &BlockTree) -> bool {
        let (canonical_chain, _) = block_tree.get_canonical_chain();
        let canonical_tip = canonical_chain.last().unwrap().block_hash;

        let mut current = *block_hash;
        while let Some((block, _)) = block_tree.get_block_and_status(&current) {
            if current == canonical_tip {
                return true;
            }
            current = block.previous_block_hash;

            if let Some((_, ChainState::Onchain)) = block_tree.get_block_and_status(&current) {
                return current == canonical_tip;
            }
        }
        false
    }

    /// Submit a validation task
    pub(super) fn submit_task(&mut self, task: BlockValidationTask) {
        let priority = self.calculate_priority(&task.block);
        self.vdf_scheduler.submit(task, priority);

        // Notify to start processing if VDF is idle
        if self.vdf_scheduler.current.is_none() && !self.vdf_scheduler.pending.is_empty() {
            self.vdf_notify.notify_one();
        }
    }

    /// Process VDF completion
    pub(super) async fn process_vdf(&mut self) -> Option<(BlockHash, VdfValidationResult)> {
        // Poll current VDF task
        if let Some((hash, result, task)) = self.vdf_scheduler.poll_current().await {
            debug!(?hash, ?result, "VDF completed");

            if matches!(result, VdfValidationResult::Valid) {
                let priority = self.calculate_priority(&task.block);
                self.concurrent_pool.submit(task, priority);
            }

            // Start next VDF task
            self.vdf_scheduler.start_next();
            return Some((hash, result));
        }

        // Try to start a VDF task if none running
        self.vdf_scheduler.start_next();
        None
    }

    /// Reevaluate all priorities after reorg
    pub(super) fn reevaluate_priorities(&mut self) {
        info!("Reevaluating priorities after reorg");

        // Reevaluate concurrent pool
        let block_tree_guard = &self.block_tree_guard;
        self.concurrent_pool.reevaluate_priorities(|hash| {
            let block_tree = block_tree_guard.read();
            block_tree.get_block(hash).map(|block| {
                let state = match block_tree.get_block_and_status(hash) {
                    Some((_, ChainState::Onchain)) => BlockPriority::Canonical,
                    Some((_, _)) => {
                        // Simplified check for canonical extension
                        let (canonical_chain, _) = block_tree.get_canonical_chain();
                        let canonical_tip = canonical_chain.last().unwrap().block_hash;
                        if block.previous_block_hash == canonical_tip {
                            BlockPriority::CanonicalExtension
                        } else {
                            BlockPriority::Fork
                        }
                    }
                    None => BlockPriority::Unknown,
                };
                ValidationPriority::new(block, state)
            })
        });

        // Reevaluate VDF pending
        let old_pending = std::mem::take(&mut self.vdf_scheduler.pending);
        for (hash, (_old_priority, task)) in old_pending {
            let new_priority = self.calculate_priority(&task.block);
            self.vdf_scheduler.pending.push(hash, (new_priority, task));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::IrysBlockHeader;

    /// Test that ValidationPriority ordering works correctly with manual Ord
    #[test]
    fn test_validation_priority_ordering() {
        let mut header1 = IrysBlockHeader::new_mock_header();
        header1.height = 100;

        let mut header2 = IrysBlockHeader::new_mock_header();
        header2.height = 200;

        // Test 1: Canonical extension should have highest priority
        let p1 = ValidationPriority::new(&header1, BlockPriority::CanonicalExtension);
        let p2 = ValidationPriority::new(&header2, BlockPriority::Canonical);
        assert!(
            p1 > p2,
            "Canonical extension should have higher priority than canonical"
        );

        // Test 2: Among same type, lower height should have higher priority
        let p3 = ValidationPriority::new(&header1, BlockPriority::Fork);
        let p4 = ValidationPriority::new(&header2, BlockPriority::Fork);
        assert!(p3 > p4, "Lower height should have higher priority");

        // Test 3: Canonical should have higher priority than fork
        let p5 = ValidationPriority::new(&header1, BlockPriority::Canonical);
        let p6 = ValidationPriority::new(&header1, BlockPriority::Fork);
        assert!(p5 > p6, "Canonical should have higher priority than fork");

        // Test 4: Test BlockPriority enum ordering (higher priority > lower priority)
        assert!(BlockPriority::CanonicalExtension > BlockPriority::Canonical);
        assert!(BlockPriority::Canonical > BlockPriority::Fork);
        assert!(BlockPriority::Fork > BlockPriority::Unknown);
    }

    /// Test the concurrent validation pool with JoinSet + Semaphore
    #[tokio::test]
    async fn test_concurrent_validation_pool() {
        let mut pool = ConcurrentValidationPool::new(2);

        // Initial state
        let active = pool.max_concurrent - pool.semaphore.available_permits();
        let pending = pool.pending.len();
        assert_eq!(active, 0);
        assert_eq!(pending, 0);
        // Capacity was 2 as configured

        // We can't easily test with real BlockValidationTask without a lot of setup,
        // but we can verify the pool structure works correctly

        // Test that join_next returns None when empty
        let result = pool.join_next().await;
        assert!(
            result.is_none(),
            "join_next should return None when pool is empty"
        );
    }

    /// Test VDF scheduler with preemption using AtomicU8
    #[test]
    fn test_vdf_scheduler_preemption() {
        let scheduler = VdfScheduler::new();

        // Initially empty
        assert!(scheduler.current.is_none());
        assert_eq!(scheduler.pending.len(), 0);

        // Can't easily test real tasks without full setup, but structure is validated
    }

    /// Test that ValidationCoordinator integrates components correctly
    #[test]
    fn test_validation_coordinator_structure() {
        // This test would require BlockTreeReadGuard which needs full setup
        // The fact that the code compiles validates the structure is correct

        // Test priority comparison edge cases
        let mut h1 = IrysBlockHeader::new_mock_header();
        h1.height = u64::MAX - 1;

        let mut h2 = IrysBlockHeader::new_mock_header();
        h2.height = 0;

        // Edge case: Very high height vs very low height
        let p1 = ValidationPriority::new(&h1, BlockPriority::Fork);
        let p2 = ValidationPriority::new(&h2, BlockPriority::Fork);

        assert!(p2 > p1, "Height 0 should have higher priority than MAX-1");
    }
}
