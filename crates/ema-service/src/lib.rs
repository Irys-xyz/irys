use std::{future::Future, task::Poll, time::Duration};

use futures::{FutureExt, StreamExt as _};
use futures_concurrency::prelude::*;
use irys_types::storage_pricing::{
    phantoms::{Ema, Usd},
    Amount,
};
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
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
    pub msg_rx: UnboundedReceiver<EmaServiceMessage>,
}

impl EmaService {
    async fn start(mut self) {
        let msg_processor = async {
            while let Some(msg) = self.msg_rx.recv().await {
                match msg {
                    EmaServiceMessage::GetCurrentEma { response } => {
                        let _ = response.send(Amount::token(dec!(1.0)).unwrap());
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
