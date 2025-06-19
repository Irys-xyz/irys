use irys_primitives::CommitmentType;
use irys_types::{Address, CommitmentTransaction, H256List, H256};
use std::collections::{BTreeMap, HashSet};
use tracing::debug;

#[derive(Debug, PartialEq)]
pub enum CommitmentSnapshotStatus {
    Accepted,    // The commitment is valid and was added to the snapshot
    Unknown,     // The commitment has no status in the snapshot
    Unsupported, // The commitment is an unsupported type (unstake/unpledge)
    Unstaked,    // The pledge commitment doesn't have a corresponding stake
}

#[derive(Debug, Default, Clone)]
pub struct CommitmentSnapshot {
    commitments: BTreeMap<Address, MinerCommitments>,
}

#[derive(Default, Debug, Clone)]
struct MinerCommitments {
    stake: Option<CommitmentTransaction>,
    pledges: Vec<CommitmentTransaction>,
}

impl CommitmentSnapshot {
    pub fn new_from_commitments(commitment_txs: Option<Vec<CommitmentTransaction>>) -> Self {
        let mut snapshot = Self::default();

        if let Some(commitment_txs) = commitment_txs {
            for commitment_tx in commitment_txs {
                let _status = snapshot.add_commitment(&commitment_tx, false);
            }
        }

        snapshot
    }

    /// Checks and returns the status of a commitment transaction
    pub fn get_commitment_status(
        &self,
        commitment_tx: &CommitmentTransaction,
    ) -> CommitmentSnapshotStatus {
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
            return CommitmentSnapshotStatus::Unsupported;
        }

        // Check if we have commitments for this miner address
        let commitments = self.commitments.get(signer);

        // Handle by the input values commitment type
        let status = match commitment_type {
            CommitmentType::Stake => {
                if let Some(commitments) = &commitments {
                    // Check for duplicate stake transaction
                    if commitments.stake.as_ref().is_some_and(|s| s.id == txid) {
                        CommitmentSnapshotStatus::Accepted
                    } else {
                        CommitmentSnapshotStatus::Unknown
                    }
                } else {
                    // No commitments for this address yet
                    CommitmentSnapshotStatus::Unknown
                }
            }
            CommitmentType::Pledge => {
                if let Some(commitments) = &commitments {
                    // Check for duplicate pledge transaction
                    if commitments.pledges.iter().any(|p| p.id == txid) {
                        CommitmentSnapshotStatus::Accepted
                    } else if commitments.stake.is_none() {
                        // Require existing stake for pledges
                        CommitmentSnapshotStatus::Unstaked
                    } else {
                        CommitmentSnapshotStatus::Unknown
                    }
                } else {
                    // No commitments for this address, so no stake exists
                    CommitmentSnapshotStatus::Unstaked
                }
            }
            _ => unreachable!(), // We already handled unsupported types
        };

        debug!("CommitmentStatus is {:?}", status);
        status
    }

    /// Adds a new commitment transaction to the snapshot and validates its acceptance
    pub fn add_commitment(
        &mut self,
        commitment_tx: &CommitmentTransaction,
        is_staked_in_current_epoch: bool,
    ) -> CommitmentSnapshotStatus {
        debug!("add_commitment() called for {}", commitment_tx.id);
        let signer = &commitment_tx.signer;
        let tx_type = commitment_tx.commitment_type;

        // Early return for unsupported commitment types
        if !matches!(tx_type, CommitmentType::Stake | CommitmentType::Pledge) {
            return CommitmentSnapshotStatus::Unsupported;
        }

        // Handle stake commitments
        if matches!(tx_type, CommitmentType::Stake) {
            // Check existing commitments in epoch service
            if is_staked_in_current_epoch {
                // Already staked in current epoch, no need to add again
                return CommitmentSnapshotStatus::Accepted;
            }

            // Get or create miner commitments entry
            let miner_commitments = self.commitments.entry(*signer).or_default();

            // Check if already has pending stake
            if miner_commitments.stake.is_some() {
                return CommitmentSnapshotStatus::Accepted;
            }

            // Store new stake commitment
            miner_commitments.stake = Some(commitment_tx.clone());
            CommitmentSnapshotStatus::Accepted
        } else {
            // Handle pledge commitments - only accept if address has a stake

            // First check if staked in current epoch
            if is_staked_in_current_epoch {
                // Address is staked in current epoch, add pledge
                let miner_commitments = self.commitments.entry(*signer).or_default();

                let existing = miner_commitments
                    .pledges
                    .iter()
                    .find(|t| t.id == commitment_tx.id);

                if let Some(existing) = existing {
                    debug!("DUPLICATING PLEDGE: {}", existing.id)
                }

                miner_commitments.pledges.push(commitment_tx.clone());
                return CommitmentSnapshotStatus::Accepted;
            }

            // Next check if there's a pending stake in the snapshots commitments
            if let Some(miner_commitments) = self.commitments.get_mut(signer) {
                if miner_commitments.stake.is_some() {
                    // Has pending stake, can add pledge
                    miner_commitments.pledges.push(commitment_tx.clone());
                    return CommitmentSnapshotStatus::Accepted;
                }
            }

            // No stake found, reject pledge
            CommitmentSnapshotStatus::Unstaked
        }
    }

    /// Removes commitment transactions with specified IDs from the commitment snapshot
    pub fn rollback_commitments(&mut self, commitment_txs: &H256List) -> eyre::Result<()> {
        // Create a HashSet for faster lookups
        let ids_set: HashSet<&H256> = commitment_txs.iter().collect();

        // Store addresses that need cleaning up
        let mut addresses_to_check = Vec::new();

        // First pass: collect all addresses (to avoid borrow issues)
        for address in self.commitments.keys() {
            addresses_to_check.push(*address);
        }

        // Second pass: update each address's commitments
        for address in addresses_to_check {
            if let Some(commitments) = self.commitments.get_mut(&address) {
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
                    self.commitments.remove(&address);
                }
            }
        }

        Ok(())
    }

    /// Collects all commitment transactions from the snapshot for epoch processing
    pub fn get_epoch_commitments(&self) -> Vec<CommitmentTransaction> {
        let mut commitment_tx: Vec<CommitmentTransaction> = Vec::new();

        // First collect all stake transactions in address order
        // BTreeMap is already ordered by keys (addresses)
        for miner_commitments in self.commitments.values() {
            if let Some(stake) = &miner_commitments.stake {
                commitment_tx.push(stake.clone());
            }
        }

        // Then collect all pledge transactions in address order
        for miner_commitments in self.commitments.values() {
            // Add all pledges for this miner
            for pledge in &miner_commitments.pledges {
                commitment_tx.push(pledge.clone());
            }
        }

        commitment_tx
    }
}
