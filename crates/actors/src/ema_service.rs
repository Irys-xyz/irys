//! EMA service module.
//!
//! The EMA service is responsible for:
//! - Keeping track of the Irys token price readjustment period.
//! - Validating the EMA price and the oracle price.
//! - Returning the current EMA price.
//! - Returning the price data for the next block.
//!
//! The EMA service keeps track of 2 caches:
//! - Confirmed: The cache of confirmed blocks.
//! - Optimistic: The cache of the longest chain, which also contains blocks
//!   that have not yet gone through the full solution validation.
//!
//! The EMA service will use the confirmed cache to calculate the EMA price for
//! outside usage (eg pricing module, API endpoints).
//! The EMA service will use the optimistic cache to validate the EMA price and
//! the oracle price.
use crate::{
    block_tree_service::{BlockTreeReadGuard, ReorgEvent},
    services::ServiceSenders,
};
use irys_types::{
    is_ema_recalculation_block, previous_ema_recalculation_block_height,
    storage_pricing::{phantoms::Percentage, Amount},
    Config, IrysBlockHeader, IrysTokenPrice,
};
use price_cache_context::{ChainStrategy, Confirmed, Optimistic, PriceCacheContext};
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
use std::{pin::pin, sync::Arc};
use tokio::{
    sync::{broadcast, mpsc::UnboundedReceiver, oneshot},
    task::JoinHandle,
};

