use std::{
    sync::{Arc, RwLock},
    time::SystemTime,
};

use irys_types::{storage_pricing::TOKEN_SCALE, Config, IrysBlockHeader, IrysTokenPrice, H256};
use reth::tasks::{TaskExecutor, TaskManager};
use rust_decimal::Decimal;

use crate::{
    ema_service::{EmaServiceMessage, NewBlockEmaResponse, PriceStatus},
    services::{ServiceReceivers, ServiceSenders},
};

use super::{BlockTreeCache, BlockTreeReadGuard, ChainState, ReorgEvent};

pub fn build_genesis_tree_with_n_blocks(
    max_block_height: u64,
) -> (BlockTreeReadGuard, Vec<PriceInfo>) {
    let (blocks, prices) = build_tree(0, max_block_height);
    let mut blocks = blocks
        .into_iter()
        .map(|block| (block, ChainState::Onchain))
        .collect::<Vec<_>>();
    assert_eq!(blocks.last().unwrap().0.height, max_block_height);
    (genesis_tree(&mut blocks), prices)
}

fn build_tree(init_height: u64, max_height: u64) -> (Vec<IrysBlockHeader>, Vec<PriceInfo>) {
    let blocks = (init_height..(max_height + init_height).saturating_add(1))
        .map(|height| {
            let mut header = IrysBlockHeader::new_mock_header();
            header.height = height;
            // add a random constant to the price to differentiate it from the height
            header.oracle_irys_price = deterministic_price(height);
            header.ema_irys_price = deterministic_price(height);
            header
        })
        .collect::<Vec<_>>();
    let prices = blocks
        .iter()
        .map(|block| PriceInfo {
            oracle: block.oracle_irys_price,
            ema: block.ema_irys_price,
        })
        .collect::<Vec<_>>();
    (blocks, prices)
}

pub fn deterministic_price(height: u64) -> IrysTokenPrice {
    let amount = TOKEN_SCALE + IrysTokenPrice::token(Decimal::from(height)).unwrap().amount;

    IrysTokenPrice::new(amount)
}

pub fn genesis_tree(blocks: &mut [(IrysBlockHeader, ChainState)]) -> BlockTreeReadGuard {
    let mut block_hash = if blocks[0].0.block_hash == H256::default() {
        H256::random()
    } else {
        blocks[0].0.block_hash
    };
    let mut iter = blocks.iter_mut();
    let genesis_block = &mut (iter.next().unwrap()).0;
    genesis_block.block_hash = block_hash;
    genesis_block.cumulative_diff = 0.into();

    let mut block_tree_cache = BlockTreeCache::new(genesis_block);
    block_tree_cache.mark_tip(&block_hash).unwrap();
    for (block, state) in iter {
        block.previous_block_hash = block_hash;
        block.cumulative_diff = block.height.into();
        if block.block_hash == H256::default() {
            block_hash = H256::random();
            block.block_hash = block_hash;
        } else {
            block_hash = block.block_hash;
        }
        block_tree_cache
            .add_common(block.block_hash, block, *state)
            .unwrap();
    }
    let block_tree_cache = Arc::new(RwLock::new(block_tree_cache));
    BlockTreeReadGuard::new(block_tree_cache)
}

#[expect(
    dead_code,
    reason = "structs are held in-memory to prevent the `drop` to trigger"
)]
struct TestCtx {
    pub guard: BlockTreeReadGuard,
    pub config: Config,
    pub task_manager: TaskManager,
    pub task_executor: TaskExecutor,
    pub service_senders: ServiceSenders,
    pub prices: Vec<PriceInfo>,
}

#[derive(Debug, Clone)]
pub struct PriceInfo {
    oracle: IrysTokenPrice,
    ema: IrysTokenPrice,
}

impl TestCtx {
    fn setup(max_block_height: u64, config: Config) -> (Self, ServiceReceivers) {
        let (block_tree_guard, prices) = build_genesis_tree_with_n_blocks(max_block_height);
        Self::setup_with_tree(block_tree_guard, prices, config)
    }

    async fn get_prices_for_new_block(
        &self,
        height_of_new_block: u64,
        new_oracle_price: IrysTokenPrice,
    ) -> eyre::Result<NewBlockEmaResponse> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.service_senders
            .ema
            .send(EmaServiceMessage::GetPriceDataForNewBlock {
                height_of_new_block,
                response: tx,
                oracle_price: new_oracle_price,
            })
            .unwrap();
        rx.await.unwrap()
    }

    async fn validate_ema_price(
        &self,
        block_height: u64,
        ema_price: IrysTokenPrice,
        oracle_price: IrysTokenPrice,
    ) -> eyre::Result<PriceStatus> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.service_senders
            .ema
            .send(EmaServiceMessage::ValidateEmaPrice {
                response: tx,
                block_height,
                ema_price,
                oracle_price,
            })
            .unwrap();
        rx.await.unwrap()
    }

    async fn validate_oracle_price(
        &self,
        block_height: u64,
        oracle_price: IrysTokenPrice,
    ) -> eyre::Result<PriceStatus> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.service_senders
            .ema
            .send(EmaServiceMessage::ValidateOraclePrice {
                response: tx,
                block_height,
                oracle_price,
            })
            .unwrap();
        rx.await.unwrap()
    }

    fn setup_with_tree(
        block_tree_guard: BlockTreeReadGuard,
        prices: Vec<PriceInfo>,
        config: Config,
    ) -> (Self, ServiceReceivers) {
        let task_manager = TaskManager::new(tokio::runtime::Handle::current());
        let task_executor = task_manager.executor();
        let (service_senders, service_rx) = ServiceSenders::new();

        (
            Self {
                guard: block_tree_guard,
                config,
                task_manager,
                service_senders,
                task_executor,
                prices,
            },
            service_rx,
        )
    }

    async fn trigger_reorg(&self, event: ReorgEvent) {
        // Trigger reorg using the provided service senders
        let _ = self.service_senders.reorg_events.send(event);
        // Give the service time to process the reorg
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
}

