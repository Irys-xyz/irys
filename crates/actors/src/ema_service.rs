use std::{future::Future, task::Poll, time::Duration};

use actix::Addr;
use futures::{FutureExt, StreamExt as _};
use futures_concurrency::prelude::*;
use irys_types::{
    storage_pricing::{
        phantoms::{Ema, IrysPrice, Usd},
        Amount,
    },
    Config, IrysBlockHeader, IrysTokenPrice, H256,
};
use reth::{
    revm::interpreter::SelfDestructResult,
    tasks::{shutdown::GracefulShutdown, TaskExecutor},
};
use rust_decimal_macros::dec;
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
    GetEmaForNewEpoch {
        prev_epoch_hash: H256,
        new_block_height: u64,
        response: oneshot::Sender<IrysTokenPrice>,
    },
    NewConfirmedBlock,
}

#[derive(Debug, Clone)]
pub struct EmaServiceHandle {
    pub sender: UnboundedSender<EmaServiceMessage>,
}

impl EmaServiceHandle {
    pub fn spawn_service(
        exec: TaskExecutor,
        block_tree: Addr<BlockTreeService>,
        config: &Config,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let genesis_token_price = config.genesis_token_price;
        let blocks_in_epoch = config.num_blocks_in_epoch;
        exec.spawn_critical_with_graceful_shutdown_signal("EMA Service", |shutdown| async move {
            let block_tree_read_guard = block_tree
                .send(GetBlockTreeGuardMessage)
                .await
                .expect("could not retrieve block tree read guard");
            let cache_service = EmaService {
                shutdown,
                blocks_in_epoch,
                msg_rx: rx,
                block_tree_read_guard,
                genesis_token_price,
            };
            cache_service.start().await
        });
        EmaServiceHandle { sender: tx }
    }
}

#[derive(Debug)]
pub struct EmaService {
    pub shutdown: GracefulShutdown,
    pub blocks_in_epoch: u64,
    pub msg_rx: UnboundedReceiver<EmaServiceMessage>,
    pub block_tree_read_guard: BlockTreeReadGuard,
    pub genesis_token_price: IrysTokenPrice,
}

impl EmaService {
    async fn start(mut self) {
        let mut context = {
            PriceCacheContext::from_canonical_chain(
                self.block_tree_read_guard.clone(),
                self.blocks_in_epoch,
                self.genesis_token_price,
            )
            .await
        };

        let msg_processor = async {
            while let Some(msg) = self.msg_rx.recv().await {
                match msg {
                    EmaServiceMessage::GetCurrentEma { response } => {
                        // store values for:
                        //  - current epoch EMA
                        //  - current epoch EMA
                        let _ = response.send(context.ema_price_to_use.0);
                    }
                    EmaServiceMessage::GetEmaForNewEpoch {
                        response,
                        prev_epoch_hash,
                        new_block_height,
                    } => {
                        let new_ema_price = calculate_new_ema(
                            self.block_tree_read_guard.clone(),
                            prev_epoch_hash,
                            new_block_height,
                        )
                        .await;

                        let _ = response.send(new_ema_price.to_irys_price());
                    }
                    EmaServiceMessage::NewConfirmedBlock => {
                        // Rebuild the entire data cache just like we do at startup.
                        let canonical_chain =
                            get_canonical_chain(self.block_tree_read_guard.clone())
                                .await
                                .0;
                        let (latest_block_hash, ..) = canonical_chain.last().unwrap();

                        // Fetch the latest block
                        let latest_block =
                            get_block(self.block_tree_read_guard.clone(), &latest_block_hash).await;

                        // Get data for the previous epoch
                        let previous_epoch_block =
                            get_block(self.block_tree_read_guard.clone(), &latest_block.block_hash)
                                .await;
                        let previous_epoch_block_predecessor = get_block(
                            self.block_tree_read_guard.clone(),
                            &previous_epoch_block.previous_block_hash,
                        )
                        .await;

                        // Get data for the epoch before that (two epochs ago)
                        let two_epochs_ago_block = get_block(
                            self.block_tree_read_guard.clone(),
                            &previous_epoch_block.last_epoch_hash,
                        )
                        .await;
                        let two_epochs_ago_block_predecessor = get_block(
                            self.block_tree_read_guard.clone(),
                            &two_epochs_ago_block.previous_block_hash,
                        )
                        .await;

                        // todo store the variables to be used in the `GetCurrentEMA` call
                    }
                }
            }
        };

        let shutdown_handler = async {
            let _ = self.shutdown.await;
        };
        msg_processor.race(shutdown_handler).await;
    }
}

