//! EMA service module. It is responsible for keeping track of Irys token price readjustment period.
use crate::block_tree_service::BlockTreeReadGuard;
use irys_types::{
    is_ema_recalculation_block, previous_ema_recalculation_block_height, Config, IrysBlockHeader,
    IrysTokenPrice,
};
use price_cache_context::PriceCacheContext;
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
use std::pin::pin;
use tokio::{
    sync::{mpsc::UnboundedReceiver, oneshot},
    task::JoinHandle,
};

/// Messages that the EMA service supports
#[derive(Debug)]
pub enum EmaServiceMessage {
    /// Return the current EMA that other components must use (eg pricing module)
    GetCurrentEmaForPricing {
        response: oneshot::Sender<IrysTokenPrice>,
    },
    /// Returns the EMA irys price that must be used in the next EMA adjustment block
    GetEmaForNewBlock {
        height_of_new_block: u64,
        oracle_price: IrysTokenPrice,
        response: oneshot::Sender<eyre::Result<IrysTokenPrice>>,
    },
    /// Supposted to be sent whenever Irys produces a new confirmed block. The EMA service will refrech its cache.
    NewConfirmedBlock,
}

#[derive(Debug)]
pub struct EmaService {
    shutdown: GracefulShutdown,
    msg_rx: UnboundedReceiver<EmaServiceMessage>,
    inner: Inner,
}

#[derive(Debug)]
struct Inner {
    blocks_in_interval: u64,
    block_tree_read_guard: BlockTreeReadGuard,
    price_ctx: PriceCacheContext,
}

impl EmaService {
    /// Spawn a new EMA service
    pub fn spawn_service(
        exec: &TaskExecutor,
        block_tree_read_guard: BlockTreeReadGuard,
        rx: UnboundedReceiver<EmaServiceMessage>,
        config: &Config,
    ) -> JoinHandle<()> {
        let blocks_in_interval = config.price_adjustment_interval;
        exec.spawn_critical_with_graceful_shutdown_signal("EMA Service", |shutdown| async move {
            let price_ctx = PriceCacheContext::from_canonical_chain(
                block_tree_read_guard.clone(),
                blocks_in_interval,
            )
            .await
            .expect("initial PriceCacheContext restoration failed");
            let ema_service = Self {
                shutdown,
                msg_rx: rx,
                inner: Inner {
                    price_ctx,
                    blocks_in_interval,
                    block_tree_read_guard,
                },
            };
            ema_service
                .start()
                .await
                .expect("ema service encountered an irrecoverable error")
        })
    }

    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting EMA service");
        use futures::future::Either;

