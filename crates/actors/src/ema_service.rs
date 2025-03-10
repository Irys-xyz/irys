use std::pin::pin;

use futures::join;
use irys_types::{
    is_ema_recalculation_block, previous_ema_recalculation_block_height, Config, IrysBlockHeader,
    IrysTokenPrice, H256,
};
use price_cache_context::PriceCacheContext;
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
use tokio::sync::{
    mpsc::{self, UnboundedReceiver, UnboundedSender},
    oneshot,
};

use crate::block_tree_service::{BlockTreeReadGuard, BlockTreeService, GetBlockTreeGuardMessage};

#[derive(Debug)]
pub enum EmaServiceMessage {
    GetCurrentEma {
        response: oneshot::Sender<IrysTokenPrice>,
    },
    GetEmaForNextAdjustmentPeriod {
        height_of_new_adjustment_block: u64,
        response: oneshot::Sender<Result<IrysTokenPrice, EmaCalculationError>>,
    },
    NewConfirmedBlock,
}

#[derive(Debug, thiserror::Error)]
pub enum EmaCalculationError {
    #[error(
        "the provided block height does not correspond to a block that requires EMA recomputation"
    )]
    HeightIsNotEmaAdjustmentBlock,
}

#[derive(Debug, Clone)]
pub struct EmaServiceHandle {
    pub sender: UnboundedSender<EmaServiceMessage>,
}

impl EmaServiceHandle {
    pub fn spawn_service(
        exec: &TaskExecutor,
        block_tree_read_guard: BlockTreeReadGuard,
        config: &Config,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let genesis_token_price = config.genesis_token_price;
        let blocks_in_interval = config.price_adjustment_interval;
        exec.spawn_critical_with_graceful_shutdown_signal("EMA Service", |shutdown| async move {
            let cache_service = EmaService {
                shutdown,
                msg_rx: rx,
                inner: Inner {
                    blocks_in_interval,
                    block_tree_read_guard,
                    genesis_token_price,
                },
            };
            cache_service.start().await
        });
        EmaServiceHandle { sender: tx }
    }
}

#[derive(Debug)]
pub struct EmaService {
    pub shutdown: GracefulShutdown,
    pub msg_rx: UnboundedReceiver<EmaServiceMessage>,
    pub inner: Inner,
}

#[derive(Debug)]
pub struct Inner {
    pub blocks_in_interval: u64,
    pub block_tree_read_guard: BlockTreeReadGuard,
    pub genesis_token_price: IrysTokenPrice,
}

impl EmaService {
    async fn start(mut self) {
        tracing::info!("starting EMA service");
        use futures::future::Either;

        let mut price_ctx = PriceCacheContext::from_canonical_chain(
            self.inner.block_tree_read_guard.clone(),
            self.inner.blocks_in_interval,
        )
        .await;

        let mut shutdown_future = pin!(&mut self.shutdown);
        let shutdown_guard = loop {
            let mut msg_rx = pin!(self.msg_rx.recv());
            match futures::future::select(&mut msg_rx, &mut shutdown_future).await {
                Either::Left((Some(msg), _)) => {
                    tracing::debug!(?msg);
                    self.inner.handle_message(msg, &mut price_ctx).await;
                }
                Either::Left((None, _)) => {
                    tracing::warn!("receiver channel closed");
                    break None;
                }
                Either::Right((shutdown, _)) => {
                    tracing::warn!("shutdown signal received");
                    break Some(shutdown);
                }
            }
        };

        tracing::debug!(amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdwon");
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.inner.handle_message(msg, &mut price_ctx).await;
        }

        // explicitly inform the TaskManager that we're shutting down
        drop(shutdown_guard);

        tracing::info!("shutting down EMA service");
    }
}