struct PriceCacheContext {
    /// this is the data price we send away upon requests
    ema_price_to_use: (IrysTokenPrice, u64),
    /// This is the the preceding adjustment interval
    adjustment_interval_preceding: Option<AdjustmentInterval>,
    /// This is the currently on-going adjustment interval (it will not have `price_epoch_block` value set)
    adjustment_interval_current: Option<AdjustmentInterval>,
}
impl PriceCacheContext {
    /// Builds the entire context from scratch by scanning the canonical chain.
    pub async fn from_canonical_chain(
        block_tree_read_guard: BlockTreeReadGuard,
        blocks_in_epoch: u64,
        genesis_token_price: IrysTokenPrice,
    ) -> Self {
        let chain_canonical = get_canonical_chain(block_tree_read_guard.clone()).await.0;
        let tip_hash = match chain_canonical.last() {
            Some((hash, _height, ..)) => *hash,
            None => {
                // No chain. Return a trivial context with genesis as default.
                return PriceCacheContext {
                    ema_price_to_use: (genesis_token_price, 0),
                    adjustment_interval_preceding: None,
                    adjustment_interval_current: None,
                };
            }
        };

        let latest_block = get_block(block_tree_read_guard.clone(), &tip_hash).await;
        if let Some(value) =
            calculate_new_ema(block_tree_read_guard, blocks_in_epoch, latest_block).await
        {
            return value;
        }

        todo!()
        // // this is the previous epoch
        // let epoch = get_block(block_tree_read_guard.clone(), &latest_block.last_epoch_hash).await;
        // let block_before_epoch =
        //     get_block(block_tree_read_guard.clone(), &epoch.previous_block_hash).await;

        // context
    }
}

// example EMA calculation at Epoch29:
// I need to take the registered Irys price in block 18 (non epoch block) and the stored EMA in E19 (epoch block).
// Using these values I must compute EMA for E29. In this case the n (number of block prices) would be 10 (E29.height - E19.height).
// This is the price that will be used in the interval 39->49, which will be reported to other systems querying for EMA prices.
async fn calculate_new_ema(
    block_tree_read_guard: BlockTreeReadGuard,
    last_epoch_hash: H256,
    new_block_height: u64,
) -> Amount<(Ema, Usd)> {
    // retrieve the pervious epoch & the block before the epoch
    let prev_epoch = get_block(block_tree_read_guard.clone(), &last_epoch_hash).await;
    let block_before_prev_epoch = get_block(
        block_tree_read_guard.clone(),
        &prev_epoch.previous_block_hash,
    )
    .await;

    // the amount of blocks in between the 2 epoch values
    let blocks_in_between = new_block_height - prev_epoch.height;

    // calculate the new EMA
    let new_ema = block_before_prev_epoch
        .irys_price
        .calculate_ema(blocks_in_between, prev_epoch.irys_price.to_ema())
        .unwrap();
    return new_ema;
}

/// adjustment interval is the sequence of blocks that lie in-between 2 epoch blocks.
/// The epoch block is also considered to be a part of the adjustment interval.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AdjustmentInterval {
    /// Height of the adjustment interval
    height: u64,
    /// the irys token price in the preceding block of the epoch
    price_epoch_block_minus_one: IrysTokenPrice,
    /// Price derived in the pecoh block.
    /// this is derived from the preceding adjustment intervals `price_epoch_block_minus_one`
    price_epoch_block: Option<IrysTokenPrice>,
}

/// rerturns the canonical chain where the leftmost item in the Vec is the latest block
async fn get_canonical_chain(
    tree: BlockTreeReadGuard,
) -> (Vec<(H256, u64, Vec<H256>, Vec<H256>)>, usize) {
    let canonical_chain = tokio::task::spawn_blocking(move || tree.read().get_canonical_chain())
        .await
        .unwrap();
    canonical_chain
}

async fn get_block(
    block_tree_read_guard: BlockTreeReadGuard,
    block_hash: &H256,
) -> IrysBlockHeader {
    let block_hash = block_hash.clone();
    let block = tokio::task::spawn_blocking(move || {
        block_tree_read_guard.read().get_block(&block_hash).cloned()
    })
    .await
    .unwrap()
    .unwrap();
    block
}
