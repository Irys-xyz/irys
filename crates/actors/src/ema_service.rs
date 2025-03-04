use actix::Addr;
use futures_concurrency::prelude::*;
use irys_types::{Config, IrysBlockHeader, IrysTokenPrice, H256};
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
    GetEmaForNextEpoch {
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
        let mut context =
            PriceCacheContext::from_canonical_chain(self.block_tree_read_guard.clone()).await;

        let msg_processor = async {
            while let Some(msg) = self.msg_rx.recv().await {
                match msg {
                    EmaServiceMessage::GetCurrentEma { response } => {
                        let _ = response.send(context.two_epochs_ago_block.irys_price);
                    }
                    EmaServiceMessage::GetEmaForNextEpoch { response } => {
                        // example EMA calculation at Epoch29:
                        // I need to take the registered Irys price in block 18 (non epoch block) and the stored EMA in E19 (epoch block).
                        // Using these values I must compute EMA for E29. In this case the n (number of block prices) would be 10 (E29.height - E19.height).
                        // This is the price that will be used in the interval 39->49, which will be reported to other systems querying for EMA prices.

                        // the amount of blocks in between the 2 epoch values
                        let blocks_in_between = self.blocks_in_epoch;

                        // calculate the new EMA
                        let new_ema = context
                            .previous_epoch_block_predecessor
                            .irys_price
                            .calculate_ema(
                                blocks_in_between,
                                context.previous_epoch_block.irys_price.to_ema(),
                            )
                            .unwrap();

                        let _ = response.send(new_ema.to_irys_price());
                    }
                    EmaServiceMessage::NewConfirmedBlock => {
                        // Rebuild the entire data cache just like we do at startup.
                        context = PriceCacheContext::from_canonical_chain(
                            self.block_tree_read_guard.clone(),
                        )
                        .await;
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
    previous_epoch_block: IrysBlockHeader,
    previous_epoch_block_predecessor: IrysBlockHeader,
    two_epochs_ago_block: IrysBlockHeader,
}

impl PriceCacheContext {
    /// Builds the entire context from scratch by scanning the canonical chain.
    async fn from_canonical_chain(block_tree_read_guard: BlockTreeReadGuard) -> Self {
        // Rebuild the entire data cache just like we do at startup.
        let canonical_chain = get_canonical_chain(block_tree_read_guard.clone()).await.0;
        let (latest_block_hash, ..) = canonical_chain.last().unwrap();

        // Fetch the latest block
        let latest_block = get_block(block_tree_read_guard.clone(), &latest_block_hash).await;

        // Get data for the previous epoch
        let previous_epoch_block =
            get_block(block_tree_read_guard.clone(), &latest_block.block_hash).await;
        let previous_epoch_block_predecessor = get_block(
            block_tree_read_guard.clone(),
            &previous_epoch_block.previous_block_hash,
        )
        .await;

        // Get data for the epoch before that (two epochs ago)
        let two_epochs_ago_block = get_block(
            block_tree_read_guard.clone(),
            &previous_epoch_block.last_epoch_hash,
        )
        .await;

        Self {
            previous_epoch_block,
            previous_epoch_block_predecessor,
            two_epochs_ago_block,
        }
    }
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
