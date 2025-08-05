use irys_primitives::CommitmentType;
use irys_types::{Address, CommitmentTransaction};
use std::collections::BTreeMap;
use tracing::debug;

use super::EpochSnapshot;

#[derive(Debug, PartialEq)]
pub enum CommitmentSnapshotStatus {
    Accepted,           // The commitment is valid and was added to the snapshot
    Unknown,            // The commitment has no status in the snapshot
    Unsupported,        // The commitment is an unsupported type (unstake/unpledge)
    Unstaked,           // The pledge commitment doesn't have a corresponding stake
    InvalidPledgeCount, // The pledge count doesn't match the actual number of pledges
}

#[derive(Debug, Default, Clone)]
pub struct CommitmentSnapshot {
    pub commitments: BTreeMap<Address, MinerCommitments>,
}

#[derive(Default, Debug, Clone)]
pub struct MinerCommitments {
    pub stake: Option<CommitmentTransaction>,
    pub pledges: Vec<CommitmentTransaction>,
}

impl CommitmentSnapshot {
    pub fn new_from_commitments(commitment_txs: Option<Vec<CommitmentTransaction>>) -> Self {
        let mut snapshot = Self::default();

        if let Some(commitment_txs) = commitment_txs {
            for commitment_tx in commitment_txs {
                let _status = snapshot.add_commitment(&commitment_tx, &EpochSnapshot::default());
            }
        }

        snapshot
    }

    /// Checks and returns the status of a commitment transaction
    pub fn get_commitment_status(
        &self,
        commitment_tx: &CommitmentTransaction,
        is_staked_in_current_epoch: bool,
    ) -> CommitmentSnapshotStatus {
        debug!("GetCommitmentStatus message received");

        let commitment_type = &commitment_tx.commitment_type;
        let txid = commitment_tx.id;
        let signer = &commitment_tx.signer;

        // First handle unsupported commitment types
        if !matches!(
            commitment_type,
            CommitmentType::Stake | CommitmentType::Pledge { .. }
        ) {
            debug!(
                "CommitmentStatus is Rejected: unsupported type: {:?}",
                commitment_type
            );
            return CommitmentSnapshotStatus::Unsupported;
        }

        // Handle by the input values commitment type
        let status = match commitment_type {
            CommitmentType::Stake => {
                // If already staked in current epoch, just return Accepted
                if is_staked_in_current_epoch {
                    CommitmentSnapshotStatus::Accepted
                } else {
                    // Only check local commitments if not staked in current epoch
                    if let Some(commitments) = self.commitments.get(signer) {
                        // Check for duplicate stake transaction
                        if commitments.stake.as_ref().is_some_and(|s| s.id == txid) {
                            CommitmentSnapshotStatus::Accepted
                        } else {
                            CommitmentSnapshotStatus::Unknown
                        }
                    } else {
                        // No local commitments and not staked in current epoch
                        CommitmentSnapshotStatus::Unknown
                    }
                }
            }
            CommitmentType::Pledge { .. } => {
                // For pledges, we need to ensure there's a stake (either current epoch or local)
                if is_staked_in_current_epoch {
                    // Has stake in current epoch, check for duplicate pledge locally
                    if let Some(commitments) = self.commitments.get(signer) {
                        if commitments.pledges.iter().any(|p| p.id == txid) {
                            CommitmentSnapshotStatus::Accepted
                        } else {
                            CommitmentSnapshotStatus::Unknown
                        }
                    } else {
                        // No local commitments but has stake in current epoch
                        CommitmentSnapshotStatus::Unknown
                    }
                } else {
                    // Not staked in current epoch, check local commitments
                    if let Some(commitments) = self.commitments.get(signer) {
                        // Check for duplicate pledge transaction
                        if commitments.pledges.iter().any(|p| p.id == txid) {
                            CommitmentSnapshotStatus::Accepted
                        } else if commitments.stake.is_none() {
                            // No local stake and not staked in current epoch
                            CommitmentSnapshotStatus::Unstaked
                        } else {
                            CommitmentSnapshotStatus::Unknown
                        }
                    } else {
                        // No local commitments and not staked in current epoch
                        CommitmentSnapshotStatus::Unstaked
                    }
                }
            }
            _ => CommitmentSnapshotStatus::Unsupported,
        };

        debug!("CommitmentStatus is {:?}", status);
        status
    }

