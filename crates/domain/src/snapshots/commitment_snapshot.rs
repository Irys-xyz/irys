use super::EpochSnapshot;
use irys_types::{CommitmentTransaction, CommitmentTypeV2, IrysAddress};

#[cfg(test)]
use irys_types::CommitmentTypeV1;
use std::{
    collections::BTreeMap,
    hash::{Hash as _, Hasher as _},
};
use tracing::debug;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitmentSnapshotStatus {
    Accepted,           // The commitment is valid and was added to the snapshot
    Unknown,            // The commitment has no status in the snapshot
    Unstaked,           // The pledge commitment doesn't have a corresponding stake
    InvalidPledgeCount, // The pledge count doesn't match the actual number of pledges
    Unowned,            // Target capacity partition is not owned by signer
    UnpledgePending,    // Duplicate unpledge for same partition in this snapshot
    UnstakePending,     // Duplicate unstake for the signer within this snapshot
    HasActivePledges,   // Unstake not allowed because signer still has pledges
}

#[derive(Debug, Default, Clone, Hash)]
pub struct CommitmentSnapshot {
    pub commitments: BTreeMap<IrysAddress, MinerCommitments>,
}

#[derive(Default, Debug, Clone, Hash)]
pub struct MinerCommitments {
    pub stake: Option<CommitmentTransaction>,
    pub pledges: Vec<CommitmentTransaction>,
    pub unpledges: Vec<CommitmentTransaction>,
    pub unstake: Option<CommitmentTransaction>,
    pub update_reward_address: Option<CommitmentTransaction>,
}

