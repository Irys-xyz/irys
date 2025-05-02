use std::collections::{BTreeMap, HashSet};

use futures::future::Either;
use irys_primitives::CommitmentType;
use irys_types::{Address, CommitmentTransaction, Config, H256List, H256};
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
    RollbackCommitments {
        commitment_txs: H256List,
        response: oneshot::Sender<eyre::Result<()>>,
    },
}

#[derive(Debug, PartialEq)]
pub enum CommitmentStatus {
    Accepted,    // The commitment is valid and was added to the cache
    Unknown,     // The commitment is unknown to the cache & has no status
    Unsupported, // The commitment is an unsupported type (pledge/unpledge)
    Unstaked,    // The pledge commitment doesn't have a corresponding stake
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
#[derive(Default, Debug)]
struct MinerCommitments {
    stake: Option<CommitmentTransaction>,
    pledges: Vec<CommitmentTransaction>,
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

        tracing::debug!(amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");
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
    /// Dispatches received messages to appropriate handler methods
    ///
    /// Acts as the main entry point for the CommitmentCache service, routing
    /// incoming messages to their respective handler functions:
    ///
    /// # Arguments
    /// * `msg` - The commitment cache message to process
    ///
    /// # Returns
    /// * `eyre::Result<()>` - Success or failure of message handling
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
            CommitmentCacheMessage::RollbackCommitments {
                commitment_txs,
                response,
            } => {
                self.rollback_commitments(&commitment_txs, response);
            }
        }
        Ok(())
    }

    /// Checks and returns the status of a commitment transaction
    ///
    /// Evaluates the provided commitment transaction to determine its status in the system:
    /// - For stake commitments: Determines if it's already accepted or unknown
    /// - For pledge commitments: Verifies if a matching pledge exists, or if the address
    ///   lacks a required stake commitment
    /// - For unsupported types: Returns an unsupported status
    ///
    /// The function does not modify any state but checks against the current cache
    /// to determine appropriate status.
    ///
    /// # Arguments
    /// * `commitment_tx` - The commitment transaction to evaluate
    /// * `response` - Channel for sending the determined status to caller
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
                .send(CommitmentStatus::Unsupported)
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
                        CommitmentStatus::Unstaked
                    } else {
                        CommitmentStatus::Unknown
                    }
                } else {
                    // No commitments for this address, so no stake exists
                    CommitmentStatus::Unstaked
                }
            }
            _ => unreachable!(), // We already handled unsupported types
        };

        debug!("CommitmentStatus is {:?}", status);
        let _ = response.send(status).inspect_err(|_| {
            tracing::warn!("CommitmentStatus could not be returned, sender has dropped its half of the channel")
        });
    }

    /// Adds a new commitment transaction to the cache and validates its acceptance
    ///
    /// Processes the commitment transaction based on its type:
    /// - For stake commitments: Verifies the address isn't already staked in either the
    ///   current epoch or pending cache
    /// - For pledge commitments: Only accepts if the address has an existing stake commitment
    ///   either in the current epoch or pending cache
    ///
    /// # Arguments
    /// * `commitment_tx` - The commitment transaction to add to the cache
    /// * `response` - Channel for sending the resulting commitment status to caller
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

        // Early return for unsupported commitment types
        if !matches!(tx_type, CommitmentType::Stake | CommitmentType::Pledge) {
            return send_status(CommitmentStatus::Unsupported);
        }

        // Check if address is already staked in current epoch
        let is_staked_in_epoch = self
            .commitment_state_guard
            .read()
            .stake_commitments
            .contains_key(signer);

        // Handle stake commitments
        if matches!(tx_type, CommitmentType::Stake) {
            // Check existing commitments in epoch service
            if is_staked_in_epoch {
                // Already staked in current epoch, no need to add again
                return send_status(CommitmentStatus::Accepted);
            }

            // Get or create miner commitments entry
            let miner_commitments = self
                .cache
                .entry(signer.clone())
                .or_insert_with(MinerCommitments::default);

            // Check if already has pending stake
            if miner_commitments.stake.is_some() {
                return send_status(CommitmentStatus::Accepted);
            }

            // Store new stake commitment
            miner_commitments.stake = Some(commitment_tx.clone());
            send_status(CommitmentStatus::Accepted);
        } else {
            // Handle pledge commitments - only accept if address has a stake

            // First check if staked in current epoch
            if is_staked_in_epoch {
                // Address is staked in current epoch, add pledge
                let miner_commitments = self
                    .cache
                    .entry(signer.clone())
                    .or_insert_with(MinerCommitments::default);

                miner_commitments.pledges.push(commitment_tx.clone());
                return send_status(CommitmentStatus::Accepted);
            }

            // Next check if there's a pending stake in the local cache
            if let Some(miner_commitments) = self.cache.get_mut(signer) {
                if miner_commitments.stake.is_some() {
                    // Has pending stake, can add pledge
                    miner_commitments.pledges.push(commitment_tx.clone());
                    return send_status(CommitmentStatus::Accepted);
                }
            }

            // No stake found, reject pledge
            return send_status(CommitmentStatus::Unstaked);
        }
    }

    /// Removes commitment transactions with specified IDs from the cache
    ///
    /// Traverses the cache and removes any commitments (both stakes and pledges) whose
    /// transaction IDs match those in the provided list. Cleans up empty entries and
    /// sends a completion status through the provided response channel.
    ///
    /// # Arguments
    /// * `commitment_txs` - List of transaction IDs to be removed
    /// * `response` - Channel for sending operation result to caller
    fn rollback_commitments(
        &mut self,
        commitment_txs: &H256List,
        response: oneshot::Sender<eyre::Result<()>>,
    ) {
        let send_status = |status| {
            debug!("RollbackCommitments response: {:?}", status);
            let _ = response.send(status).inspect_err(|_| {
                tracing::warn!("RollbackCommitments response could not be returned, sender dropped their half of the channel")
            });
        };

        // Create a HashSet for faster lookups
        let ids_set: HashSet<&H256> = commitment_txs.iter().collect();

        // Store addresses that need cleaning up
        let mut addresses_to_check = Vec::new();

        // First pass: collect all addresses (to avoid borrow issues)
        for address in self.cache.keys() {
            addresses_to_check.push(*address);
        }

        // Second pass: update each address's commitments
        for address in addresses_to_check {
            if let Some(commitments) = self.cache.get_mut(&address) {
                // Check stake transaction
                if let Some(stake) = &commitments.stake {
                    if ids_set.contains(&stake.id) {
                        commitments.stake = None;
                    }
                }

                // Filter pledges to remove matching IDs
                commitments.pledges.retain(|tx| !ids_set.contains(&tx.id));

                // If both stake and pledges are empty, remove the entry completely
                if commitments.stake.is_none() && commitments.pledges.is_empty() {
                    self.cache.remove(&address);
                }
            }
        }

        send_status(Ok(()));
    }
}