impl Inner {
    async fn handle_message(&mut self, msg: EmaServiceMessage, context: &mut PriceCacheContext) {
        match msg {
            EmaServiceMessage::GetCurrentEma { response } => {
                let _ = response.send(context.ema_price_to_use());
            }
            EmaServiceMessage::GetEmaForNextAdjustmentPeriod {
                response,
                height_of_new_adjustment_block,
            } => {
                // sanity check to ensure we only compute a new EMA on boundary blocks
                if context.next_ema_adjustment_height != height_of_new_adjustment_block {
                    let _ = response.send(Err(EmaCalculationError::HeightIsNotEmaAdjustmentBlock));
                    return;
                }

                // example EMA calculation on block 29:
                // 1. take the registered Irys price in block 18 (non EMA block) and the stored irys price in block 19 (EMA block).
                // 2. using these values compute EMA for block 29. In this case the *n* (number of block prices) would be 10 (E29.height - E19.height).
                // 3. this is the price that will be used in the interval 39->49, which will be reported to other systems querying for EMA prices.

                // the amount of blocks in between the 2 EMA calculation values
                let blocks_in_between = self.blocks_in_interval;

                // calculate the new EMA
                let new_ema = context
                    .block_previous_interval_predecessor
                    .irys_price
                    .calculate_ema(
                        blocks_in_between,
                        context.block_previous_interval.irys_price.to_ema(),
                    )
                    .unwrap();
                tracing::info!(
                    new_ema = ?new_ema.token_to_decimal().unwrap(),
                    prev_predecessor_height = ?context.block_previous_interval_predecessor.height,
                    prev_ema_height = ?context.block_previous_interval.height,
                    "computing new EMA"
                );

                let _ = response.send(Ok(new_ema.to_irys_price()));
            }
            EmaServiceMessage::NewConfirmedBlock => {
                // Rebuild the entire data cache just like we do at startup.
                *context = PriceCacheContext::from_canonical_chain(
                    self.block_tree_read_guard.clone(),
                    self.blocks_in_interval,
                )
                .await;
            }
        }
    }
}

mod price_cache_context {
    use super::*;

    #[derive(Debug)]
    pub(super) struct PriceCacheContext {
        pub(super) block_previous_interval: IrysBlockHeader,
        pub(super) block_previous_interval_predecessor: IrysBlockHeader,
        pub(super) block_two_adj_intervals_ago: IrysBlockHeader,
        pub(super) next_ema_adjustment_height: u64,
    }

    impl PriceCacheContext {
        /// Builds the entire context from scratch by scanning the canonical chain.
        pub(super) async fn from_canonical_chain(
            block_tree_read_guard: BlockTreeReadGuard,
            blocks_in_price_adjustment_interval: u64,
        ) -> Self {
            // Rebuild the entire data cache just like we do at startup.
            let canonical_chain = get_canonical_chain(block_tree_read_guard.clone()).await.0;
            let (_latest_block_hash, latest_block_height, ..) = canonical_chain.last().unwrap();

            // Derive indexes
            let previous_epoch_block_height = if is_ema_recalculation_block(
                *latest_block_height,
                blocks_in_price_adjustment_interval,
            ) {
                *latest_block_height
            } else {
                previous_ema_recalculation_block_height(
                    *latest_block_height,
                    blocks_in_price_adjustment_interval,
                )
            };
            let block_previous_interval_predecessor = previous_epoch_block_height.saturating_sub(1);
            let two_epochs_ago_block = previous_ema_recalculation_block_height(
                previous_epoch_block_height,
                blocks_in_price_adjustment_interval,
            );

            // fetch the blocks
            let fetch_block_with_height = async |height: u64| {
                let canonical_len = canonical_chain.len();
                let diff_from_latest_height = latest_block_height.saturating_sub(height) as usize;
                let adjusted_index = canonical_len - diff_from_latest_height - 1;
                let (hash, new_height, ..) = canonical_chain.get(adjusted_index).unwrap();
                assert_eq!(
                    height, *new_height,
                    "height mismatch in the canonical chain data"
                );
                get_block(block_tree_read_guard.clone(), *hash).await
            };
            let block_previous_interval = fetch_block_with_height(previous_epoch_block_height);
            let block_previous_interval_predecessor =
                fetch_block_with_height(block_previous_interval_predecessor);
            let block_two_price_intervals_ago = fetch_block_with_height(two_epochs_ago_block);

            // await concurrently
            let (
                block_previous_interval,
                block_previous_interval_predecessor,
                block_two_price_intervals_ago,
            ) = join!(
                block_previous_interval,
                block_previous_interval_predecessor,
                block_two_price_intervals_ago
            );
            Self {
                block_previous_interval,
                block_previous_interval_predecessor,
                block_two_adj_intervals_ago: block_two_price_intervals_ago,
                next_ema_adjustment_height: {
                    if previous_epoch_block_height == 0 {
                        blocks_in_price_adjustment_interval - 1
                    } else {
                        previous_epoch_block_height
                            .saturating_add(blocks_in_price_adjustment_interval)
                    }
                },
            }
        }

        pub(crate) fn ema_price_to_use(&self) -> IrysTokenPrice {
            self.block_two_adj_intervals_ago.irys_price
        }
    }