        let mut shutdown_future = pin!(self.shutdown);
        let shutdown_guard = loop {
            let mut msg_rx = pin!(self.msg_rx.recv());
            match futures::future::select(&mut msg_rx, &mut shutdown_future).await {
                Either::Left((Some(msg), _)) => {
                    self.inner.handle_message(msg).await?;
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
            self.inner.handle_message(msg).await?;
        }

        // explicitly inform the TaskManager that we're shutting down
        drop(shutdown_guard);

        tracing::info!("shutting down EMA service");
        Ok(())
    }
}

impl Inner {
    #[tracing::instrument(skip_all, err)]
    async fn handle_message(&mut self, msg: EmaServiceMessage) -> eyre::Result<()> {
        match msg {
            EmaServiceMessage::GetCurrentEmaForPricing { response } => {
                let _ = response.send(self.price_ctx.ema_price_to_use()).
                    inspect_err(|_| tracing::warn!("current EMA cannot be returned, sender has dropped its half of the channel"));
            }
            EmaServiceMessage::GetEmaForNewBlock {
                response,
                height_of_new_block,
                oracle_price,
            } => {
                // the first 2 adjustment intervals have special handling where we calculate the
                // EMA for each block using the value from the preceding one
                let new_ema = if height_of_new_block <= (self.blocks_in_interval * 2) {
                    // calculate the new EMA using the current oracle price + ema price
                    oracle_price.calculate_ema(
                        // we use the full interval for calculations even if in reality it's only 1 block diff
                        self.blocks_in_interval,
                        self.price_ctx.block_previous.ema_irys_price,
                    )
                } else {
                    // Now we have have generic handling.
                    //
                    // example EMA calculation on block 29:
                    // 1. take the registered Irys price in block 18 (non EMA block)
                    //    and the stored irys price in block 19 (EMA block).
                    // 2. using these values compute EMA for block 29. In this case
                    //    the *n* (number of block prices) would be 10 (E29.height - E19.height).
                    // 3. this is the price that will be used in the interval 39->49,
                    //    which will be reported to other systems querying for EMA prices.

                    // calculate the new EMA using historical oracle price + ema price
                    self.price_ctx
                        .block_previous_ema_predecessor
                        .oracle_irys_price
                        .calculate_ema(
                            self.blocks_in_interval,
                            self.price_ctx.block_previous_ema.ema_irys_price,
                        )
                };
                tracing::info!(
                    ?new_ema,
                    prev_predecessor_height = ?self.price_ctx.block_previous_ema_predecessor.height,
                    prev_ema_height = ?self.price_ctx.block_previous_ema.height,
                    "computing new EMA"
                );

                let _ = response.send(new_ema);
            }
            EmaServiceMessage::NewConfirmedBlock => {
                tracing::debug!("new confirmed block");
                // Rebuild the entire data cache just like we do at startup.
                self.price_ctx = PriceCacheContext::from_canonical_chain(
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

    use crate::block_tree_service::{get_block, get_canonical_chain};

    use super::*;

    #[derive(Debug)]
    pub(super) struct PriceCacheContext {
        pub(super) block_previous_ema: IrysBlockHeader,
        pub(super) block_previous_ema_predecessor: IrysBlockHeader,
        pub(super) block_two_adj_intervals_ago: IrysBlockHeader,
        pub(super) block_previous: IrysBlockHeader,
    }

    impl PriceCacheContext {
        /// Builds the entire context from scratch by scanning the canonical chain.
        pub(super) async fn from_canonical_chain(
            block_tree_read_guard: BlockTreeReadGuard,
            blocks_in_price_adjustment_interval: u64,
        ) -> eyre::Result<Self> {
            // Rebuild the entire data cache just like we do at startup.
            let canonical_chain = get_canonical_chain(block_tree_read_guard.clone()).await?.0;
            let (_latest_block_hash, latest_block_height, ..) =
                canonical_chain.last().expect("canonical chain is empty");

            // Derive indexes
            let height_previous_ema_block = if is_ema_recalculation_block(
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
            let height_previous_interval_predecessor = height_previous_ema_block.saturating_sub(1);
            let height_two_intervals_ago = previous_ema_recalculation_block_height(
                height_previous_ema_block,
                blocks_in_price_adjustment_interval,
            );

            // utility fn to fetch the block at a given height
            let fetch_block_with_height = async |height: u64| {
                let canonical_len = canonical_chain.len();
                let diff_from_latest_height = latest_block_height.saturating_sub(height) as usize;
                let adjusted_index = canonical_len
                    .saturating_sub(diff_from_latest_height)
                    .saturating_sub(1); // -1 because heights are zero based
                let (hash, new_height, ..) = canonical_chain.get(adjusted_index).unwrap();
                assert_eq!(
                    height, *new_height,
                    "height mismatch in the canonical chain data"
                );
                get_block(block_tree_read_guard.clone(), *hash)
                    .await
                    .and_then(|block| block.ok_or_else(|| eyre::eyre!("block hash {hash:?} from canonical chain cannot be retrieved from the block index")))
            };

            // fetch the blocks concurrently
            let (
                block_previous_interval,
                block_previous_interval_predecessor,
                block_two_price_intervals_ago,
                block_previous,
            ) = try_join!(
                fetch_block_with_height(height_previous_ema_block),
                fetch_block_with_height(height_previous_interval_predecessor),
                fetch_block_with_height(height_two_intervals_ago),
                fetch_block_with_height(*latest_block_height)
            )?;

            // Return an updated price cache
            Ok(Self {
                block_previous_ema: block_previous_interval,
                block_previous_ema_predecessor: block_previous_interval_predecessor,
                block_two_adj_intervals_ago: block_two_price_intervals_ago,
                block_previous,
            })
        }

        pub(crate) fn ema_price_to_use(&self) -> IrysTokenPrice {
            self.block_two_adj_intervals_ago.ema_irys_price
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::ema_service::tests::genesis_tree;
        use rstest::rstest;

        #[test_log::test(tokio::test)]
        #[rstest]
        #[case(0, 0, 0, 0)]
        #[case(1, 0, 0, 0)]
        #[case(10, 0, 9, 8)]
        #[case(18, 0, 9, 8)]
        #[case(19, 9, 19, 18)]
        #[case(20, 9, 19, 18)]
        #[case(100, 89, 99, 98)]
        async fn test_valid_price_cache(
            #[case] height_latest_block: u64,
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

            // action
            let price_cache = PriceCacheContext::from_canonical_chain(block_tree_guard, interval)
                .await
                .unwrap();

            // assert
            assert_eq!(
                price_cache.block_two_adj_intervals_ago.height,
                height_two_ema_intervals_ago
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
    use crate::block_tree_service::{get_canonical_chain, ChainState};
    use irys_types::H256;
    use reth::tasks::TaskManager;
    use rstest::rstest;
    use rust_decimal::Decimal;
    use std::sync::{Arc, RwLock};
    use test_log::test;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

    pub(crate) fn build_genesis_tree_with_n_blocks(
        blocks: u64,
    ) -> (BlockTreeReadGuard, Vec<PriceInfo>) {
        let (mut blocks, prices) = build_tree(0, blocks);
        (genesis_tree(&mut blocks), prices)
    }

    fn build_tree(init_height: u64, blocks: u64) -> (Vec<IrysBlockHeader>, Vec<PriceInfo>) {
        let blocks = (init_height..(blocks + init_height))
            .map(|height| {
                let mut header = IrysBlockHeader::new_mock_header();
                header.height = height;
                // add a random constant to the price to differentiate it from the height
                header.oracle_irys_price = rand_price(height);
                header.ema_irys_price = rand_price(height);
                header
            })
            .collect::<Vec<_>>();
        let prices = blocks
            .iter()
            .map(|block| PriceInfo {
                oracle: block.oracle_irys_price.clone(),
                ema: block.ema_irys_price.clone(),
            })
            .collect::<Vec<_>>();
        (blocks, prices)
    }

    pub(crate) fn rand_price(height: u64) -> IrysTokenPrice {
        let rand = rand::random::<u8>() as u64;
        let oracle_price = IrysTokenPrice::token(Decimal::from(height + rand)).unwrap();
        oracle_price
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
        ema_sender: UnboundedSender<EmaServiceMessage>,
        prices: Vec<PriceInfo>,
    }

    #[derive(Debug, Clone)]
    pub(crate) struct PriceInfo {
        oracle: IrysTokenPrice,
        ema: IrysTokenPrice,
    }

    impl TestCtx {
        fn setup(block_count: u64, config: Config) -> Self {
            let (block_tree_guard, prices) = build_genesis_tree_with_n_blocks(block_count);
            assert_eq!(prices.len(), block_count as usize);
            let task_manager = TaskManager::new(tokio::runtime::Handle::current());
            let task_executor = task_manager.executor();
            let (tx, rx) = unbounded_channel();
            let _handle =
                EmaService::spawn_service(&task_executor, block_tree_guard.clone(), rx, &config);
            Self {
                guard: block_tree_guard,
                config,
                task_manager,
                task_executor,
                ema_sender: tx,
                prices,
            }
        }

        fn setup_with_tree(
            block_tree_guard: BlockTreeReadGuard,
            prices: Vec<PriceInfo>,
            config: Config,
        ) -> Self {
            let task_manager = TaskManager::new(tokio::runtime::Handle::current());
            let task_executor = task_manager.executor();
            let (tx, rx) = unbounded_channel();
            let _handle =
                EmaService::spawn_service(&task_executor, block_tree_guard.clone(), rx, &config);
            Self {
                guard: block_tree_guard,
                config,
                task_manager,
                task_executor,
                ema_sender: tx,
                prices,
            }
        }
    }

    #[test(tokio::test)]
    #[rstest]
    #[case(1, 0)]
    #[case(2, 0)]
    #[case(9, 0)]
    #[case(19, 0)]
    #[case(20, 9)] // use the 10th block price during 3rd EMA interval
    #[case(29, 9)]
    #[case(30, 19)]
    #[timeout(std::time::Duration::from_millis(100))]
    async fn get_current_ema(#[case] block_count: u64, #[case] price_block_idx: usize) {
        // setup
        let ctx = TestCtx::setup(
            block_count,
            Config {
                price_adjustment_interval: 10,
                ..Config::testnet()
            },
        );
        let desired_block_price = &ctx.prices[price_block_idx];

        // action
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.ema_sender
            .send(EmaServiceMessage::GetCurrentEmaForPricing { response: tx })
            .unwrap();
        let response = rx.await.unwrap();

        // assert
        assert_eq!(response, desired_block_price.ema);
        assert!(!ctx.ema_sender.is_closed());
    }

    mod get_ema_for_next_adjustment_period {
        use super::*;
        use irys_types::storage_pricing::Amount;
        use rust_decimal_macros::dec;
        use test_log::test;

        #[test(tokio::test)]
        async fn first_block() {
            // prepare
            let price_adjustment_interval = 10;
            let config = Config {
                price_adjustment_interval,
                ..Config::testnet()
            };
            let ctx = TestCtx::setup_with_tree(
                genesis_tree(&mut [IrysBlockHeader {
                    height: 0,
                    oracle_irys_price: config.genesis_token_price,
                    ema_irys_price: config.genesis_token_price,
                    ..IrysBlockHeader::new_mock_header()
                }]),
                vec![PriceInfo {
                    oracle: config.genesis_token_price,
                    ema: config.genesis_token_price,
                }],
                config,
            );
            let new_oracle_price = Amount::token(dec!(1.01)).unwrap();

            // action
            let (tx, rx) = tokio::sync::oneshot::channel();
            ctx.ema_sender
                .send(EmaServiceMessage::GetEmaForNewBlock {
                    height_of_new_block: 1,
                    response: tx,
                    oracle_price: new_oracle_price,
                })
                .unwrap();
            let ema_response = rx.await.unwrap().unwrap();

            // assert
            let ema_computed = new_oracle_price
                .calculate_ema(price_adjustment_interval, ctx.config.genesis_token_price)
                .unwrap();
            assert_eq!(ema_computed, ema_response);
            assert_eq!(
                ema_computed,
                Amount::token(dec!(1.0018181818181818181818)).unwrap(),
                "known first magic value when oracle price is 1.01"
            );
        }

        #[test(tokio::test)]
        #[rstest]
        #[case(1)]
        #[case(5)]
        #[case(10)]
        #[case(15)]
        #[case(19)]
        async fn first_and_second_adjustment_period(#[case] block_count: u64) {
            // prepare
            let price_adjustment_interval = 10;
            let ctx = TestCtx::setup(
                block_count,
                Config {
                    price_adjustment_interval,
                    ..Config::testnet()
                },
            );
            let new_oracle_price = rand_price(10);

            // action
            let (tx, rx) = tokio::sync::oneshot::channel();
            ctx.ema_sender
                .send(EmaServiceMessage::GetEmaForNewBlock {
                    height_of_new_block: block_count,
                    response: tx,
                    oracle_price: new_oracle_price,
                })
                .unwrap();
            let ema_response = rx.await.unwrap().unwrap();

            // assert
            let prev_price = ctx.prices[block_count as usize - 1].clone();
            let ema_computed = new_oracle_price
                .calculate_ema(price_adjustment_interval, prev_price.ema)
                .unwrap();
            assert_eq!(ema_computed, ema_response);
        }

        #[test(tokio::test)]
        #[rstest]
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
            ctx.ema_sender
                .send(EmaServiceMessage::GetEmaForNewBlock {
                    height_of_new_block: block_count,
                    response: tx,
                    oracle_price: rand_price(block_count),
                })
                .unwrap();
            let ema_response = rx.await.unwrap().unwrap();

            // assert
            let prev_price = ctx.prices[prev_ema_idx].clone();
            let ema_computed = ctx.prices[prev_ema_predecessor_idx]
                .oracle
                .calculate_ema(price_adjustment_interval, prev_price.ema)
                .unwrap();
            assert_eq!(ema_computed, ema_response);
        }

        #[test(tokio::test)]
        #[rstest]
        #[case(1, 1)]
        #[case(1, 2)]
        #[case(1, 3)]
        #[case(1, 8)]
        #[case(15, 16)]
        #[case(15, 17)]
        #[case(15, 11)]
        #[case(15, 18)]
        async fn nth_ema_block_requsets_non_adjustment_block(
            #[case] block_count: u64,
            #[case] height_of_new_block: u64,
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
            let oracle_price = rand_price(block_count);
            let (tx, rx) = tokio::sync::oneshot::channel();
            ctx.ema_sender
                .send(EmaServiceMessage::GetEmaForNewBlock {
                    height_of_new_block,
                    response: tx,
                    oracle_price,
                })
                .unwrap();
            let ema_response = rx.await;

            // assert
            assert!(ema_response.is_ok());
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
            .ema_sender
            .send(EmaServiceMessage::GetCurrentEmaForPricing { response: tx });

        // assert
        assert!(
            send_result.is_err(),
            "Service should not accept new messages after shutdown"
        );
        assert!(
            ctx.ema_sender.is_closed(),
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
        let expected_final_ema_price = new_blocks[89].ema_irys_price;

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

        // Send a `NewConfirmedBlock` message
        let send_result = ctx.ema_sender.send(EmaServiceMessage::NewConfirmedBlock);
        assert!(
            send_result.is_ok(),
            "Service should accept new confirmed block messages"
        );

        // Verify that the price cache is updated
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.ema_sender
            .send(EmaServiceMessage::GetCurrentEmaForPricing { response: tx })
            .unwrap();
        let response = rx.await.unwrap();
        assert_eq!(
            response, expected_final_ema_price,
            "Price cache should reset correctly"
        );
    }
}