/// Messages that the EMA service supports
#[derive(Debug)]
pub enum EmaServiceMessage {
    /// Return the current EMA that other components must use (eg pricing module).
    /// It uses the *confirmed* price context to calculate the EMA.
    GetCurrentEmaForPricing {
        response: oneshot::Sender<IrysTokenPrice>,
    },
    /// Validate that the Oracle prices fall within the expected range.
    /// It uses the optimistic price context to calculate the EMA.
    ValidateOraclePrice {
        block_height: u64,
        oracle_price: IrysTokenPrice,
        response: oneshot::Sender<eyre::Result<PriceStatus>>,
    },
    /// Validate that the EMA price has been correctly derived.
    /// It uses the optimistic price context to calculate the EMA.
    ValidateEmaPrice {
        block_height: u64,
        ema_price: IrysTokenPrice,
        oracle_price: IrysTokenPrice,
        response: oneshot::Sender<eyre::Result<PriceStatus>>,
    },
    /// Returns the EMA irys price that must be used in the next EMA adjustment block
    GetPriceDataForNewBlock {
        height_of_new_block: u64,
        oracle_price: IrysTokenPrice,
        response: oneshot::Sender<eyre::Result<NewBlockEmaResponse>>,
    },
    /// Sent when a block is *confirmed* by the network. The EMA service will refresh its cache
    /// of confirmed blocks.
    BlockConfirmed,
    /// Sent when a block is *prevalidated* by the network. The EMA service will refresh its cache
    /// of prevalidated blocks.
    NewPrevalidatedBlock {
        // will return once the cache has been updated.
        response: oneshot::Sender<()>,
    },
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PriceStatus {
    Valid,
    Invalid,
}

#[derive(Debug)]
pub struct NewBlockEmaResponse {
    pub range_adjusted_oracle_price: IrysTokenPrice,
    pub ema: IrysTokenPrice,
}

#[derive(Debug)]
pub struct EmaService {
    shutdown: GracefulShutdown,
    msg_rx: UnboundedReceiver<EmaServiceMessage>,
    reorg_rx: tokio::sync::broadcast::Receiver<ReorgEvent>,
    inner: Inner,
}

#[derive(Debug)]
struct Inner {
    blocks_in_interval: u64,
    token_price_safe_range: Amount<Percentage>,
    block_tree_read_guard: BlockTreeReadGuard,
    confirmed_price_ctx: PriceCacheContext<Confirmed>,
    optimistic_price_ctx: PriceCacheContext<Optimistic>,
}

impl EmaService {
    /// Spawn a new EMA service
    pub fn spawn_service(
        exec: &TaskExecutor,
        block_tree_guard: BlockTreeReadGuard,
        rx: UnboundedReceiver<EmaServiceMessage>,
        config: &Config,
        service_senders: &ServiceSenders,
    ) -> JoinHandle<()> {
        let blocks_in_interval = config.consensus.ema.price_adjustment_interval;
        let token_price_safe_range = config.consensus.token_price_safe_range;
        let reorg_rx = service_senders.subscribe_reorgs();
        exec.spawn_critical_with_graceful_shutdown_signal("EMA Service", |shutdown| async move {
            let confirmed_price_ctx = PriceCacheContext::<Confirmed>::from_chain(
                block_tree_guard.clone(),
                blocks_in_interval,
            )
            .await
            .expect("initial PriceCacheContext restoration failed");
            let optimistic_price_ctx = PriceCacheContext::<Optimistic>::from_chain(
                block_tree_guard.clone(),
                blocks_in_interval,
            )
            .await
            .expect("initial PriceCacheContext restoration failed");

            let ema_service = Self {
                shutdown,
                msg_rx: rx,
                reorg_rx,
                inner: Inner {
                    optimistic_price_ctx,
                    confirmed_price_ctx,
                    token_price_safe_range,
                    blocks_in_interval,
                    block_tree_read_guard: block_tree_guard,
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

        let mut shutdown_future = pin!(self.shutdown);
        let shutdown_guard = loop {
            tokio::select! {
                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(msg) => {
                            self.inner.handle_message(msg).await?;
                        }
                        None => {
                            tracing::warn!("receiver channel closed");
                            break None;
                        }
                    }
                }
                result = self.reorg_rx.recv() => {
                    if let Some(event) = handle_broadcast_recv(result)? {
                        self.inner.handle_reorg(event).await?;
                    }
                }
                shutdown = &mut shutdown_future => {
                    tracing::warn!("shutdown signal received");
                    break Some(shutdown);
                }
            }
        };

        tracing::debug!(amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");
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
                let _ = response.send(self.confirmed_price_ctx.block_for_pricing.ema_irys_price)
                    .inspect_err(|_| tracing::warn!("current EMA cannot be returned, sender has dropped its half of the channel"));
            }
            EmaServiceMessage::GetPriceDataForNewBlock {
                response,
                height_of_new_block,
                oracle_price,
            } => {
                let ctx = &self.optimistic_price_ctx;
                let next_optimistic_block = ctx.block_previous.height.saturating_add(1);
                if height_of_new_block != next_optimistic_block {
                    let _ = response.send(Err(eyre::eyre!(format!(
                        "EMA Service has not yet been updated with the latest canonical chain {height_of_new_block:}"
                    ))));
                    return Ok(());
                }

                // enforce the min & max acceptable price ranges on the oracle price
                let capped_oracle_price = bound_in_min_max_range(
                    oracle_price,
                    self.token_price_safe_range,
                    ctx.block_previous.oracle_irys_price,
                );
                let new_ema =
                    ctx.get_ema_for_new_block(self.blocks_in_interval, capped_oracle_price);

                tracing::info!(
                    ?new_ema,
                    ?capped_oracle_price,
                    prev_predecessor_height = ?ctx.block_latest_ema_predecessor.height,
                    prev_ema_height = ?ctx.block_latest_ema.height,
                    "computing new EMA"
                );

                let _ = response.send(Ok(NewBlockEmaResponse {
                    range_adjusted_oracle_price: capped_oracle_price,
                    ema: new_ema,
                }));
            }
            EmaServiceMessage::BlockConfirmed => {
                tracing::debug!("updating confirmed price cache");

                // Rebuild the entire data cache just like we do at startup.
                self.confirmed_price_ctx = PriceCacheContext::<Confirmed>::from_chain(
                    self.block_tree_read_guard.clone(),
                    self.blocks_in_interval,
                )
                .await?;
            }
            EmaServiceMessage::NewPrevalidatedBlock { response } => {
                tracing::debug!("updating optimistic price cache");

                // Rebuild the entire data cache just like we do at startup.
                self.optimistic_price_ctx = PriceCacheContext::<Optimistic>::from_chain(
                    self.block_tree_read_guard.clone(),
                    self.blocks_in_interval,
                )
                .await?;

                let _ = response.send(());
            }
            EmaServiceMessage::ValidateOraclePrice {
                block_height,
                oracle_price,
                response,
            } => {
                tracing::debug!("validating oracle price");

                let price_status = self
                    .validate_oracle_price(block_height, oracle_price)
                    .await?;

                let _ = response.send(Ok(price_status));
            }
            EmaServiceMessage::ValidateEmaPrice {
                block_height,
                ema_price,
                oracle_price,
                response,
            } => {
                tracing::debug!("validating EMA price");

                let status = self
                    .valid_ema_price(block_height, ema_price, oracle_price)
                    .await?;

                let _ = response.send(Ok(status));
            }
        }
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn valid_ema_price(
        &self,
        block_height: u64,
        ema_price: IrysTokenPrice,
        oracle_price: IrysTokenPrice,
    ) -> Result<PriceStatus, eyre::Error> {
        // rebuild a new price context from historical data
        //
        // TODO: CANONICAL CHAIN HAS IT'S OWN CACHE CLEANUP STRATEGY;
        // IF THE BLOCK IS NOT AVAILABLE IN THE CACHE THEN WE CANNOT VALIDATE IT.
        // ADD LOGIC TO READ IT FROM THE DB IN THAT CASE?
        let temp_price_context = PriceCacheContext::<Optimistic>::from_chain_subset(
            self.block_tree_read_guard.clone(),
            self.blocks_in_interval,
            // subtract one because we want to simulate the price for *before* the block was mined
            block_height.saturating_sub(1),
        )
        .await?;

        // calculate the new EMA using historical oracle price + ema price
        let new_ema =
            temp_price_context.get_ema_for_new_block(self.blocks_in_interval, oracle_price);

        // check if the prices match
        let status = if ema_price == new_ema {
            PriceStatus::Valid
        } else {
            PriceStatus::Invalid
        };
        Ok(status)
    }

    #[tracing::instrument(skip(self))]
    async fn validate_oracle_price(
        &self,
        block_height: u64,
        oracle_price: IrysTokenPrice,
    ) -> Result<PriceStatus, eyre::Error> {
        // Get the previous block
        let prev_block = block_height.saturating_sub(1);
        let prev_block = self
            .fetch_block_using_local_cache::<Optimistic>(prev_block)
            .await?;

        // check if the oracle price is valid
        let capped_oracle_price = bound_in_min_max_range(
            oracle_price,
            self.token_price_safe_range,
            prev_block.oracle_irys_price,
        );
        let price_status = if oracle_price == capped_oracle_price {
            PriceStatus::Valid
        } else {
            PriceStatus::Invalid
        };
        Ok(price_status)
    }

    #[tracing::instrument(skip(self))]
    async fn fetch_block_using_local_cache<T: ChainStrategy>(
        &self,
        desired_height: u64,
    ) -> Result<Arc<IrysBlockHeader>, eyre::Error> {
        let cached_header = [
            // check confirmed entries
            &self.confirmed_price_ctx.block_latest_ema,
            &self.confirmed_price_ctx.block_latest_ema_predecessor,
            &self.confirmed_price_ctx.block_previous,
            // check optimistic entries
            &self.optimistic_price_ctx.block_latest_ema,
            &self.optimistic_price_ctx.block_latest_ema_predecessor,
            &self.optimistic_price_ctx.block_previous,
        ]
        .into_iter()
        .find(|hdr| hdr.height == desired_height);
        let block_header = if let Some(cached_header) = cached_header {
            Arc::clone(cached_header)
        } else {
            let chain = T::get_chain(self.block_tree_read_guard.clone()).await?;
            let (_, latest_block_height, ..) = chain.last().expect("optimistic chain is empty");

            // Attempt to find the needed block in the chain
            price_cache_context::fetch_block_with_height(
                desired_height,
                self.block_tree_read_guard.clone(),
                &chain,
                *latest_block_height,
            )
            .await?
        };
        Ok(block_header)
    }

    #[tracing::instrument(skip_all, err)]
    async fn handle_reorg(&mut self, event: ReorgEvent) -> eyre::Result<()> {
        tracing::info!(
            new_tip = ?event.new_tip,
            new_height = ?event.fork_parent.height,
            "Processing reorg"
        );

        // Rebuild both price contexts based on new canonical chain
        // It's guaranteed by the block tree service that the block
        // tree read guard already has the latest data before the event is sent.
        self.confirmed_price_ctx = PriceCacheContext::<Confirmed>::from_chain(
            self.block_tree_read_guard.clone(),
            self.blocks_in_interval,
        )
        .await?;

        self.optimistic_price_ctx = PriceCacheContext::<Optimistic>::from_chain(
            self.block_tree_read_guard.clone(),
            self.blocks_in_interval,
        )
        .await?;

        tracing::info!("EMA service caches updated after reorg");
        Ok(())
    }
}

/// Handle broadcast channel receive results
#[tracing::instrument(skip_all, err)]
fn handle_broadcast_recv<T>(
    result: Result<T, broadcast::error::RecvError>,
) -> eyre::Result<Option<T>> {
    match result {
        Ok(event) => Ok(Some(event)),
        Err(broadcast::error::RecvError::Closed) => {
            eyre::bail!("broadcast channel closed")
        }
        Err(broadcast::error::RecvError::Lagged(n)) => {
            tracing::warn!(skipped_messages = ?n, "reorg lagged");
            if n > 5 {
                tracing::error!("reorg channel significantly lagged");
            }
            Ok(None)
        }
    }
}

/// Cap the provided price value to fit within the max / min acceptable range.
/// The range is defined by the `token_price_safe_range` percentile value.
///
/// Use the previous blocks oracle price as the base value.
#[tracing::instrument]
fn bound_in_min_max_range(
    desired_price: IrysTokenPrice,
    safe_range: Amount<Percentage>,
    base_price: IrysTokenPrice,
) -> IrysTokenPrice {
    let max_acceptable = base_price.add_multiplier(safe_range).unwrap_or(base_price);
    let min_acceptable = base_price.sub_multiplier(safe_range).unwrap_or(base_price);
    if desired_price > max_acceptable {
        tracing::warn!(
            ?max_acceptable,
            ?desired_price,
            "oracle price too high, capping"
        );
        return max_acceptable;
    }
    if desired_price < min_acceptable {
        tracing::warn!(
            ?min_acceptable,
            ?desired_price,
            "oracle price too low, capping"
        );
        return min_acceptable;
    }
    desired_price
}

/// Utility module for that's responsible for extracting the desired blocks from the
/// `BlockTree` to properly report prices & calculate new interval values
mod price_cache_context {
    use std::{marker::PhantomData, sync::Arc};

    use eyre::{ensure, OptionExt as _};
    use futures::try_join;
    use irys_types::{block_height_to_use_for_price, H256};

    use crate::block_tree_service::{get_block, get_canonical_chain, get_optimistic_chain};

    use super::*;

    #[derive(Debug, Clone)]
    pub(super) struct Optimistic;
    #[derive(Debug, Clone)]
    pub(super) struct Confirmed;

    pub(super) trait ChainStrategy {
        async fn get_chain(
            block_tree_read_guard: BlockTreeReadGuard,
        ) -> eyre::Result<Vec<(H256, u64)>>;
    }

    impl ChainStrategy for Optimistic {
        async fn get_chain(
            block_tree_read_guard: BlockTreeReadGuard,
        ) -> eyre::Result<Vec<(H256, u64)>> {
            get_optimistic_chain(block_tree_read_guard).await
        }
    }

    impl ChainStrategy for Confirmed {
        async fn get_chain(
            block_tree_read_guard: BlockTreeReadGuard,
        ) -> eyre::Result<Vec<(H256, u64)>> {
            let chain = get_canonical_chain(block_tree_read_guard).await?.0;
            let chain = chain
                .into_iter()
                .map(|e| (e.block_hash, e.height))
                .collect();
            Ok(chain)
        }
    }

    #[derive(Debug, Clone)]
    pub(super) struct PriceCacheContext<T: ChainStrategy> {
        pub(super) block_latest_ema: Arc<IrysBlockHeader>,
        pub(super) block_latest_ema_predecessor: Arc<IrysBlockHeader>,
        pub(super) block_for_pricing: Arc<IrysBlockHeader>,
        pub(super) block_previous: Arc<IrysBlockHeader>,
        pub(super) chain_strategy: PhantomData<T>,
    }

    impl<T: ChainStrategy> PriceCacheContext<T> {
        /// Builds the entire context from scratch by scanning the canonical chain, but only from a certain point.
        /// Used for historical price data validation.
        pub(super) async fn from_chain_subset(
            block_tree_read_guard: BlockTreeReadGuard,
            blocks_in_price_adjustment_interval: u64,
            max_height: u64,
        ) -> eyre::Result<Self> {
            // Rebuild the entire data cache just like we do at startup.
            let canonical_chain = T::get_chain(block_tree_read_guard.clone()).await?;
            let (_latest_block_hash, latest_block_height, ..) = canonical_chain
                .last()
                .ok_or_eyre("canonical chain is empty")?;
            ensure!(
                *latest_block_height >= max_height,
                "the provided max height exceeds the one registered in the canonical chain"
            );
            let diff_from_latest_height = latest_block_height.abs_diff(max_height);
            let new_len = canonical_chain
                .len()
                .saturating_sub(diff_from_latest_height as usize);
            let canonical_chain_subset = &canonical_chain[..new_len];
            let last_item = &canonical_chain_subset
                .last()
                .ok_or_eyre("chain subset is empty")?
                .1;
            ensure!(
                max_height == *last_item,
                "height mismatch in the canonical chain data"
            );
            Self::with_chain_and_height(
                max_height,
                block_tree_read_guard,
                blocks_in_price_adjustment_interval,
                canonical_chain_subset,
            )
            .await
        }

        /// Builds the entire context from scratch by scanning the canonical chain.
        pub(super) async fn from_chain(
            block_tree_read_guard: BlockTreeReadGuard,
            blocks_in_price_adjustment_interval: u64,
        ) -> eyre::Result<Self> {
            let canonical_chain = T::get_chain(block_tree_read_guard.clone()).await?;
            let (_latest_block_hash, latest_block_height, ..) =
                canonical_chain.last().expect("canonical chain is empty");
            Self::with_chain_and_height(
                *latest_block_height,
                block_tree_read_guard,
                blocks_in_price_adjustment_interval,
                &canonical_chain,
            )
            .await
        }

        async fn with_chain_and_height(
            latest_block_height: u64,
            block_tree_read_guard: BlockTreeReadGuard,
            blocks_in_price_adjustment_interval: u64,
            canonical_chain: &[(irys_types::H256, u64)],
        ) -> eyre::Result<Self> {
            let height_pricing_block = block_height_to_use_for_price(
                latest_block_height,
                blocks_in_price_adjustment_interval,
            );
            let height_latest_ema_block = if is_ema_recalculation_block(
                latest_block_height,
                blocks_in_price_adjustment_interval,
            ) {
                latest_block_height
            } else {
                // Derive indexes
                previous_ema_recalculation_block_height(
                    latest_block_height,
                    blocks_in_price_adjustment_interval,
                )
            };
            let height_latest_ema_interval_predecessor = height_latest_ema_block.saturating_sub(1);

            // utility fn to fetch the block at a given height
            let fetch_block_with_height = async |height: u64| {
                fetch_block_with_height(
                    height,
                    block_tree_read_guard.clone(),
                    canonical_chain,
                    latest_block_height,
                )
                .await
            };

            // fetch the blocks concurrently
            let (block_latest_ema, block_latest_ema_predecessor, block_previous, block_for_pricing) =
                try_join!(
                    fetch_block_with_height(height_latest_ema_block),
                    fetch_block_with_height(height_latest_ema_interval_predecessor),
                    fetch_block_with_height(latest_block_height),
                    fetch_block_with_height(height_pricing_block)
                )?;

            // Return an updated price cache
            Ok(Self {
                block_latest_ema,
                block_latest_ema_predecessor,
                block_previous,
                block_for_pricing,
                chain_strategy: PhantomData,
            })
        }

        #[tracing::instrument(skip_all)]
        pub(crate) fn get_ema_for_new_block(
            &self,
            blocks_in_interval: u64,
            oracle_price: IrysTokenPrice,
        ) -> IrysTokenPrice {
            let oracle_price_to_use = if self.block_previous.height < (blocks_in_interval * 2) {
                oracle_price
            } else {
                self.block_latest_ema_predecessor.oracle_irys_price
            };
            // the first 2 adjustment intervals have special handling where we calculate the
            // EMA for each block using the value from the preceding one.
            //
            // But the generic case:
            // example EMA calculation on block 29:
            // 1. take the registered Oracle Irys price in block 18
            //    and the stored EMA Irys price in block 19.
            // 2. using these values compute EMA for block 29. In this case
            //    the *n* (number of block prices) would be 10 (E29.height - E19.height).
            // 3. this is the price that will be used in the interval 39->49,
            //    which will be reported to other systems querying for EMA prices.

            // calculate the new EMA using historical oracle price + ema price

            oracle_price_to_use
                .calculate_ema(
                    // note: we calculate the EMA for each block, but we don't calculate relative intervals between them
                    blocks_in_interval,
                    self.block_latest_ema.ema_irys_price,
                )
                .unwrap_or_else(|err| {
                    tracing::warn!(?err, "price overflow, using previous EMA price");
                    self.block_latest_ema.ema_irys_price
                })
        }
    }

    pub(crate) async fn fetch_block_with_height(
        height: u64,
        block_tree_read_guard: BlockTreeReadGuard,
        chain: &[(irys_types::H256, u64)],
        latest_block_height: u64,
    ) -> Result<Arc<IrysBlockHeader>, eyre::Error> {
        let chain_len = chain.len();
        let diff_from_latest_height = latest_block_height.saturating_sub(height) as usize;
        let adjusted_index = chain_len
            .saturating_sub(diff_from_latest_height)
            .saturating_sub(1); // -1 because heights are zero based
        let (hash, new_height, ..) = chain
            .get(adjusted_index)
            .expect("the block at the index to be present");
        assert_eq!(
            height, *new_height,
            "height mismatch in the canonical chain data"
        );
        let block = get_block(block_tree_read_guard, *hash).await?;
        block.ok_or_else(|| {
            eyre::eyre!(
                "block hash {hash:?} from canonical chain cannot be retrieved from the block index"
            )
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::block_tree_service::ChainState;
        use rstest::rstest;

        #[test_log::test(tokio::test)]
        #[rstest]
        #[case(0, 0, 0, 0)]
        #[case(1, 0, 1, 0)]
        #[case(10, 0, 10, 9)]
        #[case(18, 0, 18, 17)]
        #[case(19, 0, 19, 18)]
        #[case(20, 9, 19, 18)]
        #[case(85, 69, 79, 78)]
        #[case(90, 79, 89, 88)]
        #[case(99, 79, 99, 98)]
        async fn test_valid_price_cache(
            #[case] height_latest_block: u64,
            #[case] height_for_pricing: u64,
            #[case] height_current_ema: u64,
            #[case] height_current_ema_predecessor: u64,
        ) {
            // setup
            use crate::block_tree_service::{test_utils::genesis_tree, BlockState};
            let interval = 10;
            let threshold = height_latest_block / 2;
            let mut blocks = (0..=height_latest_block)
                .map(|height| {
                    let block = IrysBlockHeader {
                        height,
                        ..IrysBlockHeader::new_mock_header()
                    };

                    // Determine chain state based on block height
                    let state = if block.height <= threshold {
                        ChainState::Onchain
                    } else {
                        ChainState::NotOnchain(BlockState::ValidationScheduled)
                    };

                    (block, state)
                })
                .collect::<Vec<_>>();
            let block_tree_guard = genesis_tree(&mut blocks);

            // action
            let price_cache =
                PriceCacheContext::<Optimistic>::from_chain(block_tree_guard, interval)
                    .await
                    .unwrap();

            // assert
            assert_eq!(
                price_cache.block_for_pricing.height, height_for_pricing,
                "invalid ema 2 intervals ago"
            );
            assert_eq!(price_cache.block_latest_ema.height, height_current_ema);
            assert_eq!(
                price_cache.block_latest_ema_predecessor.height,
                height_current_ema_predecessor
            );
        }

        #[cfg(test)]
        mod from_canonical_chain_subset_tests {
            use super::*;
            use crate::block_tree_service::test_utils::genesis_tree;
            use crate::block_tree_service::BlockState;
            use rstest::rstest;

            /// Parameterized test for from_canonical_chain_subset with varying chain_length, max_height,
            /// and the expected "interval" blocks. In some cases below, `max_height` is less than the top,
            /// which can validate that it correctly slices the canonical chain.
            #[test_log::test(tokio::test)]
            #[rstest]
            #[case(15, 10, 10, 9, 0)]
            #[case(19, 19, 19, 18, 0)]
            #[case(20, 20, 19, 18, 9)]
            #[case(20, 15, 15, 14, 0)]
            #[case(30, 25, 19, 18, 9)]
            #[case(30, 28, 19, 18, 9)]
            #[case(40, 38, 29, 28, 19)]
            #[case(70, 65, 59, 58, 49)]
            async fn test_from_canonical_chain_subset(
                #[case] height_chain_max: u64,
                #[case] height_chain_subset_max: u64,
                #[case] height_exp_latest_ema: u64,
                #[case] height_exp_latest_ema_pred: u64,
                #[case] height_exp_pricing: u64,
            ) {
                // Setup
                let blocks_in_interval = 10;
                let max_confirmed_height = height_chain_max / 2;
                let mut blocks = (0..=height_chain_max)
                    .map(|height| {
                        let block = IrysBlockHeader {
                            height,
                            ..IrysBlockHeader::new_mock_header()
                        };

                        // Determine chain state based on block height
                        let state = if block.height <= max_confirmed_height {
                            ChainState::Onchain
                        } else {
                            ChainState::NotOnchain(BlockState::ValidationScheduled)
                        };

                        (block, state)
                    })
                    .collect::<Vec<_>>();
                assert_eq!(blocks.len(), (height_chain_max + 1) as usize);
                let block_tree_guard = genesis_tree(&mut blocks);

                // Action
                // check optimistic context
                let optimistic_ctx = PriceCacheContext::<Optimistic>::from_chain_subset(
                    block_tree_guard.clone(),
                    blocks_in_interval,
                    height_chain_subset_max,
                )
                .await
                .unwrap();
                let confirmed_ctx = PriceCacheContext::<Confirmed>::from_chain_subset(
                    block_tree_guard,
                    max_confirmed_height,
                    max_confirmed_height,
                )
                .await
                .unwrap();

                // Assert
                assert_eq!(
                    optimistic_ctx.block_latest_ema.height,
                    height_exp_latest_ema
                );
                assert_eq!(
                    optimistic_ctx.block_latest_ema_predecessor.height,
                    height_exp_latest_ema_pred
                );
                assert_eq!(optimistic_ctx.block_for_pricing.height, height_exp_pricing);

                // check confirmed context
                assert_eq!(confirmed_ctx.block_latest_ema.height, max_confirmed_height);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_tree_service::test_utils::{
        build_tree, create_and_apply_fork, deterministic_price, setup_chain_for_fork_test,
        PriceInfo, TestCtx,
    };
    use crate::block_tree_service::{get_canonical_chain, ChainState};
    use crate::CommitmentSnapshot;
    use irys_types::{
        block_height_to_use_for_price, ConsensusConfig, ConsensusOptions, EmaConfig, NodeConfig,
        H256,
    };

    use rstest::rstest;

    use test_log::test;

    fn spawn_ema(ctx: &TestCtx, rx: UnboundedReceiver<EmaServiceMessage>) {
        let _handle = EmaService::spawn_service(
            &ctx.task_executor,
            ctx.guard.clone(),
            rx,
            &ctx.config,
            &ctx.service_senders,
        );
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
    async fn get_current_ema(#[case] max_block_height: u64, #[case] price_block_idx: usize) {
        // setup
        let (ctx, rxs) = TestCtx::setup(
            max_block_height,
            Config::new(NodeConfig {
                consensus: ConsensusOptions::Custom(ConsensusConfig {
                    ema: EmaConfig {
                        price_adjustment_interval: 10,
                    },
                    ..ConsensusConfig::testnet()
                }),
                ..NodeConfig::testnet()
            }),
        );
        spawn_ema(&ctx, rxs.ema);
        let desired_block_price = &ctx.prices[price_block_idx];

        // action
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.service_senders
            .ema
            .send(EmaServiceMessage::GetCurrentEmaForPricing { response: tx })
            .unwrap();
        let response = rx.await.unwrap();

        // assert
        assert_eq!(response, desired_block_price.ema);
        assert!(!ctx.service_senders.ema.is_closed());
        drop(ctx);
    }

    mod get_ema_for_next_adjustment_period {
        use crate::block_tree_service::test_utils::genesis_tree;

        use super::*;
        use irys_types::storage_pricing::Amount;
        use rust_decimal_macros::dec;
        use test_log::test;

        #[test(tokio::test)]
        async fn first_block() {
            // prepare
            let price_adjustment_interval = 10;
            let config = Config::new(NodeConfig {
                consensus: ConsensusOptions::Custom(ConsensusConfig {
                    ema: EmaConfig {
                        price_adjustment_interval,
                    },
                    ..ConsensusConfig::testnet()
                }),
                ..NodeConfig::testnet()
            });
            let (ctx, rxs) = TestCtx::setup_with_tree(
                genesis_tree(&mut [(
                    IrysBlockHeader {
                        height: 0,
                        oracle_irys_price: config.consensus.genesis_price,
                        ema_irys_price: config.consensus.genesis_price,
                        ..IrysBlockHeader::new_mock_header()
                    },
                    ChainState::Onchain,
                )]),
                vec![PriceInfo {
                    oracle: config.consensus.genesis_price,
                    ema: config.consensus.genesis_price,
                }],
                config,
            );
            spawn_ema(&ctx, rxs.ema);
            let new_oracle_price = Amount::token(dec!(1.01)).unwrap();

            // action - get EMA for new block
            let ema_response = ctx
                .get_prices_for_new_block(1, new_oracle_price)
                .await
                .unwrap()
                .ema;
            // action - check if the returned EMA is valid

            // assert
            let ema_computed = new_oracle_price
                .calculate_ema(
                    price_adjustment_interval,
                    ctx.config.consensus.genesis_price,
                )
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
        async fn first_and_second_adjustment_period(#[case] max_height: u64) {
            // prepare
            let price_adjustment_interval = 10;
            let (ctx, rxs) = TestCtx::setup(
                max_height,
                Config::new(NodeConfig {
                    consensus: ConsensusOptions::Custom(ConsensusConfig {
                        ema: EmaConfig {
                            price_adjustment_interval,
                        },
                        ..ConsensusConfig::testnet()
                    }),
                    ..NodeConfig::testnet()
                }),
            );
            spawn_ema(&ctx, rxs.ema);

            let new_oracle_price = deterministic_price(max_height);

            // action
            let (tx, rx) = tokio::sync::oneshot::channel();
            ctx.service_senders
                .ema
                .send(EmaServiceMessage::GetPriceDataForNewBlock {
                    height_of_new_block: max_height + 1,
                    response: tx,
                    oracle_price: new_oracle_price,
                })
                .unwrap();
            let ema_response = rx.await.unwrap().unwrap().ema;

            // assert
            let prev_price = ctx.prices.last().unwrap().clone();
            let ema_computed = new_oracle_price
                .calculate_ema(price_adjustment_interval, prev_price.ema)
                .unwrap();
            assert_eq!(ema_computed, ema_response);
        }

        #[test(tokio::test)]
        #[rstest]
        #[case(5)]
        #[case(8)]
        #[case(9)]
        #[case(20)]
        #[case(30)]
        #[case(15)]
        #[case(19)]
        #[case(20)]
        #[case(29)]
        #[case(28)]
        #[case(27)]
        async fn oracle_price_gets_capped(#[case] max_height: u64) {
            // prepare
            let price_adjustment_interval = 10;
            let token_price_safe_range = Amount::percentage(dec!(0.1)).unwrap();
            let (ctx, rxs) = TestCtx::setup(
                max_height,
                Config::new(NodeConfig {
                    consensus: ConsensusOptions::Custom(ConsensusConfig {
                        ema: EmaConfig {
                            price_adjustment_interval,
                        },
                        token_price_safe_range,
                        ..ConsensusConfig::testnet()
                    }),
                    ..NodeConfig::testnet()
                }),
            );
            spawn_ema(&ctx, rxs.ema);

            let price_oracle_latest = ctx.prices.last().unwrap().clone().oracle;
            let mul_outside_of_range = Amount::percentage(dec!(0.101)).unwrap();
            let oracle_prices: &[IrysTokenPrice] = &[
                price_oracle_latest
                    .add_multiplier(mul_outside_of_range)
                    .unwrap(),
                price_oracle_latest
                    .sub_multiplier(mul_outside_of_range)
                    .unwrap(),
            ];

            for oracle_price in oracle_prices {
                // action
                let new_block_height = max_height + 1;
                let capped_oracle_price = ctx
                    .get_prices_for_new_block(new_block_height, *oracle_price)
                    .await
                    .unwrap()
                    .range_adjusted_oracle_price;
                let price_status_original = ctx
                    .validate_oracle_price(new_block_height, *oracle_price)
                    .await
                    .unwrap();
                let price_status_capped = ctx
                    .validate_oracle_price(new_block_height, capped_oracle_price)
                    .await
                    .unwrap();

                // assert
                assert_ne!(
                    &capped_oracle_price, oracle_price,
                    "the oracle price must differ"
                );
                assert_eq!(price_status_original, PriceStatus::Invalid);
                assert_eq!(price_status_capped, PriceStatus::Valid);
            }
        }

        #[test(tokio::test)]
        #[rstest]
        #[case(28, 19, 18)]
        #[case(38, 29, 28)]
        #[case(168, 159, 158)]
        async fn nth_adjustment_period(
            #[case] max_height: u64,
            #[case] prev_ema_height: usize,
            #[case] prev_ema_predecessor_height: usize,
        ) {
            // prepare
            let price_adjustment_interval = 10;
            let (ctx, rxs) = TestCtx::setup(
                max_height,
                Config::new(NodeConfig {
                    consensus: ConsensusOptions::Custom(ConsensusConfig {
                        ema: EmaConfig {
                            price_adjustment_interval,
                        },
                        ..ConsensusConfig::testnet()
                    }),
                    ..NodeConfig::testnet()
                }),
            );
            spawn_ema(&ctx, rxs.ema);

            // action
            let new_block_height = max_height + 1;
            let response = ctx
                .get_prices_for_new_block(new_block_height, deterministic_price(max_height))
                .await
                .unwrap();
            let price_status_original = ctx
                .validate_ema_price(
                    new_block_height,
                    response.ema,
                    response.range_adjusted_oracle_price,
                )
                .await
                .unwrap();

            // assert
            let prev_price = ctx.prices[prev_ema_height].clone();
            let ema_computed = ctx.prices[prev_ema_predecessor_height]
                .oracle
                .calculate_ema(price_adjustment_interval, prev_price.ema)
                .unwrap();
            assert!(is_ema_recalculation_block(
                new_block_height,
                price_adjustment_interval
            ));
            assert_eq!(ema_computed, response.ema);
            assert_eq!(price_status_original, PriceStatus::Valid);
        }

        #[test(tokio::test)]
        #[rstest]
        #[case(1, 1)]
        #[case(1, 3)]
        #[case(1, 8)]
        #[case(15, 17)]
        #[case(15, 18)]
        async fn invalid_block_height_requested(
            #[case] max_block_height: u64,
            #[case] height_of_new_block: u64,
        ) {
            // prepare
            let price_adjustment_interval = 10;
            let (ctx, rxs) = TestCtx::setup(
                max_block_height,
                Config::new(NodeConfig {
                    consensus: ConsensusOptions::Custom(ConsensusConfig {
                        ema: EmaConfig {
                            price_adjustment_interval,
                        },
                        ..ConsensusConfig::testnet()
                    }),
                    ..NodeConfig::testnet()
                }),
            );
            spawn_ema(&ctx, rxs.ema);

            // action
            let oracle_price = deterministic_price(max_block_height);
            let res = ctx
                .get_prices_for_new_block(height_of_new_block, oracle_price)
                .await;

            // assert
            assert!(res.is_err(), "a block height that big should be rejected");
        }
    }

    #[test(tokio::test)]
    #[rstest]
    #[timeout(std::time::Duration::from_secs(3))]
    async fn test_ema_service_shutdown_no_pending_messages() {
        // Setup
        let block_count = 3;
        let price_adjustment_interval = 10;
        let (ctx, rxs) = TestCtx::setup(
            block_count,
            Config::new(NodeConfig {
                consensus: ConsensusOptions::Custom(ConsensusConfig {
                    ema: EmaConfig {
                        price_adjustment_interval,
                    },
                    ..ConsensusConfig::testnet()
                }),
                ..NodeConfig::testnet()
            }),
        );
        spawn_ema(&ctx, rxs.ema);

        // Send shutdown signal
        tokio::task::spawn_blocking(|| {
            ctx.task_manager.graceful_shutdown();
        })
        .await
        .unwrap();

        // Attempt to send a message and ensure it fails
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let send_result = ctx
            .service_senders
            .ema
            .send(EmaServiceMessage::GetCurrentEmaForPricing { response: tx });

        // assert
        assert!(
            send_result.is_err(),
            "Service should not accept new messages after shutdown"
        );
        assert!(
            ctx.service_senders.ema.is_closed(),
            "Service sender should be closed"
        );
    }

    #[test(tokio::test)]
    async fn test_ema_service_new_confirmed_block() {
        // Setup
        let initial_block_count = 10;
        let price_adjustment_interval = 10;
        let (ctx, rxs) = TestCtx::setup(
            initial_block_count,
            Config::new(NodeConfig {
                consensus: ConsensusOptions::Custom(ConsensusConfig {
                    ema: EmaConfig {
                        price_adjustment_interval,
                    },
                    ..ConsensusConfig::testnet()
                }),
                ..NodeConfig::testnet()
            }),
        );
        spawn_ema(&ctx, rxs.ema);

        // setup -- generate new blocks to be added
        let (chain, ..) = get_canonical_chain(ctx.guard.clone()).await.unwrap();
        let latest = chain.last().unwrap();
        let (new_blocks, ..) = build_tree(initial_block_count, 100);
        let mut latest_block_hash = latest.block_hash;

        // extract the final price that we expect.
        // in total there are 110 blocks once we add the new ones.
        // The EMA to use is the 100th block (idx 89 in the `new_blocks`).
        let expected_final_ema_price = new_blocks[89].ema_irys_price;

        // setup  -- add new blocks to the canonical chain post-initializatoin
        {
            let mut tree = ctx.guard.write();
            for mut block in new_blocks {
                block.previous_block_hash = latest_block_hash;
                block.cumulative_diff = block.height.into();
                latest_block_hash = H256::random();
                block.block_hash = latest_block_hash;
                tree.add_common(
                    block.block_hash,
                    &block,
                    Arc::new(CommitmentSnapshot::default()),
                    ChainState::Onchain,
                )
                .unwrap();
            }
            drop(tree)
        };

        // Send a `NewConfirmedBlock` message
        let send_result = ctx
            .service_senders
            .ema
            .send(EmaServiceMessage::BlockConfirmed);
        assert!(
            send_result.is_ok(),
            "Service should accept new confirmed block messages"
        );

        // Verify that the price cache is updated
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.service_senders
            .ema
            .send(EmaServiceMessage::GetCurrentEmaForPricing { response: tx })
            .unwrap();
        let response = rx.await.unwrap();
        assert_eq!(
            response, expected_final_ema_price,
            "Price cache should reset correctly"
        );
    }

    /// Create a simple fork scenario and just check if the service still works after processing the fork event
    #[test(tokio::test)]
    async fn test_ema_service_reorg_basic() {
        // Step 1: Create clean chain state without any forks
        let (block_tree_guard, prices) = setup_chain_for_fork_test(13); // height 10 + 3 blocks for old fork

        let config = Config::new(NodeConfig {
            consensus: ConsensusOptions::Custom(ConsensusConfig {
                ema: EmaConfig {
                    price_adjustment_interval: 10,
                },
                ..ConsensusConfig::testnet()
            }),
            ..NodeConfig::testnet()
        });

        // Step 2: Initialize EMA service with clean block tree (no forks)
        let (ctx, rxs) = TestCtx::setup_with_tree(block_tree_guard.clone(), prices.clone(), config);
        spawn_ema(&ctx, rxs.ema);

        // Get initial EMA price before creating fork
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.service_senders
            .ema
            .send(EmaServiceMessage::GetCurrentEmaForPricing { response: tx })
            .unwrap();
        let initial_ema = rx.await.unwrap();

        // Initial EMA should be the genesis price (height 0) since we're in the first interval
        assert_eq!(
            initial_ema, prices[0].ema,
            "Initial EMA should match genesis price"
        );

        // Step 3: Now create fork on the block tree after EMA service is initialized
        // Fork from height 10, creating a new chain that ends at height 14
        let (reorg_event, _fork_prices) =
            create_and_apply_fork(&block_tree_guard, 14, 10, ChainState::Onchain);

        // Step 4: Trigger reorg
        ctx.trigger_reorg(reorg_event).await;

        // Step 5: Verify the EMA price after reorg
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.service_senders
            .ema
            .send(EmaServiceMessage::GetCurrentEmaForPricing { response: tx })
            .unwrap();
        let post_reorg_ema = rx.await.unwrap();

        // After reorg, the chain height is 14 (fork at 10 + new fork length 4)
        // Since we're still in the first price interval (< 20), the EMA should still be the genesis price
        assert_eq!(
            post_reorg_ema, prices[0].ema,
            "Post-reorg EMA should still be genesis price"
        );

        // Verify service is responsive and can calculate prices for the next block
        let new_height = 15;
        let oracle_price = deterministic_price(new_height);
        let result = ctx.get_prices_for_new_block(new_height, oracle_price).await;
        assert!(
            result.is_ok(),
            "Should be able to calculate prices for next block"
        );
    }

    #[test(tokio::test)]
    async fn test_ema_service_reorg_multiple_intervals() {
        // Step 1: Create clean chain state without any forks
        let (block_tree_guard, prices) = setup_chain_for_fork_test(30);

        let config = Config::new(NodeConfig {
            consensus: ConsensusOptions::Custom(ConsensusConfig {
                ema: EmaConfig {
                    price_adjustment_interval: 10,
                },
                ..ConsensusConfig::testnet()
            }),
            ..NodeConfig::testnet()
        });

        // Step 2: Initialize EMA service with clean block tree (no forks)
        let (ctx, rxs) = TestCtx::setup_with_tree(block_tree_guard.clone(), prices.clone(), config);
        spawn_ema(&ctx, rxs.ema);

        // Verify initial state crosses multiple EMA intervals
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.service_senders
            .ema
            .send(EmaServiceMessage::GetCurrentEmaForPricing { response: tx })
            .unwrap();
        let initial_ema = rx.await.unwrap();

        // At height 30, we're in the 4th interval (30-39)
        // The pricing block is from 2 intervals ago: block 19 (the last block of interval 10-19)
        let pricing_height = block_height_to_use_for_price(30, 10);
        assert_eq!(
            pricing_height, 19,
            "At height 30, should use price from block 19"
        );
        assert_eq!(
            initial_ema, prices[19].ema,
            "Initial EMA should be from height 19 (2 intervals ago)"
        );

        // Step 3: Create fork with height 30 (crossing multiple EMA intervals)
        // Fork from height 15, creating a new chain that ends at height 30
        let (reorg_event, fork_prices) =
            create_and_apply_fork(&block_tree_guard, 30, 15, ChainState::Onchain);

        // Step 4: Trigger reorg
        ctx.trigger_reorg(reorg_event).await;

        // Step 5: Verify EMA after reorg
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.service_senders
            .ema
            .send(EmaServiceMessage::GetCurrentEmaForPricing { response: tx })
            .unwrap();
        let post_reorg_ema = rx.await.unwrap();

        // After reorg, chain height is 30 (new fork ends at 30)
        // At height 30, we're still in the 4th interval (30-39)
        // The pricing block is still from 2 intervals ago: block 19
        assert_eq!(
            post_reorg_ema, fork_prices[19].ema,
            "Post-reorg EMA should be from forked block 19"
        );

        // Test that we can get prices for the next block after reorg
        let new_height = 31; // Next block after the fork tip at 30
        let oracle_price = deterministic_price(new_height);
        let result = ctx.get_prices_for_new_block(new_height, oracle_price).await;

        assert!(result.is_ok());
        let response = result.unwrap();

        // Verify the oracle price is properly bounded
        assert_eq!(
            response.range_adjusted_oracle_price, oracle_price,
            "Oracle price should not need adjustment"
        );

        // Test optimistic cache
        let test_height = new_height;
        let test_oracle_price = deterministic_price(test_height);
        let status = ctx
            .validate_oracle_price(test_height, test_oracle_price)
            .await;
        assert!(status.is_ok());

        // The oracle price validation should pass since it's within range
        assert_eq!(
            status.unwrap(),
            PriceStatus::Valid,
            "Oracle price should be valid"
        );

        // Also verify the optimistic cache can calculate EMA for validation
        let validation_result = ctx
            .validate_ema_price(test_height, prices[0].ema, test_oracle_price)
            .await;
        assert!(validation_result.is_ok(), "EMA validation should not error");
    }
}
