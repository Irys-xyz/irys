use std::{future::Future, task::Poll, time::Duration};

use futures::{FutureExt, StreamExt as _};
use futures_concurrency::prelude::*;
use irys_types::{
    storage_pricing::{
        phantoms::{Ema, IrysPrice, Usd},
        Amount,
    },
    IrysBlockHeader,
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

#[derive(Debug)]
pub enum EmaServiceMessage {
    GetCurrentEma {
        response: oneshot::Sender<Amount<(Ema, Usd)>>,
    },
    NewConfirmedBlock(IrysBlockHeader),
}

#[derive(Debug, Clone)]
pub struct EmaServiceHandle {
    pub sender: UnboundedSender<EmaServiceMessage>,
}

impl EmaServiceHandle {
    pub fn spawn_service(exec: TaskExecutor) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        exec.spawn_critical_with_graceful_shutdown_signal("EMA Service", |shutdown| async move {
            let cache_service = EmaService {
                shutdown,
                blocks_in_epoch: 8,
                msg_rx: rx,
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
}

impl EmaService {
    async fn start(mut self) {
        const MAX_BLOCK_PRICES: usize = 16;
        assert!(
            self.blocks_in_epoch <= (MAX_BLOCK_PRICES as u64),
            "blocks in epoch exceed the max allowed"
        );
        let mut current_ema = Amount::token(dec!(1.0)).unwrap();
        let mut current_height = 0;
        let mut block_prices_in_epoch =
            heapless::Vec::<(u64, Amount<(IrysPrice, Usd)>), MAX_BLOCK_PRICES>::new();

        let msg_processor = async {
            while let Some(msg) = self.msg_rx.recv().await {
                match msg {
                    EmaServiceMessage::GetCurrentEma { response } => {
                        let _ = response.send(current_ema);
                    }
                    EmaServiceMessage::NewConfirmedBlock(irys_block_header) => {
                        if irys_block_header.is_epoch(self.blocks_in_epoch) {
                            // after we reach an epoch block, we calculate EMA for the last n blocks.
                            let diff = current_height - irys_block_header.height;
                            let new_ema = irys_block_header
                                .irys_price
                                .calculate_ema(diff, current_ema)
                                .unwrap();

                            // update the latest price info
                            current_ema = new_ema;
                            current_height = irys_block_header.height;
                            block_prices_in_epoch.clear();
                        } else {
                            // we only record the price until we reach a new epoch block
                            let idx = irys_block_header
                                .height
                                .checked_rem(self.blocks_in_epoch)
                                .unwrap();
                            block_prices_in_epoch[idx as usize] =
                                (irys_block_header.height, irys_block_header.irys_price);
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