    /// Adds a new commitment transaction to the snapshot and validates its acceptance
    pub fn add_commitment(
        &mut self,
        commitment_tx: &CommitmentTransaction,
        epoch_snapshot: &EpochSnapshot,
    ) -> CommitmentSnapshotStatus {
        let is_staked_in_current_epoch = epoch_snapshot.is_staked(commitment_tx.signer);
        let pledges_in_epoch = epoch_snapshot
            .commitment_state
            .pledge_commitments
            .get(&commitment_tx.signer)
            .map(std::vec::Vec::len)
            .unwrap_or_default();
        debug!("add_commitment() called for {}", commitment_tx.id);
        let signer = &commitment_tx.signer;
        let tx_type = &commitment_tx.commitment_type;

        // Early return for unsupported commitment types
        if !matches!(
            tx_type,
            CommitmentType::Stake | CommitmentType::Pledge { .. }
        ) {
            return CommitmentSnapshotStatus::Unsupported;
        }

        // Handle commitment by type
        match tx_type {
            CommitmentType::Stake => {
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
            }
            CommitmentType::Pledge {
                pledge_count_before_executing,
            } => {
                // First, check if the address has a stake (either in current epoch or pending)
                let has_stake = if is_staked_in_current_epoch {
                    true
                } else if let Some(miner_commitments) = self.commitments.get(signer) {
                    miner_commitments.stake.is_some()
                } else {
                    false
                };

                if !has_stake {
                    return CommitmentSnapshotStatus::Unstaked;
                }

                // Get or create miner commitments
                let miner_commitments = self.commitments.entry(*signer).or_default();

                // Check for duplicate pledge first
                let existing = miner_commitments
                    .pledges
                    .iter()
                    .find(|t| t.id == commitment_tx.id);

                if let Some(existing) = existing {
                    debug!("DUPLICATING PLEDGE: {}", existing.id);
                    return CommitmentSnapshotStatus::Accepted;
                }

                // Validate pledge count matches actual number of existing pledges
                let current_pledge_count = miner_commitments
                    .pledges
                    .len()
                    .saturating_add(pledges_in_epoch)
                    as u64;
                if *pledge_count_before_executing != current_pledge_count {
                    tracing::error!(
                        "Invalid pledge count for {}: expected {}, but miner has {} pledges",
                        commitment_tx.id,
                        pledge_count_before_executing,
                        current_pledge_count
                    );
                    return CommitmentSnapshotStatus::InvalidPledgeCount;
                }

                // Add the pledge
                miner_commitments.pledges.push(commitment_tx.clone());
                CommitmentSnapshotStatus::Accepted
            }
            _ => {
                // This should not be reached due to the early return check,
                // but we handle it for completeness
                CommitmentSnapshotStatus::Unsupported
            }
        }
    }