/// Creates a clean chain state without any forks for testing
fn setup_chain_for_fork_test(max_height: u64) -> (BlockTreeReadGuard, Vec<PriceInfo>) {
    let mut blocks = Vec::new();
    let mut prices = Vec::new();

    // Build linear chain without any forks
    let mut last_hash = H256::default();
    for height in 0..=max_height {
        let mut header = IrysBlockHeader::new_mock_header();
        header.height = height;
        header.oracle_irys_price = deterministic_price(height);
        header.ema_irys_price = deterministic_price(height);
        header.block_hash = H256::random();
        header.previous_block_hash = last_hash;
        last_hash = header.block_hash;

        prices.push(PriceInfo {
            oracle: header.oracle_irys_price,
            ema: header.ema_irys_price,
        });

        blocks.push((header, ChainState::Onchain));
    }

    let block_tree_guard = genesis_tree(&mut blocks);
    (block_tree_guard, prices)
}

/// Adds fork to an existing block tree
/// Returns (ReorgEvent, Vec<PriceInfo>) where the Vec contains all prices from genesis to the tip of the new fork
pub fn create_and_apply_fork(
    block_tree_guard: &BlockTreeReadGuard,
    fork_height: u64,
    common_ancestor_height: u64,
    chain_state: ChainState,
) -> (ReorgEvent, Vec<PriceInfo>) {
    assert!(common_ancestor_height < fork_height);

    // Get the fork parent block
    let fork_parent = {
        let tree = block_tree_guard.read();
        let (chain, _) = tree.get_canonical_chain();
        let fork_parent_hash = chain
            .iter()
            .find(|entry| entry.height == common_ancestor_height)
            .map(|entry| entry.block_hash)
            .expect("Fork height should exist in chain");

        let block = tree.get_block(&fork_parent_hash).unwrap();
        Arc::new(block.clone())
    };

    let fork_parent_hash = fork_parent.block_hash;

    // Collect all prices from genesis to fork point first
    let mut fork_prices = Vec::new();
    {
        let tree = block_tree_guard.read();
        let (chain, _) = tree.get_canonical_chain();

        // Collect prices from genesis (height 0) up to and including the fork point
        for entry in chain.iter() {
            if entry.height <= common_ancestor_height {
                let block = tree.get_block(&entry.block_hash).unwrap();
                fork_prices.push(PriceInfo {
                    oracle: block.oracle_irys_price,
                    ema: block.ema_irys_price,
                });
            }
        }
    }

    // Create new fork blocks with higher difficulty
    let mut new_fork_blocks = Vec::new();
    let mut last_hash = fork_parent_hash;

    {
        let mut tree = block_tree_guard.write();
        for height in (common_ancestor_height + 1)..=fork_height {
            let mut header = IrysBlockHeader::new_mock_header();
            header.height = height;
            header.previous_block_hash = last_hash;
            // Use different prices to distinguish from old fork
            header.oracle_irys_price = deterministic_price(header.height + 100);
            header.ema_irys_price = deterministic_price(header.height + 100);
            header.block_hash = H256::random();
            // Much higher difficulty to ensure it becomes canonical
            header.cumulative_diff = (height + fork_height).into();
            last_hash = header.block_hash;

            // Collect the price info for this fork block
            fork_prices.push(PriceInfo {
                oracle: header.oracle_irys_price,
                ema: header.ema_irys_price,
            });

            // Add to block tree as validated but not yet canonical
            tree.add_common(header.block_hash, &header, chain_state)
                .unwrap();

            new_fork_blocks.push(Arc::new(header));
        }

        // Mark the new tip as the canonical chain
        let is_new_tip = tree
            .mark_tip(&new_fork_blocks.last().unwrap().block_hash)
            .unwrap();
        assert!(is_new_tip);
    }

    let new_tip = new_fork_blocks.last().unwrap().block_hash;

    let data = block_tree_guard.read();
    assert_eq!(
        data.get_canonical_chain().0.len(),
        (fork_height + 1) as usize
    );

    // Create reorg event
    let reorg_event = ReorgEvent {
        // todo: write data here
        old_fork: Arc::new(vec![]),
        // todo: write data here
        new_fork: Arc::new(vec![]),
        fork_parent,
        new_tip,
        timestamp: SystemTime::now(),
    };

    (reorg_event, fork_prices)
}
