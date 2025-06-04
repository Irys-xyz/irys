use std::collections::{BTreeMap, HashSet};

use futures::future::Either;
use irys_primitives::CommitmentType;
use irys_types::{Address, CommitmentTransaction, H256List, H256};
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
        response: oneshot::Sender<CommitmentCacheStatus>,
    },
    AddCommitment {
        commitment_tx: CommitmentTransaction,
        response: oneshot::Sender<CommitmentCacheStatus>,
    },
    RollbackCommitments {
        commitment_txs: H256List,
        response: oneshot::Sender<eyre::Result<()>>,
    },
    GetEpochCommitments {
        response: oneshot::Sender<Vec<CommitmentTransaction>>,
    },
    ClearCache {
        response: oneshot::Sender<eyre::Result<()>>,
    },
}

#[derive(Debug, PartialEq)]
pub enum CommitmentCacheStatus {
    Accepted,    // The commitment is valid and was added to the cache
    Unknown,     // The commitment is unknown to the cache & has no status
    Unsupported, // The commitment is an unsupported type (pledge/unpledge)
    Unstaked,    // The pledge commitment doesn't have a corresponding stake
}

#[derive(Debug)]
pub struct CommitmentCache {
    shutdown: GracefulShutdown,
    msg_rx: UnboundedReceiver<CommitmentCacheMessage>,
    inner: CommitmentCacheInner,
}

#[derive(Debug)]
pub struct CommitmentCacheInner {
    cache: BTreeMap<Address, MinerCommitments>,
    commitment_state_guard: CommitmentStateReadGuard,
}

#[derive(Default, Debug)]
struct MinerCommitments {
    stake: Option<CommitmentTransaction>,
    pledges: Vec<CommitmentTransaction>,
}

impl CommitmentCacheInner {
    /// Create a new CommitmentCacheInner instance
    pub fn new(commitment_state_guard: CommitmentStateReadGuard) -> Self {
        Self {
            cache: BTreeMap::new(),
            commitment_state_guard,
        }
    }

    /// Checks and returns the status of a commitment transaction
    pub fn get_commitment_status(
        &self,
        commitment_tx: CommitmentTransaction,
    ) -> CommitmentCacheStatus {
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
            return CommitmentCacheStatus::Unsupported;
        }

        // Check if we have commitments for this miner address
        let commitments = self.cache.get(signer);

        // Handle by the input values commitment type
        let status = match commitment_type {
            CommitmentType::Stake => {
                if let Some(commitments) = &commitments {
                    // Check for duplicate stake transaction
                    if commitments.stake.as_ref().is_some_and(|s| s.id == txid) {
                        CommitmentCacheStatus::Accepted
                    } else {
                        CommitmentCacheStatus::Unknown
                    }
                } else {
                    // No commitments for this address yet
                    CommitmentCacheStatus::Unknown
                }
            }
            CommitmentType::Pledge => {
                if let Some(commitments) = &commitments {
                    // Check for duplicate pledge transaction
                    if commitments.pledges.iter().any(|p| p.id == txid) {
                        CommitmentCacheStatus::Accepted
                    } else if commitments.stake.is_none() {
                        // Require existing stake for pledges
                        CommitmentCacheStatus::Unstaked
                    } else {
                        CommitmentCacheStatus::Unknown
                    }
                } else {
                    // No commitments for this address, so no stake exists
                    CommitmentCacheStatus::Unstaked
                }
            }
            _ => unreachable!(), // We already handled unsupported types
        };