    /// Collects all commitment transactions from the snapshot for epoch processing
    pub fn get_epoch_commitments(&self) -> Vec<CommitmentTransaction> {
        let mut all_commitments: Vec<CommitmentTransaction> = Vec::new();

        // Collect all commitments from all miners
        for miner_commitments in self.commitments.values() {
            if let Some(stake) = &miner_commitments.stake {
                all_commitments.push(stake.clone());
            }

            for pledge in &miner_commitments.pledges {
                all_commitments.push(pledge.clone());
            }
        }

        // Sort using PrioritizedCommitment wrapper
        let mut prioritized: Vec<_> = all_commitments
            .iter()
            .map(super::PrioritizedCommitment)
            .collect();

        prioritized.sort();

        // Convert back to Vec<CommitmentTransaction>
        prioritized.into_iter().map(|p| p.0.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::epoch_snapshot::commitment_state::CommitmentStateEntry;
    use super::*;
    use irys_primitives::CommitmentStatus;
    use irys_types::{IrysSignature, H256, U256};

    fn create_test_commitment(
        signer: Address,
        commitment_type: CommitmentType,
    ) -> CommitmentTransaction {
        let mut tx = CommitmentTransaction {
            id: H256::zero(),
            anchor: H256::zero(),
            signer,
            signature: IrysSignature::default(),
            fee: 100,
            value: U256::from(1000),
            commitment_type,
            version: 1,
            chain_id: 1,
        };
        // Generate a proper ID for the transaction
        tx.id = H256::random();
        tx
    }

    #[test]
    fn test_pledge_count_validation_success() {
        let mut snapshot = CommitmentSnapshot::default();
        let signer = Address::random();

        // Add stake first
        let stake = create_test_commitment(signer, CommitmentType::Stake);
        let status = snapshot.add_commitment(&stake, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);

        // Add first pledge with count 0
        let pledge1 = create_test_commitment(
            signer,
            CommitmentType::Pledge {
                pledge_count_before_executing: 0,
            },
        );
        let status = snapshot.add_commitment(&pledge1, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);

        // Add second pledge with count 1
        let pledge2 = create_test_commitment(
            signer,
            CommitmentType::Pledge {
                pledge_count_before_executing: 1,
            },
        );
        let status = snapshot.add_commitment(&pledge2, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);

        // Verify the miner has 2 pledges
        assert_eq!(snapshot.commitments[&signer].pledges.len(), 2);
    }

    #[test]
    fn test_pledge_count_validation_failure() {
        let mut snapshot = CommitmentSnapshot::default();
        let signer = Address::random();

        // Add stake first
        let stake = create_test_commitment(signer, CommitmentType::Stake);
        let status = snapshot.add_commitment(&stake, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);

        // Try to add pledge with wrong count (should be 0, but using 1)
        let pledge_wrong_count = create_test_commitment(
            signer,
            CommitmentType::Pledge {
                pledge_count_before_executing: 1,
            },
        );
        let status = snapshot.add_commitment(&pledge_wrong_count, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::InvalidPledgeCount);

        // Verify no pledges were added
        assert_eq!(snapshot.commitments[&signer].pledges.len(), 0);
    }

    #[test]
    fn test_pledge_without_stake() {
        let mut snapshot = CommitmentSnapshot::default();
        let signer = Address::random();

        // Try to add pledge without stake
        let pledge = create_test_commitment(
            signer,
            CommitmentType::Pledge {
                pledge_count_before_executing: 0,
            },
        );
        let status = snapshot.add_commitment(&pledge, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Unstaked);
    }

    #[test]
    fn test_pledge_with_staked_in_epoch() {
        let mut snapshot = CommitmentSnapshot::default();
        let signer = Address::random();

        // Add pledge when already staked in current epoch
        let pledge = create_test_commitment(
            signer,
            CommitmentType::Pledge {
                pledge_count_before_executing: 0,
            },
        );
        // Create an epoch snapshot with the signer already staked
        let mut epoch_snapshot = EpochSnapshot::default();
        epoch_snapshot.commitment_state.stake_commitments.insert(
            signer,
            CommitmentStateEntry {
                id: H256::random(),
                commitment_status: CommitmentStatus::Active,
                partition_hash: None,
                signer,
                amount: 1000,
            },
        );
        let status = snapshot.add_commitment(&pledge, &epoch_snapshot);
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);

        // Add second pledge with correct count
        let pledge2 = create_test_commitment(
            signer,
            CommitmentType::Pledge {
                pledge_count_before_executing: 1,
            },
        );
        // Don't modify the epoch snapshot - the first pledge is already in the local commitment snapshot
        let status = snapshot.add_commitment(&pledge2, &epoch_snapshot);
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);
    }

    #[test]
    fn test_duplicate_stake() {
        let mut snapshot = CommitmentSnapshot::default();
        let signer = Address::random();

        // Add stake
        let stake = create_test_commitment(signer, CommitmentType::Stake);
        let status = snapshot.add_commitment(&stake, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);

        // Try to add another stake (should be accepted but not added)
        let stake2 = create_test_commitment(signer, CommitmentType::Stake);
        let status = snapshot.add_commitment(&stake2, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);

        // Verify only one stake exists
        assert!(snapshot.commitments[&signer].stake.is_some());
    }