    /// rerturns the canonical chain where the first item in the Vec is the oldest block
    async fn get_canonical_chain(
        tree: BlockTreeReadGuard,
    ) -> (Vec<(H256, u64, Vec<H256>, Vec<H256>)>, usize) {
        let canonical_chain =
            tokio::task::spawn_blocking(move || tree.read().get_canonical_chain())
                .await
                .unwrap();
        canonical_chain
    }

    async fn get_block(
        block_tree_read_guard: BlockTreeReadGuard,
        block_hash: H256,
    ) -> IrysBlockHeader {
        let block = tokio::task::spawn_blocking(move || {
            block_tree_read_guard.read().get_block(&block_hash).cloned()
        })
        .await
        .unwrap()
        .unwrap();
        block
    }

    #[cfg(test)]
    mod tests {

        use std::sync::{Arc, RwLock};

        use crate::block_tree_service::{BlockState, BlockTreeCache, ChainState};

        use super::*;

        #[test_log::test(tokio::test)]
        async fn test_build_valid_price_context_when_only_genesis_exists() {
            // setup
            let block_tree_guard = genesis_tree(&mut [IrysBlockHeader {
                height: 0,
                ..IrysBlockHeader::new_mock_header()
            }]);
            let interval = 10;

            // actoin
            let price_cache =
                PriceCacheContext::from_canonical_chain(block_tree_guard, interval).await;

            // assert
            assert_eq!(price_cache.block_previous_interval.height, 0);
            assert_eq!(price_cache.block_previous_interval_predecessor.height, 0);
            assert_eq!(price_cache.block_two_adj_intervals_ago.height, 0);
            assert_eq!(price_cache.next_ema_adjustment_height, 9);
        }

        // todo use rstest to parametrise the tests

        #[test_log::test(tokio::test)]
        async fn test_valid_price_after_first_epoch() {
            // setup
            let interval = 10;
            let mut blocks = (0..=10)
                .map(|height| IrysBlockHeader {
                    height,
                    ..IrysBlockHeader::new_mock_header()
                })
                .collect::<Vec<_>>();
            let block_tree_guard = genesis_tree(&mut blocks);

            // actoin
            let price_cache =
                PriceCacheContext::from_canonical_chain(block_tree_guard, interval).await;

            // assert
            assert_eq!(price_cache.block_two_adj_intervals_ago.height, 0);
            assert_eq!(price_cache.next_ema_adjustment_height, 19);
            assert_eq!(price_cache.block_previous_interval.height, 9);
            assert_eq!(price_cache.block_previous_interval_predecessor.height, 8);
        }

        #[test_log::test(tokio::test)]
        async fn test_valid_price_after_second_epoch() {
            // setup
            let interval = 10;
            let mut blocks = (0..=19)
                .map(|height| IrysBlockHeader {
                    height,
                    ..IrysBlockHeader::new_mock_header()
                })
                .collect::<Vec<_>>();
            let block_tree_guard = genesis_tree(&mut blocks);

            // actoin
            let price_cache =
                PriceCacheContext::from_canonical_chain(block_tree_guard, interval).await;

            // assert
            assert_eq!(price_cache.next_ema_adjustment_height, 29);
            assert_eq!(price_cache.block_previous_interval.height, 19);
            assert_eq!(price_cache.block_previous_interval_predecessor.height, 18);
            assert_eq!(price_cache.block_two_adj_intervals_ago.height, 9);
        }

        /// The underlyig ieda of the test -- after we go through the first 2 epochs where
        /// the genesis block is used, we start using E9 block
        #[test_log::test(tokio::test)]
        async fn test_valid_price_after_21_blocks() {
            // setup
            let interval = 10;
            let mut blocks = (0..=20)
                .map(|height| IrysBlockHeader {
                    height,
                    ..IrysBlockHeader::new_mock_header()
                })
                .collect::<Vec<_>>();
            let block_tree_guard = genesis_tree(&mut blocks);

            // actoin
            let price_cache =
                PriceCacheContext::from_canonical_chain(block_tree_guard, interval).await;

            // assert
            assert_eq!(price_cache.next_ema_adjustment_height, 29);
            assert_eq!(price_cache.block_previous_interval.height, 19);
            assert_eq!(price_cache.block_previous_interval_predecessor.height, 18);
            assert_eq!(price_cache.block_two_adj_intervals_ago.height, 9);
        }

