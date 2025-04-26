use std::collections::BTreeMap;

use futures::future::Either;
use irys_primitives::CommitmentType;
use irys_types::{Address, CommitmentTransaction, Config};
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
use std::pin::pin;
use tokio::{
    sync::{mpsc::UnboundedReceiver, oneshot},
    task::JoinHandle,
};
use tracing::debug;

use crate::CommitmentStateReadGuard;
// Messages that the CommitmentCache service supports
#[derive(Debug)]
pub enum CommitmentCacheMessage {
    GetCommitmentStatus {
        commitment_tx: CommitmentTransaction,
        response: oneshot::Sender<CommitmentStatus>,
    },
    AddCommitment {
        commitment_tx: CommitmentTransaction,
        response: oneshot::Sender<CommitmentStatus>,
    },
}

#[derive(Debug, PartialEq)]
pub enum CommitmentStatus {
    Accepted,        // The commitment is valid and was added to the cache
    Invalid(String), // The commitment failed validation with specific reason
    Unknown,         // The commitment is unknown to the cache & has no status
}

#[derive(Default, Debug)]
struct MinerCommitments {
    stake: Option<CommitmentTransaction>,
    pledges: Vec<CommitmentTransaction>,
}

#[derive(Debug)]
pub struct CommitmentCache {
    shutdown: GracefulShutdown,
    msg_rx: UnboundedReceiver<CommitmentCacheMessage>,
    inner: Inner,
}

#[derive(Debug)]
struct Inner {
    cache: BTreeMap<Address, MinerCommitments>,
    commitment_state_guard: CommitmentStateReadGuard,
}

impl CommitmentCache {
    /// Spawn a new CommitmentCache service
    pub fn spawn_service(
        exec: &TaskExecutor,
        rx: UnboundedReceiver<CommitmentCacheMessage>,
        commitment_state_guard: CommitmentStateReadGuard,
        _config: &Config,
    ) -> JoinHandle<()> {
        exec.spawn_critical_with_graceful_shutdown_signal(
            "CommitmentCache Service",
            |shutdown| async move {
                let pending_commitments_cache = Self {
                    shutdown,
                    msg_rx: rx,
                    inner: Inner {
                        cache: BTreeMap::new(),
                        commitment_state_guard,
                    },
                };
                pending_commitments_cache
                    .start()
                    .await
                    .expect("CommitmentCache encountered an irrecoverable error")
            },
        )
    }

    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting CommitmentCache service");

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

        tracing::info!("shutting down CommitmentCache service");
        Ok(())
    }
}

impl Inner {
    #[tracing::instrument(skip_all, err)]
    async fn handle_message(&mut self, msg: CommitmentCacheMessage) -> eyre::Result<()> {
        match msg {
            CommitmentCacheMessage::GetCommitmentStatus {
                commitment_tx,
                response,
            } => {
                self.get_commitment_status(commitment_tx, response);
            }
            CommitmentCacheMessage::AddCommitment {
                commitment_tx,
                response,
            } => {
                self.add_commitment(commitment_tx, response);
            }
        }
        Ok(())
    }

    fn get_commitment_status(
        &self,
        commitment_tx: CommitmentTransaction,
        response: oneshot::Sender<CommitmentStatus>,
    ) {
        debug!("GetCommitmentStatus message received");

        let commitment_type = commitment_tx.commitment_type;
        let txid = commitment_tx.id;
        let signer = &commitment_tx.signer;

        // First handle unsupported commitment types
        if !matches!(
            commitment_type,
            CommitmentType::Stake | CommitmentType::Pledge
        ) {
            debug!(
                "CommitmentStatus is Rejected: unsupported type: {:?}",
                commitment_type
            );
            let _ = response
                .send(CommitmentStatus::Invalid(
                    "Unsupported commitment action".into(),
                ))
                .inspect_err(|_| tracing::warn!("Response send failed, channel dropped"));
            return;
        }

        // Check if we have commitments for this miner address
        let commitments = self.cache.get(signer);

        // Handle by the input values commitment type
        let status = match commitment_type {
            CommitmentType::Stake => {
                if let Some(commitments) = &commitments {
                    // Check for duplicate stake transaction
                    if commitments.stake.as_ref().is_some_and(|s| s.id == txid) {
                        CommitmentStatus::Accepted
                    } else {
                        CommitmentStatus::Unknown
                    }
                } else {
                    // No commitments for this address yet
                    CommitmentStatus::Unknown
                }
            }
            CommitmentType::Pledge => {
                if let Some(commitments) = &commitments {
                    // Check for duplicate pledge transaction
                    if commitments.pledges.iter().any(|p| p.id == txid) {
                        CommitmentStatus::Accepted
                    } else if commitments.stake.is_none() {
                        // Require existing stake for pledges
                        CommitmentStatus::Invalid("pledge address not staked".into())
                    } else {
                        CommitmentStatus::Unknown
                    }
                } else {
                    // No commitments for this address, so no stake exists
                    CommitmentStatus::Invalid("pledge address not staked".into())
                }
            }
            _ => unreachable!(), // We already handled unsupported types
        };

        debug!("CommitmentStatus is {:?}", status);
        let _ = response.send(status).inspect_err(|_| {
            tracing::warn!("CommitmentStatus could not be returned, sender has dropped its half of the channel")
        });
    }

    fn add_commitment(
        &mut self,
        commitment_tx: CommitmentTransaction,
        response: oneshot::Sender<CommitmentStatus>,
    ) {
        debug!("AddCommitment message received");
        let signer = &commitment_tx.signer;
        let tx_type = commitment_tx.commitment_type;
        let send_status = |status| {
            debug!("AddCommitment response: {:?}", status);
            let _ = response.send(status).inspect_err(|_| {
                tracing::warn!("CommitmentStatus could not be returned, sender dropped their half of the channel")
            });
        };

        // Early return for non-supported commitment types
        if !matches!(tx_type, CommitmentType::Stake | CommitmentType::Pledge) {
            return send_status(CommitmentStatus::Invalid(
                "unsupported commitment type".into(),
            ));
        }

        // Get or create miner commitments entry
        let miner_commitments = self
            .cache
            .entry(signer.clone())
            .or_insert_with(MinerCommitments::default);

        // Handle stake commitments
        if matches!(tx_type, CommitmentType::Stake) {
            // Check existing commitments in epoch service
            if self
                .commitment_state_guard
                .read()
                .stake_commitments
                .contains_key(signer)
                || miner_commitments.stake.is_some()
            {
                return send_status(CommitmentStatus::Accepted);
            }

            // Store new stake commitment
            miner_commitments.stake = Some(commitment_tx.clone());
        } else {
            // Add pledge commitment
            miner_commitments.pledges.push(commitment_tx.clone());
        }

        send_status(CommitmentStatus::Accepted);
    }
}