    #[test]
    fn test_duplicate_pledge() {
        let mut snapshot = CommitmentSnapshot::default();
        let signer = Address::random();

        // Add stake
        let stake = create_test_commitment(signer, CommitmentType::Stake);
        snapshot.add_commitment(&stake, &EpochSnapshot::default());

        // Add pledge
        let pledge = create_test_commitment(
            signer,
            CommitmentType::Pledge {
                pledge_count_before_executing: 0,
            },
        );
        let pledge_id = pledge.id;
        let status = snapshot.add_commitment(&pledge, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);

        // Try to add the same pledge again (should be accepted but not duplicated)
        let status = snapshot.add_commitment(&pledge, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);

        // Verify only one pledge exists
        assert_eq!(snapshot.commitments[&signer].pledges.len(), 1);
        assert_eq!(snapshot.commitments[&signer].pledges[0].id, pledge_id);
    }

    #[test]
    fn test_unsupported_commitment_types() {
        let mut snapshot = CommitmentSnapshot::default();
        let signer = Address::random();

        // Try to add unstake
        let unstake = create_test_commitment(signer, CommitmentType::Unstake);
        let status = snapshot.add_commitment(&unstake, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Unsupported);

        // Try to add unpledge
        let unpledge = create_test_commitment(
            signer,
            CommitmentType::Unpledge {
                pledge_count_before_executing: 0,
            },
        );
        let status = snapshot.add_commitment(&unpledge, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Unsupported);
    }

    #[test]
    fn test_get_epoch_commitments_ordering() {
        let mut snapshot = CommitmentSnapshot::default();

        // Create multiple signers
        let signer1 = Address::random();
        let signer2 = Address::random();
        let signer3 = Address::random();

        // Add stakes with different fees
        let mut stake1 = create_test_commitment(signer1, CommitmentType::Stake);
        stake1.fee = 100;
        snapshot.add_commitment(&stake1, &EpochSnapshot::default());

        let mut stake2 = create_test_commitment(signer2, CommitmentType::Stake);
        stake2.fee = 200;
        snapshot.add_commitment(&stake2, &EpochSnapshot::default());

        // Add pledges with different counts and fees
        let mut pledge1_count0 = create_test_commitment(
            signer1,
            CommitmentType::Pledge {
                pledge_count_before_executing: 0,
            },
        );
        pledge1_count0.fee = 50;
        snapshot.add_commitment(&pledge1_count0, &EpochSnapshot::default());

        let mut pledge2_count0 = create_test_commitment(
            signer2,
            CommitmentType::Pledge {
                pledge_count_before_executing: 0,
            },
        );
        pledge2_count0.fee = 150;
        snapshot.add_commitment(&pledge2_count0, &EpochSnapshot::default());

        // Add another stake after some pledges
        let mut stake3 = create_test_commitment(signer3, CommitmentType::Stake);
        stake3.fee = 50;
        snapshot.add_commitment(&stake3, &EpochSnapshot::default());

        // Add pledge with higher count
        let mut pledge1_count1 = create_test_commitment(
            signer1,
            CommitmentType::Pledge {
                pledge_count_before_executing: 1,
            },
        );
        pledge1_count1.fee = 300;
        snapshot.add_commitment(&pledge1_count1, &EpochSnapshot::default());

        // Get commitments and verify order
        let commitments = snapshot.get_epoch_commitments();

        // Should be ordered as:
        // 1. All stakes (by fee descending): stake2 (200), stake1 (100), stake3 (50)
        // 2. Pledges count 0 (by fee descending): pledge2_count0 (150), pledge1_count0 (50)
        // 3. Pledges count 1: pledge1_count1 (300)
        assert_eq!(commitments.len(), 6);
        assert_eq!(commitments[0].id, stake2.id); // Stake with fee 200
        assert_eq!(commitments[1].id, stake1.id); // Stake with fee 100
        assert_eq!(commitments[2].id, stake3.id); // Stake with fee 50
        assert_eq!(commitments[3].id, pledge2_count0.id); // Pledge count 0, fee 150
        assert_eq!(commitments[4].id, pledge1_count0.id); // Pledge count 0, fee 50
        assert_eq!(commitments[5].id, pledge1_count1.id); // Pledge count 1, fee 300
    }
}
