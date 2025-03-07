use actix::Addr;
use futures::join;
use futures_concurrency::prelude::*;
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
        block_tree: Addr<BlockTreeService>,
        config: &Config,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let genesis_token_price = config.genesis_token_price;
        let blocks_in_interval = config.price_adjustment_interval;
        exec.spawn_critical_with_graceful_shutdown_signal("EMA Service", |shutdown| async move {
            let block_tree_read_guard = block_tree
                .send(GetBlockTreeGuardMessage)
                .await
                .expect("could not retrieve block tree read guard");
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

impl Inner {
    async fn handle_message(&mut self, msg: EmaServiceMessage, context: &mut PriceCacheContext) {
        match msg {
            EmaServiceMessage::GetCurrentEma { response } => {
                let _ = response.send(context.block_two_adj_intervals_ago.irys_price);
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

impl EmaService {
    async fn start(mut self) {
        let mut context = PriceCacheContext::from_canonical_chain(
            self.inner.block_tree_read_guard.clone(),
            self.inner.blocks_in_interval,
        )
        .await;

        let msg_processor = async {
            while let Some(msg) = self.msg_rx.recv().await {
                self.inner.handle_message(msg, &mut context).await;
            }
        };

        let shutdown_handler = async {
            let _ = self.shutdown.await;
        };
        msg_processor.race(shutdown_handler).await;
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.inner.handle_message(msg, &mut context).await;
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
            dbg!(&canonical_chain);
            dbg!(&latest_block_height);

            // Derive indexes
            // if the latest block is an EMA recalculation block
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
            dbg!(&previous_epoch_block_height);
            let block_previous_interval_predecessor = previous_epoch_block_height.saturating_sub(1);
            let two_epochs_ago_block = previous_ema_recalculation_block_height(
                previous_epoch_block_height,
                blocks_in_price_adjustment_interval,
            );

            // fetch the blocks
            let fetch_block_from_idx = async |idx: u64| {
                // todo recalculate the idx based on the latest entry in the `canonical_chain` and the length of the canonical chain
                let (hash, height, ..) = canonical_chain.get(idx as usize).unwrap();
                assert_eq!(
                    previous_epoch_block_height, *height,
                    "height mismatch in the canonical chain data"
                );
                get_block(block_tree_read_guard.clone(), *hash).await
            };
            let block_previous_interval = fetch_block_from_idx(previous_epoch_block_height);
            let block_previous_interval_predecessor =
                fetch_block_from_idx(block_previous_interval_predecessor);
            let block_two_price_intervals_ago = fetch_block_from_idx(two_epochs_ago_block);

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
                    // for the first interval we skip EMA recalculation
                    // we subtract 1 because the block heights are 0 indexed
                    let ema_interval_beginning = blocks_in_price_adjustment_interval
                        .saturating_mul(2)
                        .saturating_sub(1);
                    if latest_block_height < &ema_interval_beginning {
                        ema_interval_beginning
                    } else {
                        previous_epoch_block_height
                            .saturating_add(blocks_in_price_adjustment_interval)
                    }
                },
            }
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
            assert_eq!(price_cache.next_ema_adjustment_height, 19);
            assert_eq!(price_cache.block_previous_interval.height, 0);
            assert_eq!(price_cache.block_previous_interval_predecessor.height, 0);
            assert_eq!(price_cache.block_two_adj_intervals_ago.height, 0);
        }

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
            assert_eq!(price_cache.next_ema_adjustment_height, 19);
            assert_eq!(price_cache.block_previous_interval.height, 0);
            assert_eq!(price_cache.block_previous_interval_predecessor.height, 0);
            assert_eq!(price_cache.block_two_adj_intervals_ago.height, 0);
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
            assert_eq!(price_cache.next_ema_adjustment_height, 19);
            assert_eq!(price_cache.block_previous_interval.height, 0);
            assert_eq!(price_cache.block_previous_interval_predecessor.height, 0);
            assert_eq!(price_cache.block_two_adj_intervals_ago.height, 0);
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
                block_tree_cache.mark_tip(&block_hash).unwrap();
                // let (.., state) = block_tree_cache.get_block_and_status(&block_hash).unwrap();
                // assert_eq!(state, &ChainState::Onchain);
            }

            let block_tree_cache = Arc::new(RwLock::new(block_tree_cache));
            let block_tree_guard = BlockTreeReadGuard::new(block_tree_cache);
            block_tree_guard
        }
    }
}