impl CommitmentSnapshot {
    /// Returns true if signer has an active stake (either in epoch snapshot or pending locally)
    fn has_stake(&self, signer: &IrysAddress, epoch_snapshot: &EpochSnapshot) -> bool {
        epoch_snapshot.is_staked(*signer)
            || self
                .commitments
                .get(signer)
                .is_some_and(|mc| mc.stake.is_some())
    }

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
        epoch_snapshot: &EpochSnapshot,
    ) -> CommitmentSnapshotStatus {
        debug!("GetCommitmentStatus message received");

        let commitment_type = &commitment_tx.commitment_type();
        let txid = commitment_tx.id();
        let signer = &commitment_tx.signer();

        // Handle by the input values commitment type
        let status = match commitment_type {
            CommitmentTypeV2::Stake => {
                // If already staked in current epoch, just return Accepted
                if epoch_snapshot.is_staked(*signer) {
                    CommitmentSnapshotStatus::Accepted
                } else {
                    // Only check local commitments if not staked in current epoch
                    if let Some(commitments) = self.commitments.get(signer) {
                        // Check for duplicate stake transaction
                        if commitments.stake.as_ref().is_some_and(|s| s.id() == txid) {
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
            CommitmentTypeV2::Pledge { .. } | CommitmentTypeV2::Unpledge { .. } => {
                // For pledges, we need to ensure there's a stake (either current epoch or local)
                if epoch_snapshot.is_staked(*signer) {
                    // Has stake in current epoch, check for duplicate pledge locally
                    if let Some(commitments) = self.commitments.get(signer) {
                        // If unstake is pending for this signer, pledges are not allowed
                        if commitments.unstake.is_some() {
                            return CommitmentSnapshotStatus::UnstakePending;
                        }
                        if commitments.pledges.iter().any(|p| p.id() == txid) {
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
                        // Check for duplicate pledge transaction and unstake gating
                        if commitments.pledges.iter().any(|p| p.id() == txid) {
                            CommitmentSnapshotStatus::Accepted
                        } else if commitments.unstake.is_some() {
                            CommitmentSnapshotStatus::UnstakePending
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
            CommitmentTypeV2::Unstake => {
                // Unstake requires signer to be staked (epoch or local) and not already pending unstake
                if !self.has_stake(signer, epoch_snapshot) {
                    return CommitmentSnapshotStatus::Unstaked;
                }
                if let Some(commitments) = self.commitments.get(signer) {
                    if commitments.unstake.as_ref().is_some_and(|u| u.id() == txid) {
                        CommitmentSnapshotStatus::Accepted
                    } else if commitments.unstake.is_some() {
                        CommitmentSnapshotStatus::UnstakePending
                    } else {
                        CommitmentSnapshotStatus::Unknown
                    }
                } else {
                    CommitmentSnapshotStatus::Unknown
                }
            }
            CommitmentTypeV2::UpdateRewardAddress { .. } => {
                // UpdateRewardAddress requires signer to be staked (epoch or local)
                if !self.has_stake(signer, epoch_snapshot) {
                    return CommitmentSnapshotStatus::Unstaked;
                }
                // Check if this is the currently stored update
                if let Some(commitments) = self.commitments.get(signer)
                    && commitments
                        .update_reward_address
                        .as_ref()
                        .is_some_and(|u| u.id() == txid)
                {
                    return CommitmentSnapshotStatus::Accepted;
                }
                CommitmentSnapshotStatus::Unknown
            }
        };

        debug!("CommitmentStatus is {:?}", status);
        status
    }

    fn active_pledge_count(miner_commitments: &MinerCommitments, pledges_in_epoch: usize) -> usize {
        let total = pledges_in_epoch + miner_commitments.pledges.len();
        total.saturating_sub(miner_commitments.unpledges.len())
    }

    /// Adds a new commitment transaction to the snapshot and validates its acceptance
    pub fn add_commitment(
        &mut self,
        commitment_tx: &CommitmentTransaction,
        epoch_snapshot: &EpochSnapshot,
    ) -> CommitmentSnapshotStatus {
        let is_staked_in_current_epoch = epoch_snapshot.is_staked(commitment_tx.signer());
        let pledges_in_epoch = epoch_snapshot
            .commitment_state
            .pledge_commitments
            .get(&commitment_tx.signer())
            .map(std::vec::Vec::len)
            .unwrap_or_default();
        let signer = &commitment_tx.signer();
        let tx_type = &commitment_tx.commitment_type();

        debug!(
            "add_commitment() called for tx {}, address {}",
            commitment_tx.id(),
            &signer
        );

        // Handle commitment by type
        match tx_type {
            CommitmentTypeV2::Stake => {
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
            CommitmentTypeV2::Pledge {
                pledge_count_before_executing,
            } => {
                // First, check if the address has a stake (either in current epoch or pending)
                if !self.has_stake(signer, epoch_snapshot) {
                    return CommitmentSnapshotStatus::Unstaked;
                }

                // Get or create miner commitments
                let miner_commitments = self.commitments.entry(*signer).or_default();

                // Disallow pledges while an unstake is in progress
                if miner_commitments.unstake.is_some() {
                    return CommitmentSnapshotStatus::UnstakePending;
                }

                // Check for duplicate pledge first
                let existing = miner_commitments
                    .pledges
                    .iter()
                    .find(|t| t.id() == commitment_tx.id());

                if let Some(_existing) = existing {
                    return CommitmentSnapshotStatus::Accepted;
                }

                // Validate pledge count matches actual number of existing pledges
                let current_pledge_count =
                    Self::active_pledge_count(miner_commitments, pledges_in_epoch) as u64;
                if *pledge_count_before_executing != current_pledge_count {
                    tracing::error!(
                        "Invalid pledge count for {}: expected {}, but miner {} has {} pledges",
                        commitment_tx.id(),
                        pledge_count_before_executing,
                        &signer,
                        current_pledge_count
                    );
                    return CommitmentSnapshotStatus::InvalidPledgeCount;
                }

                // Add the pledge
                miner_commitments.pledges.push(commitment_tx.clone());
                CommitmentSnapshotStatus::Accepted
            }
            CommitmentTypeV2::Unpledge {
                pledge_count_before_executing,
                partition_hash,
            } => {
                // Require staked or pending local stake
                if !self.has_stake(signer, epoch_snapshot) {
                    return CommitmentSnapshotStatus::Unstaked;
                }

                // Validate pledge count
                let miner_commitments = self.commitments.entry(*signer).or_default();
                let current_pledge_count =
                    Self::active_pledge_count(miner_commitments, pledges_in_epoch) as u64;
                if *pledge_count_before_executing != current_pledge_count {
                    tracing::error!(
                        tx.id = ?commitment_tx.id(),
                        custom.pledge_count_before_executing = ?pledge_count_before_executing,
                        custom.current_pledge_count = ?current_pledge_count,
                        "rejected"
                    );
                    return CommitmentSnapshotStatus::InvalidPledgeCount;
                }

                // Capacity ownership check
                let owned = epoch_snapshot
                    .partition_assignments
                    .get_assignment(*partition_hash)
                    .is_some_and(|pa| pa.miner_address == *signer);
                if !owned {
                    return CommitmentSnapshotStatus::Unowned;
                }

                // Duplicate check
                if miner_commitments.unpledges.iter().any(|tx| {
                    matches!(
                        tx.commitment_type(),
                        CommitmentTypeV2::Unpledge { partition_hash: ph, .. } if ph == *partition_hash
                    )
                }) {
                    return CommitmentSnapshotStatus::UnpledgePending;
                }

                miner_commitments.unpledges.push(commitment_tx.clone());
                CommitmentSnapshotStatus::Accepted
            }
            CommitmentTypeV2::Unstake => {
                // Require staked or pending local stake
                if !self.has_stake(signer, epoch_snapshot) {
                    return CommitmentSnapshotStatus::Unstaked;
                }

                // Compute effective pledge count (epoch + local - local unpledges)
                let miner_commitments = self.commitments.entry(*signer).or_default();
                let current_pledge_count =
                    Self::active_pledge_count(miner_commitments, pledges_in_epoch);
                if current_pledge_count > 0 {
                    return CommitmentSnapshotStatus::HasActivePledges;
                }

                if miner_commitments.unstake.is_some() {
                    return CommitmentSnapshotStatus::UnstakePending;
                }

                miner_commitments.unstake = Some(commitment_tx.clone());
                CommitmentSnapshotStatus::Accepted
            }
            CommitmentTypeV2::UpdateRewardAddress { .. } => {
                if !self.has_stake(signer, epoch_snapshot) {
                    return CommitmentSnapshotStatus::Unstaked;
                }

                let miner_commitments = self.commitments.entry(*signer).or_default();

                // Idempotency: if this exact tx is already stored, return Accepted
                if miner_commitments
                    .update_reward_address
                    .as_ref()
                    .is_some_and(|u| u.id() == commitment_tx.id())
                {
                    return CommitmentSnapshotStatus::Accepted;
                }

                // Store the update (block producer ensures correct ordering, last one wins)
                miner_commitments.update_reward_address = Some(commitment_tx.clone());

                CommitmentSnapshotStatus::Accepted
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

            for unpledge in &miner_commitments.unpledges {
                all_commitments.push(unpledge.clone());
            }

            if let Some(unstake) = &miner_commitments.unstake {
                all_commitments.push(unstake.clone());
            }

            if let Some(update_reward_address) = &miner_commitments.update_reward_address {
                all_commitments.push(update_reward_address.clone());
            }
        }

        // Sort commitments directly
        all_commitments.sort();

        all_commitments
    }

    pub fn is_staked(&self, miner_address: IrysAddress) -> bool {
        let commitments_for_address = self.commitments.get_key_value(&miner_address);
        if let Some((_, commitments)) = commitments_for_address
            && commitments.stake.is_some()
        {
            return true;
        }
        false
    }

    // NON CANONICAL HASH
    // SHOULD BE USED FOR DEBUGGING ONLY
    pub fn get_hash(&self) -> String {
        let mut hasher = std::hash::DefaultHasher::new();
        self.hash(&mut hasher);
        let res = hasher.finish();
        use base58::ToBase58 as _;
        res.to_le_bytes().to_base58()
    }
}

#[cfg(test)]
mod tests {
    use super::super::epoch_snapshot::commitment_state::{PledgeEntry, StakeEntry};
    use super::*;
    use irys_types::CommitmentStatus;
    use irys_types::{H256, IrysSignature, U256, partition::PartitionAssignment};

    fn create_test_commitment(
        signer: IrysAddress,
        commitment_type: CommitmentTypeV1,
        value: U256,
    ) -> CommitmentTransaction {
        let mut tx = CommitmentTransaction::V2(irys_types::CommitmentV2WithMetadata {
            tx: irys_types::CommitmentTransactionV2 {
                id: H256::zero(),
                anchor: H256::zero(),
                signer,
                signature: IrysSignature::default(),
                fee: 100,
                value,
                commitment_type: commitment_type.into(),
                chain_id: 1,
            },
            metadata: Default::default(),
        });
        // Generate a proper ID for the transaction
        tx.set_id(H256::random());
        tx
    }

    fn create_test_commitment_v2(
        signer: IrysAddress,
        commitment_type: CommitmentTypeV2,
        value: U256,
    ) -> CommitmentTransaction {
        let mut tx = CommitmentTransaction::V2(irys_types::CommitmentV2WithMetadata {
            tx: irys_types::CommitmentTransactionV2 {
                id: H256::zero(),
                anchor: H256::zero(),
                signer,
                signature: IrysSignature::default(),
                fee: 100,
                value,
                commitment_type,
                chain_id: 1,
            },
            metadata: Default::default(),
        });
        tx.set_id(H256::random());
        tx
    }

    #[test]
    fn test_pledge_count_validation_success() {
        let mut snapshot = CommitmentSnapshot::default();
        let signer = IrysAddress::random();

        // Add stake first
        let stake = create_test_commitment(signer, CommitmentTypeV1::Stake, U256::from(1000));
        let status = snapshot.add_commitment(&stake, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);

        // Add first pledge with count 0
        let pledge1 = create_test_commitment(
            signer,
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 0,
            },
            U256::from(1000),
        );
        let status = snapshot.add_commitment(&pledge1, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);

        // Add second pledge with count 1
        let pledge2 = create_test_commitment(
            signer,
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 1,
            },
            U256::from(1000),
        );
        let status = snapshot.add_commitment(&pledge2, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);

        // Verify the miner has 2 pledges
        assert_eq!(snapshot.commitments[&signer].pledges.len(), 2);
    }

    #[test]
    fn test_pledge_count_validation_failure() {
        let mut snapshot = CommitmentSnapshot::default();
        let signer = IrysAddress::random();

        // Add stake first
        let stake = create_test_commitment(signer, CommitmentTypeV1::Stake, U256::from(1000));
        let status = snapshot.add_commitment(&stake, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);

        // Try to add pledge with wrong count (should be 0, but using 1)
        let pledge_wrong_count = create_test_commitment(
            signer,
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 1,
            },
            U256::from(1000),
        );
        let status = snapshot.add_commitment(&pledge_wrong_count, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::InvalidPledgeCount);

        // Verify no pledges were added
        assert_eq!(snapshot.commitments[&signer].pledges.len(), 0);
    }

    #[test]
    fn test_unpledge_counts_include_snapshot_changes() {
        let mut snapshot = CommitmentSnapshot::default();
        let signer = IrysAddress::random();
        let partition_hashes = vec![H256::random(), H256::random(), H256::random()];

        let mut epoch_snapshot = EpochSnapshot::default();
        epoch_snapshot.commitment_state.pledge_commitments.insert(
            signer,
            partition_hashes
                .iter()
                .map(|hash| PledgeEntry {
                    id: H256::random(),
                    commitment_status: CommitmentStatus::Active,
                    signer,
                    amount: U256::from(1_000_u64),
                    partition_hash: Some(*hash),
                })
                .collect(),
        );
        epoch_snapshot.commitment_state.stake_commitments.insert(
            signer,
            StakeEntry {
                id: H256::random(),
                commitment_status: CommitmentStatus::Active,
                signer,
                amount: U256::from(5_000_u64),
                reward_address: signer,
            },
        );
        for hash in &partition_hashes {
            epoch_snapshot
                .partition_assignments
                .capacity_partitions
                .insert(
                    *hash,
                    PartitionAssignment {
                        partition_hash: *hash,
                        miner_address: signer,
                        ledger_id: None,
                        slot_index: None,
                    },
                );
        }

        // First unpledge should see 3 total pledges
        let first_unpledge = create_test_commitment(
            signer,
            CommitmentTypeV1::Unpledge {
                pledge_count_before_executing: 3,
                partition_hash: partition_hashes[0],
            },
            U256::from(500_u64),
        );
        assert_eq!(
            snapshot.add_commitment(&first_unpledge, &epoch_snapshot),
            CommitmentSnapshotStatus::Accepted
        );

        // Second unpledge should observe that one pledge is already pending removal
        let second_unpledge = create_test_commitment(
            signer,
            CommitmentTypeV1::Unpledge {
                pledge_count_before_executing: 2,
                partition_hash: partition_hashes[1],
            },
            U256::from(400_u64),
        );
        assert_eq!(
            snapshot.add_commitment(&second_unpledge, &epoch_snapshot),
            CommitmentSnapshotStatus::Accepted
        );
    }

    #[test]
    fn test_pledge_after_unpledge_uses_effective_count() {
        let mut snapshot = CommitmentSnapshot::default();
        let signer = IrysAddress::random();
        let partition_hashes = vec![H256::random(), H256::random()];

        let mut epoch_snapshot = EpochSnapshot::default();
        epoch_snapshot.commitment_state.pledge_commitments.insert(
            signer,
            partition_hashes
                .iter()
                .map(|hash| PledgeEntry {
                    id: H256::random(),
                    commitment_status: CommitmentStatus::Active,
                    signer,
                    amount: U256::from(1_000_u64),
                    partition_hash: Some(*hash),
                })
                .collect(),
        );
        epoch_snapshot.commitment_state.stake_commitments.insert(
            signer,
            StakeEntry {
                id: H256::random(),
                commitment_status: CommitmentStatus::Active,
                signer,
                amount: U256::from(5_000_u64),
                reward_address: signer,
            },
        );
        for hash in &partition_hashes {
            epoch_snapshot
                .partition_assignments
                .capacity_partitions
                .insert(
                    *hash,
                    PartitionAssignment {
                        partition_hash: *hash,
                        miner_address: signer,
                        ledger_id: None,
                        slot_index: None,
                    },
                );
        }

        // Remove the newest pledge (count should be 2 before executing)
        let unpledge = create_test_commitment(
            signer,
            CommitmentTypeV1::Unpledge {
                pledge_count_before_executing: 2,
                partition_hash: partition_hashes[0],
            },
            U256::from(500_u64),
        );
        assert_eq!(
            snapshot.add_commitment(&unpledge, &epoch_snapshot),
            CommitmentSnapshotStatus::Accepted
        );

        // Adding a pledge should now see only one active pledge remaining
        let pledge = create_test_commitment(
            signer,
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 1,
            },
            U256::from(750_u64),
        );
        assert_eq!(
            snapshot.add_commitment(&pledge, &epoch_snapshot),
            CommitmentSnapshotStatus::Accepted
        );
    }

    #[test]
    fn test_pledge_without_stake() {
        let mut snapshot = CommitmentSnapshot::default();
        let signer = IrysAddress::random();

        // Try to add pledge without stake
        let pledge = create_test_commitment(
            signer,
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 0,
            },
            U256::from(1000),
        );
        let status = snapshot.add_commitment(&pledge, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Unstaked);
    }

    #[test]
    fn test_pledge_with_staked_in_epoch() {
        let mut snapshot = CommitmentSnapshot::default();
        let signer = IrysAddress::random();

        // Add pledge when already staked in current epoch
        let pledge = create_test_commitment(
            signer,
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 0,
            },
            U256::from(1000),
        );
        // Create an epoch snapshot with the signer already staked
        let mut epoch_snapshot = EpochSnapshot::default();
        epoch_snapshot.commitment_state.stake_commitments.insert(
            signer,
            StakeEntry {
                id: H256::random(),
                commitment_status: CommitmentStatus::Active,
                signer,
                amount: U256::from(1000),
                reward_address: signer,
            },
        );
        let status = snapshot.add_commitment(&pledge, &epoch_snapshot);
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);

        // Add second pledge with correct count
        let pledge2 = create_test_commitment(
            signer,
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 1,
            },
            U256::from(1000),
        );
        // Don't modify the epoch snapshot - the first pledge is already in the local commitment snapshot
        let status = snapshot.add_commitment(&pledge2, &epoch_snapshot);
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);
    }

    #[test]
    fn test_duplicate_stake() {
        let mut snapshot = CommitmentSnapshot::default();
        let signer = IrysAddress::random();

        // Add stake
        let stake = create_test_commitment(signer, CommitmentTypeV1::Stake, U256::from(1000));
        let status = snapshot.add_commitment(&stake, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);

        // Try to add another stake (should be accepted but not added)
        let stake2 = create_test_commitment(signer, CommitmentTypeV1::Stake, U256::from(1000));
        let status = snapshot.add_commitment(&stake2, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);

        // Verify only one stake exists
        assert!(snapshot.commitments[&signer].stake.is_some());
    }

    #[test]
    fn test_duplicate_pledge() {
        let mut snapshot = CommitmentSnapshot::default();
        let signer = IrysAddress::random();

        // Add stake
        let stake = create_test_commitment(signer, CommitmentTypeV1::Stake, U256::from(1000));
        snapshot.add_commitment(&stake, &EpochSnapshot::default());

        // Add pledge
        let pledge = create_test_commitment(
            signer,
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 0,
            },
            U256::from(1000),
        );
        let pledge_id = pledge.id();
        let status = snapshot.add_commitment(&pledge, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);

        // Try to add the same pledge again (should be accepted but not duplicated)
        let status = snapshot.add_commitment(&pledge, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Accepted);

        // Verify only one pledge exists
        assert_eq!(snapshot.commitments[&signer].pledges.len(), 1);
        assert_eq!(snapshot.commitments[&signer].pledges[0].id(), pledge_id);
    }

    #[test]
    fn test_unstake_without_existing_stake_is_rejected() {
        let mut snapshot = CommitmentSnapshot::default();
        let signer = IrysAddress::random();

        // Try to add unstake
        let unstake = create_test_commitment(signer, CommitmentTypeV1::Unstake, U256::from(1000));
        let status = snapshot.add_commitment(&unstake, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Unstaked);
    }

    #[test]
    fn test_update_reward_address_without_stake() {
        let mut snapshot = CommitmentSnapshot::default();
        let signer = IrysAddress::random();
        let new_reward_address = IrysAddress::random();

        // Try to update reward address without stake
        let update_tx = create_test_commitment_v2(
            signer,
            CommitmentTypeV2::UpdateRewardAddress { new_reward_address },
            U256::zero(),
        );
        let status = snapshot.add_commitment(&update_tx, &EpochSnapshot::default());
        assert_eq!(status, CommitmentSnapshotStatus::Unstaked);
    }

    #[test]
    fn test_get_epoch_commitments_ordering() {
        let mut snapshot = CommitmentSnapshot::default();

        // Create multiple signers
        let signer1 = IrysAddress::random();
        let signer2 = IrysAddress::random();
        let signer3 = IrysAddress::random();

        // Add stakes with different fees
        let mut stake1 = create_test_commitment(signer1, CommitmentTypeV1::Stake, U256::from(1000));
        stake1.set_fee(100);
        snapshot.add_commitment(&stake1, &EpochSnapshot::default());

        let mut stake2 = create_test_commitment(signer2, CommitmentTypeV1::Stake, U256::from(1000));
        stake2.set_fee(200);
        snapshot.add_commitment(&stake2, &EpochSnapshot::default());

        // Add pledges with different counts and fees
        let mut pledge1_count0 = create_test_commitment(
            signer1,
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 0,
            },
            U256::from(1000),
        );
        pledge1_count0.set_fee(50);
        snapshot.add_commitment(&pledge1_count0, &EpochSnapshot::default());

        let mut pledge2_count0 = create_test_commitment(
            signer2,
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 0,
            },
            U256::from(1000),
        );
        pledge2_count0.set_fee(150);
        snapshot.add_commitment(&pledge2_count0, &EpochSnapshot::default());

        // Add another stake after some pledges
        let mut stake3 = create_test_commitment(signer3, CommitmentTypeV1::Stake, U256::from(1000));
        stake3.set_fee(50);
        snapshot.add_commitment(&stake3, &EpochSnapshot::default());

        // Add pledge with higher count
        let mut pledge1_count1 = create_test_commitment(
            signer1,
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 1,
            },
            U256::from(1000),
        );
        pledge1_count1.set_fee(300);
        snapshot.add_commitment(&pledge1_count1, &EpochSnapshot::default());

        // Get commitments and verify order
        let commitments = snapshot.get_epoch_commitments();

        // Should be ordered as:
        // 1. All stakes (by fee descending): stake2 (200), stake1 (100), stake3 (50)
        // 2. Pledges count 0 (by fee descending): pledge2_count0 (150), pledge1_count0 (50)
        // 3. Pledges count 1: pledge1_count1 (300)
        assert_eq!(commitments.len(), 6);
        assert_eq!(commitments[0].id(), stake2.id()); // Stake with fee 200
        assert_eq!(commitments[1].id(), stake1.id()); // Stake with fee 100
        assert_eq!(commitments[2].id(), stake3.id()); // Stake with fee 50
        assert_eq!(commitments[3].id(), pledge2_count0.id()); // Pledge count 0, fee 150
        assert_eq!(commitments[4].id(), pledge1_count0.id()); // Pledge count 0, fee 50
        assert_eq!(commitments[5].id(), pledge1_count1.id()); // Pledge count 1, fee 300
    }

    #[test]
    fn test_stake_commitment_amount_tracking() {
        let mut snapshot = CommitmentSnapshot::default();

        // Create and add stakes with different amounts
        let test_cases = vec![
            (
                IrysAddress::random(),
                U256::from(10_000_000_000_000_000_000_000_u128),
            ), // 10k tokens
            (
                IrysAddress::random(),
                U256::from(50_000_000_000_000_000_000_000_u128),
            ), // 50k tokens
            (
                IrysAddress::random(),
                U256::from(20_000_000_000_000_000_000_000_u128),
            ), // 20k tokens
        ];

        for (signer, amount) in &test_cases {
            let stake = create_test_commitment(*signer, CommitmentTypeV1::Stake, *amount);
            snapshot.add_commitment(&stake, &EpochSnapshot::default());
        }

        // Verify all amounts are correctly stored
        for (signer, expected_amount) in &test_cases {
            assert_eq!(
                snapshot.commitments[signer].stake.as_ref().unwrap().value(),
                *expected_amount
            );
        }
    }

    #[test]
    fn test_pledge_commitment_amount_tracking() {
        let mut snapshot = CommitmentSnapshot::default();
        let signer = IrysAddress::random();

        // Add stake first
        let stake_amount = U256::from(20_000_000_000_000_000_000_000_u128); // 20k tokens
        snapshot.add_commitment(
            &create_test_commitment(signer, CommitmentTypeV1::Stake, stake_amount),
            &EpochSnapshot::default(),
        );

        // Add pledges with different amounts
        let pledge_amounts = vec![
            U256::from(1_000_000_000_000_000_000_000_u128), // 1k tokens
            U256::from(2_500_000_000_000_000_000_000_u128), // 2.5k tokens
            U256::from(5_000_000_000_000_000_000_000_u128), // 5k tokens
            U256::from(10_000_000_000_000_000_000_000_u128), // 10k tokens
        ];

        for (i, &amount) in pledge_amounts.iter().enumerate() {
            snapshot.add_commitment(
                &create_test_commitment(
                    signer,
                    CommitmentTypeV1::Pledge {
                        pledge_count_before_executing: i as u64,
                    },
                    amount,
                ),
                &EpochSnapshot::default(),
            );
        }

        // Verify stake amount
        assert_eq!(
            snapshot.commitments[&signer]
                .stake
                .as_ref()
                .unwrap()
                .value(),
            stake_amount
        );

        // Verify pledge amounts
        let pledges = &snapshot.commitments[&signer].pledges;
        assert_eq!(pledges.len(), pledge_amounts.len());
        for (pledge, &expected) in pledges.iter().zip(&pledge_amounts) {
            assert_eq!(pledge.value(), expected);
        }
    }
}
