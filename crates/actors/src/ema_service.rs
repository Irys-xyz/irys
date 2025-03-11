//! EMA service module. It is responsible for keeping track of Irys token price readjustment period.
//!
//! example EMA calculation for block with height 29 (block heights start with 0, so this is for the 30th block):
//! 1. take the registered Irys price in block height 18 (non EMA block) and the stored irys price in block with height 19 (EMA block).
//! 2. using these values compute EMA for block with height 29. In this case the *n* (number of block prices) would be 10 (E29.height - E19.height).
//! 3. this is the price that will be used in the interval 39->49, which will be reported to other systems querying for EMA prices.
use crate::block_tree_service::BlockTreeReadGuard;
use irys_types::{
    is_ema_recalculation_block, previous_ema_recalculation_block_height, Config, IrysBlockHeader,
    IrysTokenPrice, H256,
};
use price_cache_context::PriceCacheContext;
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
use std::pin::pin;
use tokio::sync::{
    mpsc::{self, UnboundedReceiver, UnboundedSender},
    oneshot,
};

/// Messages that the EMA service supports
#[derive(Debug)]
pub enum EmaServiceMessage {
    /// Return the current EMA that other components must use (eg pricing module)
    GetCurrentEma {
        response: oneshot::Sender<IrysTokenPrice>,
    },
    /// Returns the EMA irys price that must be used in the next EMA adjustment block
    GetEmaForNextAdjustmentPeriod {
        height_of_new_adjustment_block: u64,
        response: oneshot::Sender<Result<IrysTokenPrice, EmaCalculationError>>,
    },
    /// Supposted to be sent whenever Irys produces a new confirmed block. The EMA service will refrech its cache.
    NewConfirmedBlock,
}

/// Potenntial response errors that the EMA service may return
#[derive(Debug, thiserror::Error)]
pub enum EmaCalculationError {
    /// Returned when requesting a new EMA to be included in the next EMA block,
    /// but the desired block is not an EMA readjustment block.
    #[error(
        "the provided block height does not correspond to a block that requires EMA recomputation"
    )]
    HeightIsNotEmaAdjustmentBlock,
}

/// Sender handle for the EMA service
#[derive(Debug, Clone)]
pub struct EmaServiceHandle {
    pub sender: UnboundedSender<EmaServiceMessage>,
}