        #[test_log::test(tokio::test)]
        async fn test_valid_price_after_100_blocks() {
            // setup
            let interval = 10;
            let mut blocks = (0..100)
                .map(|height| IrysBlockHeader {
                    height,
                    ..IrysBlockHeader::new_mock_header()
                })
                .collect::<Vec<_>>();
            let block_tree_guard = genesis_tree(&mut blocks);

            // actoin
            let price_cache =
                PriceCacheContext::from_canonical_chain(block_tree_guard, interval).await;

            // assert
            assert_eq!(price_cache.next_ema_adjustment_height, 109);
            assert_eq!(price_cache.block_previous_interval.height, 99);
            assert_eq!(price_cache.block_previous_interval_predecessor.height, 98);
            assert_eq!(price_cache.block_two_adj_intervals_ago.height, 89);
        }

        fn genesis_tree(blocks: &mut [IrysBlockHeader]) -> BlockTreeReadGuard {
            let mut block_hash = H256::random();
            let mut iter = blocks.into_iter();
            let genesis_block = iter.next().unwrap();
            genesis_block.block_hash = block_hash;
            genesis_block.cumulative_diff = 0.into();

            let mut block_tree_cache = BlockTreeCache::new(&genesis_block);
            block_tree_cache.mark_tip(&block_hash).unwrap();
            for block in iter {
                // update the prev block hash
                block.previous_block_hash = block_hash;
                block.cumulative_diff = block.height.into();

                // generate a new block hash
                block_hash = H256::random();
                block.block_hash = block_hash;
                block_tree_cache
                    .add_common(
                        block.block_hash.clone(),
                        block,
                        Arc::new(Vec::new()),
                        ChainState::Onchain,
                    )
                    .unwrap();
            }

            let block_tree_cache = Arc::new(RwLock::new(block_tree_cache));
            let block_tree_guard = BlockTreeReadGuard::new(block_tree_cache);
            block_tree_guard
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth::tasks::TaskManager;
    use rust_decimal::Decimal;
    use std::sync::{Arc, RwLock};

    fn build_tree_with_n_blocks(blocks: u64) -> (BlockTreeReadGuard, Vec<IrysTokenPrice>) {
        let mut blocks = (0..blocks)
            .map(|height| {
                let mut b = IrysBlockHeader::new_mock_header();
                b.height = height;
                // add a random constant to the price to differentiate it from the height
                let price = IrysTokenPrice::token(Decimal::from(height + 5)).unwrap();
                b.irys_price = price;
                b
            })
            .collect::<Vec<_>>();
        let prices = blocks
            .iter()
            .map(|block| block.irys_price.clone())
            .collect::<Vec<_>>();
        (genesis_tree(&mut blocks), prices)
    }

    fn genesis_tree(blocks: &mut [IrysBlockHeader]) -> BlockTreeReadGuard {
        use crate::block_tree_service::{BlockTreeCache, ChainState};
        let mut block_hash = H256::random();
        let mut iter = blocks.iter_mut();
        let genesis_block = iter.next().unwrap();
        genesis_block.block_hash = block_hash;
        genesis_block.cumulative_diff = 0.into();

        let mut block_tree_cache = BlockTreeCache::new(genesis_block);
        block_tree_cache.mark_tip(&block_hash).unwrap();
        for block in iter {
            block.previous_block_hash = block_hash;
            block.cumulative_diff = block.height.into();
            block_hash = H256::random();
            block.block_hash = block_hash;
            block_tree_cache
                .add_common(
                    block.block_hash.clone(),
                    block,
                    Arc::new(Vec::new()),
                    ChainState::Onchain,
                )
                .unwrap();
        }
        let block_tree_cache = Arc::new(RwLock::new(block_tree_cache));
        BlockTreeReadGuard::new(block_tree_cache)
    }

    struct TestCtx {
        guard: BlockTreeReadGuard,
        config: Config,
        task_manager: TaskManager,
        task_executor: TaskExecutor,
        ema_handle: EmaServiceHandle,
        prices: Vec<IrysTokenPrice>,
    }

    impl TestCtx {
        fn setup(block_count: u64, config: Config) -> Self {
            let (block_tree_guard, prices) = build_tree_with_n_blocks(block_count);
            assert_eq!(prices.len(), block_count as usize);
            let task_manager = TaskManager::new(tokio::runtime::Handle::current());
            let task_executor = task_manager.executor();
            let service =
                EmaServiceHandle::spawn_service(&task_executor, block_tree_guard.clone(), &config);
            Self {
                guard: block_tree_guard,
                config,
                task_manager,
                task_executor,
                ema_handle: service,
                prices,
            }
        }
    }
    mod get_current_ema {
        use super::*;
        use rstest::rstest;
        use test_log::test;

        #[test(tokio::test)]
        #[rstest]
        #[case(1, 0)]
        #[case(9, 0)]
        #[case(19, 0)]
        #[case(20, 9)] // use the 10th block price during 3rd EMA interval
        #[case(29, 9)]
        #[case(30, 19)]
        #[timeout(std::time::Duration::from_millis(100))]
        async fn test_when_on_genesis_block(
            #[case] block_count: u64,
            #[case] price_block_idx: usize,
        ) {
            // setup
            let ctx = TestCtx::setup(
                block_count,
                Config {
                    price_adjustment_interval: 10,
                    ..Config::testnet()
                },
            );
            let desired_block_price = ctx.prices[price_block_idx];

            // action
            let (tx, rx) = tokio::sync::oneshot::channel();
            ctx.ema_handle
                .sender
                .send(EmaServiceMessage::GetCurrentEma { response: tx })
                .unwrap();
            let response = rx.await.unwrap();

            // assert
            assert_eq!(response, desired_block_price);
            assert!(!ctx.ema_handle.sender.is_closed());
        }
    }

    mod get_ema_for_next_adjustment_period {
        use super::*;
        use rstest::rstest;
        use test_log::test;

        #[test(tokio::test)]
        async fn first_adjustment_period() {
            // prepare
            let block_count = 9;
            let price_adjustment_interval = 10;
            let ctx = TestCtx::setup(
                block_count,
                Config {
                    price_adjustment_interval,
                    ..Config::testnet()
                },
            );

            // action
            let (tx, rx) = tokio::sync::oneshot::channel();
            ctx.ema_handle
                .sender
                .send(EmaServiceMessage::GetEmaForNextAdjustmentPeriod {
                    height_of_new_adjustment_block: 9,
                    response: tx,
                })
                .unwrap();
            let ema_response = rx.await.unwrap().unwrap();

            // assert
            let prev_price = ctx.prices[0];
            let ema_computed = ctx.prices[0]
                .calculate_ema(price_adjustment_interval, prev_price.to_ema())
                .unwrap();
            assert_eq!(ema_computed, ema_response.to_ema());
        }

        #[test(tokio::test)]
        #[rstest]
        #[case(19, 9, 8)]
        #[case(29, 19, 18)]
        #[case(39, 29, 28)]
        #[case(169, 159, 158)]
        async fn nth_adjustment_period(
            #[case] block_count: u64,
            #[case] prev_ema_idx: usize,
            #[case] prev_ema_predecessor_idx: usize,
        ) {
            // prepare
            let price_adjustment_interval = 10;
            let ctx = TestCtx::setup(
                block_count,
                Config {
                    price_adjustment_interval,
                    ..Config::testnet()
                },
            );

            // action
            let (tx, rx) = tokio::sync::oneshot::channel();
            ctx.ema_handle
                .sender
                .send(EmaServiceMessage::GetEmaForNextAdjustmentPeriod {
                    height_of_new_adjustment_block: block_count,
                    response: tx,
                })
                .unwrap();
            let ema_response = rx.await.unwrap().unwrap();

            // assert
            let prev_price = ctx.prices[prev_ema_idx];
            let ema_computed = ctx.prices[prev_ema_predecessor_idx]
                .calculate_ema(price_adjustment_interval, prev_price.to_ema())
                .unwrap();
            assert_eq!(ema_computed, ema_response.to_ema());
        }

        #[test(tokio::test)]
        #[rstest]
        #[case(1, 2)]
        #[case(1, 3)]
        #[case(1, 8)]
        #[case(15, 16)]
        #[case(15, 17)]
        #[case(15, 11)]
        #[case(15, 18)]
        async fn nth_invalid_ema_block_requsets(
            #[case] block_count: u64,
            #[case] height_of_new_adjustment_block: u64,
        ) {
            // prepare
            let price_adjustment_interval = 10;
            let ctx = TestCtx::setup(
                block_count,
                Config {
                    price_adjustment_interval,
                    ..Config::testnet()
                },
            );

            // action
            let (tx, rx) = tokio::sync::oneshot::channel();
            ctx.ema_handle
                .sender
                .send(EmaServiceMessage::GetEmaForNextAdjustmentPeriod {
                    height_of_new_adjustment_block,
                    response: tx,
                })
                .unwrap();
            let ema_response = rx.await.unwrap();

            // assert
            assert!(matches!(
                ema_response,
                Err(EmaCalculationError::HeightIsNotEmaAdjustmentBlock)
            ));
        }
    }

    // todo test shutdown behaviour
    // todo test `new confirmed block`
}