        debug!("CommitmentStatus is {:?}", status);
        status
    }

    /// Adds a new commitment transaction to the cache and validates its acceptance
    pub fn add_commitment(
        &mut self,
        commitment_tx: CommitmentTransaction,
    ) -> CommitmentCacheStatus {
        debug!("AddCommitment message received");
        let signer = &commitment_tx.signer;
        let tx_type = commitment_tx.commitment_type;

        // Early return for unsupported commitment types
        if !matches!(tx_type, CommitmentType::Stake | CommitmentType::Pledge) {
            return CommitmentCacheStatus::Unsupported;
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
                return CommitmentCacheStatus::Accepted;
            }

            // Get or create miner commitments entry
            let miner_commitments = self
                .cache
                .entry(*signer)
                .or_default();

            // Check if already has pending stake
            if miner_commitments.stake.is_some() {
                return CommitmentCacheStatus::Accepted;
            }

            // Store new stake commitment
            miner_commitments.stake = Some(commitment_tx.clone());
            CommitmentCacheStatus::Accepted
        } else {
            // Handle pledge commitments - only accept if address has a stake

            // First check if staked in current epoch
            if is_staked_in_epoch {
                // Address is staked in current epoch, add pledge
                let miner_commitments = self
                    .cache
                    .entry(*signer)
                    .or_default();

                miner_commitments.pledges.push(commitment_tx.clone());
                return CommitmentCacheStatus::Accepted;
            }

            // Next check if there's a pending stake in the local cache
            if let Some(miner_commitments) = self.cache.get_mut(signer) {
                if miner_commitments.stake.is_some() {
                    // Has pending stake, can add pledge
                    miner_commitments.pledges.push(commitment_tx.clone());
                    return CommitmentCacheStatus::Accepted;
                }
            }

            // No stake found, reject pledge
            CommitmentCacheStatus::Unstaked
        }
    }

    /// Removes commitment transactions with specified IDs from the cache
    pub fn rollback_commitments(&mut self, commitment_txs: &H256List) -> eyre::Result<()> {
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

        Ok(())
    }

    /// Collects all commitment transactions from the cache for epoch processing
    pub fn get_epoch_commitments(&self) -> Vec<CommitmentTransaction> {
        let mut commitment_tx: Vec<CommitmentTransaction> = Vec::new();

        // First collect all stake transactions in address order
        // BTreeMap is already ordered by keys (addresses)
        for miner_commitments in self.cache.values() {
            if let Some(stake) = &miner_commitments.stake {
                commitment_tx.push(stake.clone());
            }
        }

        // Then collect all pledge transactions in address order
        for miner_commitments in self.cache.values() {
            // Add all pledges for this miner
            for pledge in &miner_commitments.pledges {
                commitment_tx.push(pledge.clone());
            }
        }

        commitment_tx
    }

    pub fn clear_cache(&mut self) -> eyre::Result<()> {
        // Clear out all the existing cached commitments as they have
        // been applied to the CommitmentState in the epoch service
        self.cache.clear();
        Ok(())
    }

    /// Dispatches received messages to appropriate handler methods and sends responses
    #[tracing::instrument(skip_all, err)]
    async fn handle_message(&mut self, msg: CommitmentCacheMessage) -> eyre::Result<()> {
        match msg {
            CommitmentCacheMessage::GetCommitmentStatus {
                commitment_tx,
                response,
            } => {
                let status = self.get_commitment_status(commitment_tx);
                let _ = response.send(status).inspect_err(|_| {
                    tracing::warn!("GetCommitmentStatus response could not be returned, sender dropped their half of the channel")
                });
            }
            CommitmentCacheMessage::AddCommitment {
                commitment_tx,
                response,
            } => {
                let status = self.add_commitment(commitment_tx);
                let _ = response.send(status).inspect_err(|_| {
                    tracing::warn!("AddCommitment response could not be returned, sender dropped their half of the channel")
                });
            }
            CommitmentCacheMessage::RollbackCommitments {
                commitment_txs,
                response,
            } => {
                let result = self.rollback_commitments(&commitment_txs);
                let _ = response.send(result).inspect_err(|_| {
                    tracing::warn!("RollbackCommitments response could not be returned, sender dropped their half of the channel")
                });
            }
            CommitmentCacheMessage::GetEpochCommitments { response } => {
                let commitments = self.get_epoch_commitments();
                let _ = response.send(commitments).inspect_err(|_| {
                    tracing::warn!("GetEpochCommitments response could not be delivered, sender dropped their half of the channel")
                });
            }
            CommitmentCacheMessage::ClearCache { response } => {
                let result = self.clear_cache();
                let _ = response.send(result).inspect_err(|_| {
                    tracing::warn!("StartNewEpoch response could not be returned, sender dropped their half of the channel")
                });
            }
        }
        Ok(())
    }
}

impl CommitmentCache {
    /// Spawn a new CommitmentCache service
    pub fn spawn_service(
        exec: &TaskExecutor,
        rx: UnboundedReceiver<CommitmentCacheMessage>,
        commitment_state_guard: CommitmentStateReadGuard,
    ) -> JoinHandle<()> {
        exec.spawn_critical_with_graceful_shutdown_signal(
            "CommitmentCache Service",
            |shutdown| async move {
                let pending_commitments_cache = Self {
                    shutdown,
                    msg_rx: rx,
                    inner: CommitmentCacheInner::new(commitment_state_guard),
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