impl EmaServiceHandle {
    /// Spawn a new EMA service
    pub fn spawn_service(
        exec: &TaskExecutor,
        block_tree_read_guard: BlockTreeReadGuard,
        config: &Config,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let genesis_token_price = config.genesis_token_price;
        let blocks_in_interval = config.price_adjustment_interval;
        exec.spawn_critical_with_graceful_shutdown_signal("EMA Service", |shutdown| async move {
            let ema_service = EmaService {
                shutdown,
                msg_rx: rx,
                inner: Inner {
                    blocks_in_interval,
                    block_tree_read_guard,
                    genesis_token_price,
                },
            };
            ema_service
                .start()
                .await
                .expect("ema service encountered an irrecoverable error")
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
    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting EMA service");
        use futures::future::Either;

        let mut price_ctx = PriceCacheContext::from_canonical_chain(
            self.inner.block_tree_read_guard.clone(),
            self.inner.blocks_in_interval,
        )
        .await?;

        let mut shutdown_future = pin!(self.shutdown);
        let shutdown_guard = loop {
            let mut msg_rx = pin!(self.msg_rx.recv());
            match futures::future::select(&mut msg_rx, &mut shutdown_future).await {
                Either::Left((Some(msg), _)) => {
                    self.inner.handle_message(msg, &mut price_ctx).await?;
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
            self.inner.handle_message(msg, &mut price_ctx).await?;
        }

        // explicitly inform the TaskManager that we're shutting down
        drop(shutdown_guard);

        tracing::info!("shutting down EMA service");
        Ok(())
    }
}

impl Inner {
    #[tracing::instrument(skip_all, err)]
    async fn handle_message(
        &mut self,
        msg: EmaServiceMessage,
        context: &mut PriceCacheContext,
    ) -> eyre::Result<()> {
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
                    return Ok(());
                }

                // example EMA calculation on block 29:
                // 1. take the registered Irys price in block 18 (non EMA block) and the stored irys price in block 19 (EMA block).
                // 2. using these values compute EMA for block 29. In this case the *n* (number of block prices) would be 10 (E29.height - E19.height).
                // 3. this is the price that will be used in the interval 39->49, which will be reported to other systems querying for EMA prices.

                // the amount of blocks in between the 2 EMA calculation values
                let blocks_in_between = self.blocks_in_interval;

                // calculate the new EMA
                let new_ema = context
                    .block_previous_ema_predecessor
                    .irys_price
                    .calculate_ema(
                        blocks_in_between,
                        context.block_previous_ema.irys_price.to_ema(),
                    )?;
                tracing::info!(
                    new_ema = ?new_ema.token_to_decimal().unwrap(),
                    prev_predecessor_height = ?context.block_previous_ema_predecessor.height,
                    prev_ema_height = ?context.block_previous_ema.height,
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
                .await?;
            }
        }
        Ok(())
    }
}

/// Utility module for that's responsible for extracting the desired blocks from the
/// `BlockTree` to properly report prices & calculate new interval values
mod price_cache_context {
    use futures::try_join;

    use super::*;

    #[derive(Debug)]
    pub(super) struct PriceCacheContext {
        pub(super) block_previous_ema: IrysBlockHeader,
        pub(super) block_previous_ema_predecessor: IrysBlockHeader,
        pub(super) block_two_adj_intervals_ago: IrysBlockHeader,
        pub(super) next_ema_adjustment_height: u64,
    }

    impl PriceCacheContext {
        /// Builds the entire context from scratch by scanning the canonical chain.
        pub(super) async fn from_canonical_chain(
            block_tree_read_guard: BlockTreeReadGuard,
            blocks_in_price_adjustment_interval: u64,
        ) -> eyre::Result<Self> {
            // Rebuild the entire data cache just like we do at startup.
            let canonical_chain = get_canonical_chain(block_tree_read_guard.clone()).await?.0;
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
                get_block(block_tree_read_guard.clone(), *hash)
                    .await
                    .and_then(|block| block.ok_or_else(|| eyre::eyre!("block hash {hash:?} from canonical chain cannot be retrieved from the block index")))
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
            ) = try_join!(
                block_previous_interval,
                block_previous_interval_predecessor,
                block_two_price_intervals_ago
            )?;
            Ok(Self {
                block_previous_ema: block_previous_interval,
                block_previous_ema_predecessor: block_previous_interval_predecessor,
                block_two_adj_intervals_ago: block_two_price_intervals_ago,
                next_ema_adjustment_height: {
                    if previous_epoch_block_height == 0 {
                        blocks_in_price_adjustment_interval - 1
                    } else {
                        previous_epoch_block_height
                            .saturating_add(blocks_in_price_adjustment_interval)
                    }
                },
            })
        }

        pub(crate) fn ema_price_to_use(&self) -> IrysTokenPrice {
            self.block_two_adj_intervals_ago.irys_price
        }
    }

    /// returns the canonical chain where the first item in the Vec is the oldest block
    pub(crate) async fn get_canonical_chain(
        tree: BlockTreeReadGuard,
    ) -> eyre::Result<(Vec<(H256, u64, Vec<H256>, Vec<H256>)>, usize)> {
        let canonical_chain =
            tokio::task::spawn_blocking(move || tree.read().get_canonical_chain()).await?;
        Ok(canonical_chain)
    }

    pub(crate) async fn get_block(
        block_tree_read_guard: BlockTreeReadGuard,
        block_hash: H256,
    ) -> eyre::Result<Option<IrysBlockHeader>> {
        let res = tokio::task::spawn_blocking(move || {
            block_tree_read_guard.read().get_block(&block_hash).cloned()
        })
        .await?;
        Ok(res)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::ema_service::tests::genesis_tree;
        use rstest::rstest;

        #[test_log::test(tokio::test)]
        #[rstest]
        #[case(0, 9, 0, 0, 0)]
        #[case(1, 9, 0, 0, 0)]
        #[case(10, 19, 0, 9, 8)]
        #[case(18, 19, 0, 9, 8)]
        #[case(19, 29, 9, 19, 18)]
        #[case(20, 29, 9, 19, 18)]
        #[case(100, 109, 89, 99, 98)]
        async fn test_valid_price_cache(
            #[case] height_latest_block: u64,
            #[case] height_next_ema_adjustment: u64,
            #[case] height_two_ema_intervals_ago: u64,
            #[case] height_previous_ema: u64,
            #[case] height_previous_interval_predecessor: u64,
        ) {
            // setup
            let interval = 10;
            let mut blocks = (0..=height_latest_block)
                .map(|height| IrysBlockHeader {
                    height,
                    ..IrysBlockHeader::new_mock_header()
                })
                .collect::<Vec<_>>();
            let block_tree_guard = genesis_tree(&mut blocks);

            // actoin
            let price_cache = PriceCacheContext::from_canonical_chain(block_tree_guard, interval)
                .await
                .unwrap();

            // assert
            assert_eq!(
                price_cache.block_two_adj_intervals_ago.height,
                height_two_ema_intervals_ago
            );
            assert_eq!(
                price_cache.next_ema_adjustment_height,
                height_next_ema_adjustment
            );
            assert_eq!(price_cache.block_previous_ema.height, height_previous_ema);
            assert_eq!(
                price_cache.block_previous_ema_predecessor.height,
                height_previous_interval_predecessor
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_tree_service::ChainState;
    use price_cache_context::get_canonical_chain;
    use reth::tasks::TaskManager;
    use rstest::rstest;
    use rust_decimal::Decimal;
    use std::sync::{Arc, RwLock};
    use test_log::test;

    pub(crate) fn build_genesis_tree_with_n_blocks(
        blocks: u64,
    ) -> (BlockTreeReadGuard, Vec<IrysTokenPrice>) {
        let (mut blocks, prices) = build_tree(0, blocks);
        (genesis_tree(&mut blocks), prices)
    }

    fn build_tree(init_height: u64, blocks: u64) -> (Vec<IrysBlockHeader>, Vec<IrysTokenPrice>) {
        let blocks = (init_height..(blocks + init_height))
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
        (blocks, prices)
    }

    pub(crate) fn genesis_tree(blocks: &mut [IrysBlockHeader]) -> BlockTreeReadGuard {
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

    #[expect(
        dead_code,
        reason = "structs are held in-memory to prevent the `drop` to trigger"
    )]
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
            let (block_tree_guard, prices) = build_genesis_tree_with_n_blocks(block_count);
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

    #[test(tokio::test)]
    #[rstest]
    #[timeout(std::time::Duration::from_secs(3))]
    async fn test_ema_service_shutdown_no_pending_messages() {
        // Setup
        let block_count = 3;
        let price_adjustment_interval = 10;
        let ctx = TestCtx::setup(
            block_count,
            Config {
                price_adjustment_interval,
                ..Config::testnet()
            },
        );

        // Send shutdown signal
        tokio::task::spawn_blocking(|| {
            ctx.task_manager.graceful_shutdown();
        })
        .await
        .unwrap();

        // Attempt to send a message and ensure it fails
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let send_result = ctx
            .ema_handle
            .sender
            .send(EmaServiceMessage::GetCurrentEma { response: tx });

        // assert
        assert!(
            send_result.is_err(),
            "Service should not accept new messages after shutdown"
        );
        assert!(
            ctx.ema_handle.sender.is_closed(),
            "Service sender should be closed"
        );
    }

    #[test(tokio::test)]
    async fn test_ema_service_new_confirmed_block() {
        // Setup
        let initial_block_count = 10;
        let price_adjustment_interval = 10;
        let ctx = TestCtx::setup(
            initial_block_count,
            Config {
                price_adjustment_interval,
                ..Config::testnet()
            },
        );

        // setup -- generate new blocks to be added
        let (chain, ..) = get_canonical_chain(ctx.guard.clone()).await.unwrap();
        let (mut latest_block_hash, ..) = *chain.last().unwrap();
        let (new_blocks, ..) = build_tree(initial_block_count, 100);

        // extract the final price that we expect.
        // in total there are 110 blocks once we add the new ones.
        // The EMA to use is the 100th block (idx 89 in the `new_blocks`).
        let expected_final_ema_price = new_blocks[89].irys_price;

        // setup  -- add new blocks to the canonical chain post-initializatoin
        let mut tree = ctx.guard.write();
        for mut block in new_blocks {
            block.previous_block_hash = latest_block_hash;
            block.cumulative_diff = block.height.into();
            latest_block_hash = H256::random();
            block.block_hash = latest_block_hash;
            tree.add_common(
                block.block_hash.clone(),
                &block,
                Arc::new(Vec::new()),
                ChainState::Onchain,
            )
            .unwrap();
        }
        drop(tree);

        // Send a NewConfirmedBlock message
        let send_result = ctx
            .ema_handle
            .sender
            .send(EmaServiceMessage::NewConfirmedBlock);
        assert!(
            send_result.is_ok(),
            "Service should accept new confirmed block messages"
        );

        // Verify that the price cache is updated
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.ema_handle
            .sender
            .send(EmaServiceMessage::GetCurrentEma { response: tx })
            .unwrap();
        let response = rx.await.unwrap();

        assert_eq!(
            response, expected_final_ema_price,
            "Price cache should reset correctly"
        );
    }
}
