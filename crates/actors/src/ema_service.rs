use std::{future::Future, task::Poll, time::Duration};

use actix::Addr;
use futures::{FutureExt, StreamExt as _};
use futures_concurrency::prelude::*;
use irys_types::{
    storage_pricing::{
        phantoms::{Ema, IrysPrice, Usd},
        Amount,
    },
    IrysBlockHeader, H256,
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
        /// return: (irys price, (latest_block_height, latest_block_hash))
        response: oneshot::Sender<(Amount<(Ema, Usd)>, (u64, H256))>,
    },
    NewConfirmedBlock,
}

#[derive(Debug, Clone)]
pub struct EmaServiceHandle {
    pub sender: UnboundedSender<EmaServiceMessage>,
}

impl EmaServiceHandle {
    pub fn spawn_service(exec: TaskExecutor, block_tree: Addr<BlockTreeService>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        exec.spawn_critical_with_graceful_shutdown_signal("EMA Service", |shutdown| async move {
            let block_tree_read_guard = block_tree
                .send(GetBlockTreeGuardMessage)
                .await
                .expect("could not retrieve block tree read guard");
            let cache_service = EmaService {
                shutdown,
                blocks_in_epoch: 8,
                msg_rx: rx,
                block_tree_read_guard,
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
}

impl EmaService {
    async fn start(mut self) {
        const MAX_BLOCK_PRICES: usize = 16;
        assert!(
            self.blocks_in_epoch <= (MAX_BLOCK_PRICES as u64),
            "blocks in epoch exceed the max allowed"
        );

        // todo get the canonical chain and re-build the prices
        let mut current_ema = Amount::token(dec!(1.0)).unwrap();

        let msg_processor = async {
            while let Some(msg) = self.msg_rx.recv().await {
                match msg {
                    EmaServiceMessage::GetCurrentEma { response } => {
                        let _ = response.send((current_ema, (current_height, current_hash)));
                    }
                    EmaServiceMessage::NewConfirmedBlock => {
                        // clear the old cache, we will re-build a new one by fetching the canonical chain
                        block_prices_in_epoch.clear();

                        let canonical_chain =
                            get_canonical_chain(self.block_tree_read_guard.clone()).await;

                        // get the latest block in the canonical chain
                        let latest_block = {
                            let latest_enstry = canonical_chain.0.first().unwrap();
                            get_block(self.block_tree_read_guard.clone(), &latest_enstry.0).await
                        };

                        if latest_block.is_epoch(self.blocks_in_epoch) {
                        } else {
                            // build the cache until we hit the latest epoch block
                            for (block_hash, block_height, ..) in canonical_chain.0.iter() {
                                if block_height % self.blocks_in_epoch == 0 {
                                    // we hit an epoch block, stop processing
                                    break;
                                }

                                // store the price cache value
                                let block =
                                    get_block(self.block_tree_read_guard.clone(), block_hash).await;
                                block_prices_in_epoch
                                    .push((*block_height, block.irys_price))
                                    .unwrap();
                            }
                        }

                        // process the new block
                        if latest_block.is_epoch(self.blocks_in_epoch) {
                            // after we reach an epoch block, we calculate EMA for the last n blocks.
                            let diff = current_height - latest_block.height;
                            let new_ema = latest_block
                                .irys_price
                                .calculate_ema(diff, current_ema)
                                .unwrap();

                            // update the latest price info
                            current_ema = new_ema;
                            current_height = latest_block.height;
                            current_hash = latest_block.block_hash;

                            // clear the price history
                            block_prices_in_epoch.clear();
                        } else {
                            // we only record the price until we reach a new epoch block
                            let idx = latest_block
                                .height
                                .checked_rem(self.blocks_in_epoch)
                                .unwrap();
                            block_prices_in_epoch[idx as usize] =
                                (latest_block.height, latest_block.irys_price);
                        }
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
