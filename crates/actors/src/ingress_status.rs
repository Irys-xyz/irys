//! Read-only Submit-ledger ingress assessment for one data root.
//!
//! Expiry, signer dedup, and anchor filtering are the functions promotion uses.
//! This module applies them to the proofs
//! stored for one data root and names the miner each stored proof belongs to.

use crate::block_producer::ledger_expiry::resolve_promotion_heights_on_branch;
use crate::block_validation::{
    clamped_assigned_requirement, required_ingress_proofs_total, submit_assignees_for_range,
};
use crate::tx_selector::helpers::{
    anchor_valid_ingress_proofs, dedup_ingress_proofs_by_signer, keep_first_per_address,
    min_ingress_proof_anchor_height, unexpired_ingress_proofs,
};
use eyre::eyre;
use irys_database::db::DatabaseProviderCacheExt as _;
use irys_database::tables::CompactCachedIngressProof;
use irys_database::{DatabaseProvider, ingress_proofs_by_data_root};
use irys_domain::{BlockTreeReadGuard, EpochSnapshot};
use irys_types::{Config, DataTransactionHeader, H256, IrysAddress, UnixTimestamp};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::debug;

/// Chain and epoch inputs for one assessment.
///
/// `parent_height` is the canonical block the next produced block would extend.
/// `requirement_timestamp` is the timestamp that block would carry. Hardfork
/// rules switch on that wall-clock timestamp.
pub struct IngressStatusQuery<'a> {
    pub parent_height: u64,
    pub requirement_timestamp: UnixTimestamp,
    pub block_tree: &'a BlockTreeReadGuard,
    pub db: &'a DatabaseProvider,
    pub config: &'a Config,
    pub epoch_snapshot: &'a EpochSnapshot,
}

/// Unpromoted proofs for one data root.
///
/// `signers` names every observed stored address, in first-seen cache order.
/// `required_assigned` is the configured assignee gate after `min` with the
/// number of unique miners on the Submit slots the tx's canonical range
/// intersects. It is 0 when the tx has no canonical Submit range yet. Counts
/// and deficits are computed from
/// `signers` and these two requirements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngressProofAssessment {
    pub required_total: u64,
    pub required_assigned: u64,
    pub signers: Vec<IngressProofSigner>,
}

impl IngressProofAssessment {
    pub fn observed(&self) -> u64 {
        self.signers.len() as u64
    }

    pub fn assigned(&self) -> u64 {
        self.signers.iter().filter(|signer| signer.assigned).count() as u64
    }

    pub fn unassigned(&self) -> u64 {
        self.signers
            .iter()
            .filter(|signer| !signer.assigned)
            .count() as u64
    }

    pub fn eligible_assigned(&self) -> u64 {
        self.signers
            .iter()
            .filter(|signer| signer.assigned && signer.eligible)
            .count() as u64
    }

    pub fn eligible_unassigned(&self) -> u64 {
        self.signers
            .iter()
            .filter(|signer| !signer.assigned && signer.eligible)
            .count() as u64
    }

    pub fn eligible(&self) -> u64 {
        self.eligible_assigned() + self.eligible_unassigned()
    }

    pub fn remaining_total(&self) -> u64 {
        self.required_total.saturating_sub(self.eligible())
    }

    pub fn remaining_assigned(&self) -> u64 {
        self.required_assigned
            .saturating_sub(self.eligible_assigned())
    }

    /// True only when both deficits are zero. This is not a promise that the
    /// next block will include the transaction.
    pub fn proof_requirements_met(&self) -> bool {
        self.remaining_total() == 0 && self.remaining_assigned() == 0
    }
}

/// One stored miner for the assessed data root.
///
/// `address` is the cache subkey promotion deduplicates on: the miner who owns
/// the proof, not a peer that only relayed it. Duplicate proofs from one
/// address are one row.
///
/// `eligible` means the proof currently qualifies toward promotion. `assigned`
/// is Submit-range assignment and includes proofs that are not eligible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngressProofSigner {
    pub address: IrysAddress,
    pub assigned: bool,
    pub eligible: bool,
}

/// Load the cache rows for `tx_header.data_root` and assess them.
///
/// A cache read error is returned. An empty walker is a confirmed empty set.
pub fn load_submit_ingress_assessment(
    tx_header: &DataTransactionHeader,
    query: &IngressStatusQuery<'_>,
) -> eyre::Result<IngressProofAssessment> {
    let stored = query
        .db
        .view_cache_eyre(|read_tx| ingress_proofs_by_data_root(read_tx, tx_header.data_root))?;
    let proofs = stored.into_iter().map(|(_, proof)| proof).collect();
    assess_submit_ingress_proofs(tx_header, proofs, query)
}

/// Assess an already loaded proof set.
///
/// The caller supplies every cached proof for this data root. Signers are the
/// addresses stored with those proofs.
///
/// A corrupt canonical Submit range, or an anchor read that returns a database
/// error, is `Err`. A resolved empty range is a real answer: nobody is
/// assigned, and the assignee gate clamps to 0. The range is read even when
/// `stored_proofs` is empty.
///
/// A stored proof that fails `pre_validate` stays observed and is not eligible.
/// The selector still aborts that transaction when any candidate fails
/// `pre_validate`.
pub(crate) fn assess_submit_ingress_proofs(
    tx_header: &DataTransactionHeader,
    stored_proofs: Vec<CompactCachedIngressProof>,
    query: &IngressStatusQuery<'_>,
) -> eyre::Result<IngressProofAssessment> {
    let (observed, _) = keep_first_per_address(
        stored_proofs.iter().map(|proof| proof.address).collect(),
        |address| *address,
    );
    let candidates = selector_candidates(tx_header, stored_proofs, query)?;
    let block_range = crate::tx_inclusion::find_canonical_ledger_range(
        &tx_header.id,
        query.parent_height,
        query.config.consensus.block_migration_depth,
        query.config.consensus.chunk_size,
        query.block_tree,
        query.db,
    )
    .map_err(|e| eyre!("canonical submit range lookup failed: {e:#}"))?;
    // Same rule as `get_assigned_ingress_proofs`: a signer is assigned when it
    // holds a Submit slot intersecting the tx's canonical range, and the
    // assignee gate clamps to the number of such miners.
    let assignees = block_range
        .as_ref()
        .map(|range| submit_assignees_for_range(range, query.config, query.epoch_snapshot))
        .unwrap_or_default();
    let assigned_miners = assignees.len();
    let candidate_assigned: HashMap<IrysAddress, bool> = candidates
        .iter()
        .map(|(stored, recovered)| (*stored, assignees.contains(recovered)))
        .collect();

    let signers = observed
        .into_iter()
        .map(|address| {
            // A stored proof that is not a candidate (expired, bad anchor, or
            // failed `pre_validate`) has no recovered signer, so its stored
            // address is checked directly.
            let (assigned, eligible) = match candidate_assigned.get(&address) {
                Some(&assigned) => (
                    assigned,
                    assigned || query.epoch_snapshot.is_staked(address),
                ),
                None => (assignees.contains(&address), false),
            };
            IngressProofSigner {
                address,
                assigned,
                eligible,
            }
        })
        .collect();

    let configured_assigned = usize::from(
        query
            .config
            .number_of_ingress_proofs_from_assignees_at(query.requirement_timestamp),
    );
    Ok(IngressProofAssessment {
        required_total: u64::from(required_ingress_proofs_total(
            &query.config.consensus.hardforks,
            query.requirement_timestamp,
            query.epoch_snapshot,
        )),
        required_assigned: clamped_assigned_requirement(configured_assigned, assigned_miners)
            as u64,
        signers,
    })
}

/// Selector candidates as `(stored address, recovered signer)`, in cache order.
fn selector_candidates(
    tx_header: &DataTransactionHeader,
    stored_proofs: Vec<CompactCachedIngressProof>,
    query: &IngressStatusQuery<'_>,
) -> eyre::Result<Vec<(IrysAddress, IrysAddress)>> {
    let non_expired =
        unexpired_ingress_proofs(stored_proofs, query.block_tree, query.db, query.config);
    let (deduped, _) = dedup_ingress_proofs_by_signer(non_expired);
    let min_anchor_height = min_ingress_proof_anchor_height(query.parent_height + 1, query.config);
    let anchor_valid =
        anchor_valid_ingress_proofs(deduped, query.block_tree, query.db, min_anchor_height)?;

    Ok(anchor_valid
        .into_iter()
        .filter_map(|cached| match cached.proof.pre_validate(&tx_header.data_root) {
            Ok(recovered) => Some((cached.address, recovered)),
            Err(e) => {
                debug!(
                    signer = %cached.address,
                    error = %e,
                    "ingress status: proof failed pre_validate; counted as observed but not eligible"
                );
                None
            }
        })
        .collect())
}

/// Height of the Publish block that promoted `tx_id` on the canonical branch
/// ending at `tip_hash`, or `None`.
///
/// Same branch-correct check the selector uses to skip an already promoted
/// candidate. The mempool and metadata `promoted_height` hint is branch-variant
/// and is not consulted. The walk starts at the tip, so a promotion that has
/// not migrated is visible.
pub fn canonical_promotion_height(
    tx_id: H256,
    tip_hash: H256,
    tip_height: u64,
    config: &Config,
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<Option<u64>> {
    Ok(resolve_promotion_heights_on_branch(
        &[tx_id],
        tip_hash,
        tip_height + 1,
        config,
        block_tree,
        db,
    )?
    .remove(&tx_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_tree_service::test_utils::build_genesis_tree_with_n_blocks;
    use irys_database::db::IrysDatabaseExt as _;
    use irys_database::tables::{IngressProofs, IrysBlockHeaders, MigratedBlockHashes};
    use irys_database::{
        DatabaseProvider, DatabaseProviderTestExt as _, insert_tx_header,
        set_data_tx_included_height,
    };
    use irys_domain::{EpochSnapshot, StakeEntry};
    use irys_testing_utils::IrysBlockHeaderTestExt as _;
    use irys_testing_utils::tempfile::TempDir;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::ingress::{CachedIngressProof, generate_ingress_proof};
    use irys_types::irys::IrysSigner;
    use irys_types::partition::PartitionAssignment;
    use irys_types::{
        CommitmentStatus, Config, ConsensusConfig, DataLedger, DataTransactionHeader,
        DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata, DataTransactionMetadata,
        H256, H256List, IrysBlockHeader, NodeConfig, U256,
    };
    use reth_db::transaction::DbTxMut as _;

    const TS: u64 = 1_700_000_000;

    struct Harness {
        _tmp: TempDir,
        db: DatabaseProvider,
        config: Config,
        block_tree: BlockTreeReadGuard,
        epoch: EpochSnapshot,
        parent_height: u64,
    }

    impl Harness {
        fn testing() -> eyre::Result<Self> {
            Self::open(Config::new_with_random_peer_id(NodeConfig::testing()), 0, 5)
        }

        fn open(config: Config, tree_tip: u64, parent_height: u64) -> eyre::Result<Self> {
            assert_eq!(
                config.consensus.num_chunks_in_partition, 10,
                "slot fixtures assume the testing partition size"
            );
            let tmp = TempDirBuilder::new().prefix("ingress-status-").build();
            let db = DatabaseProvider::for_testing(tmp.path(), &tmp.path().join("cache"))?;
            let (block_tree, _) = build_genesis_tree_with_n_blocks(tree_tip);
            // The builder marks genesis as the tip. Expiry reads that tip, so a
            // tall fixture has to move the tip or every anchor looks current.
            if tree_tip > 0 {
                let tip_hash = {
                    let tree = block_tree.read();
                    tree.get_hashes_for_height(tree_tip)
                        .and_then(|hashes| hashes.iter().copied().next())
                        .ok_or_else(|| eyre!("block tree is missing height {tree_tip}"))?
                };
                block_tree
                    .write()
                    .mark_tip(&tip_hash)
                    .map_err(|e| eyre!("mark canonical tip: {e:#}"))?;
            }
            Ok(Self {
                _tmp: tmp,
                db,
                config,
                block_tree,
                epoch: EpochSnapshot::default(),
                parent_height,
            })
        }

        fn anchor_at(&self, height: u64) -> H256 {
            // The canonical window is only `block_tree_depth` blocks. Height
            // index still holds every block the fixture inserted.
            let tree = self.block_tree.read();
            let hashes = tree
                .get_hashes_for_height(height)
                .unwrap_or_else(|| panic!("block tree has no block at height {height}"));
            let hash = hashes
                .iter()
                .copied()
                .next()
                .unwrap_or_else(|| panic!("height {height} has no block hash"));
            assert_eq!(hashes.len(), 1, "fixture has one block per height");
            hash
        }

        fn signer(&self) -> IrysSigner {
            IrysSigner::random_signer(&self.config.consensus)
        }

        fn stake(&mut self, address: IrysAddress) {
            self.epoch.commitment_state.stake_commitments.insert(
                address,
                StakeEntry {
                    id: H256::random(),
                    commitment_status: CommitmentStatus::Active,
                    signer: address,
                    amount: U256::from(1),
                    reward_address: address,
                },
            );
        }

        fn assign_slot(&mut self, address: IrysAddress, slot: usize) {
            let assignment = PartitionAssignment {
                partition_hash: H256::random(),
                miner_address: address,
                ledger_id: Some(u32::from(DataLedger::Submit)),
                slot_index: Some(slot),
            };
            self.epoch
                .partition_assignments
                .data_partitions
                .insert(assignment.partition_hash, assignment);
        }

        fn proof(
            &self,
            signer: &IrysSigner,
            data_root: H256,
            anchor: H256,
            marker: u8,
        ) -> eyre::Result<CompactCachedIngressProof> {
            let chunk = [marker; 32];
            let proof = generate_ingress_proof(
                signer,
                data_root,
                std::iter::once(Ok(chunk.as_slice())),
                self.config.consensus.chain_id,
                anchor,
            )?;
            Ok(CompactCachedIngressProof(CachedIngressProof {
                address: signer.address(),
                proof,
            }))
        }

        fn query(&self, secs: u64) -> IngressStatusQuery<'_> {
            IngressStatusQuery {
                parent_height: self.parent_height,
                requirement_timestamp: UnixTimestamp::from_secs(secs),
                block_tree: &self.block_tree,
                db: &self.db,
                config: &self.config,
                epoch_snapshot: &self.epoch,
            }
        }

        fn assess(
            &self,
            header: &DataTransactionHeader,
            proofs: &[CompactCachedIngressProof],
            secs: u64,
        ) -> eyre::Result<IngressProofAssessment> {
            assess_submit_ingress_proofs(header, proofs.to_vec(), &self.query(secs))
        }
    }

    fn config_with(total: u8, assigned: u8) -> Config {
        let node = NodeConfig::testing().with_consensus(|consensus| {
            consensus.hardforks.frontier.number_of_ingress_proofs_total = total;
            consensus
                .hardforks
                .frontier
                .number_of_ingress_proofs_from_assignees = assigned;
        });
        Config::new_with_random_peer_id(node)
    }

    fn data_header(tx_id: H256, data_root: H256) -> DataTransactionHeader {
        DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: tx_id,
                data_root,
                ledger_id: u32::from(DataLedger::Submit),
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        })
    }

    fn write_submit_range(
        db: &DatabaseProvider,
        header: &DataTransactionHeader,
        prev_total: u64,
        total: u64,
    ) -> eyre::Result<IrysBlockHeader> {
        let mut h0 = IrysBlockHeader::new_mock_header();
        h0.height = 0;
        h0.previous_block_hash = H256::zero();
        h0.cumulative_diff = U256::from(0);
        h0.data_ledgers[DataLedger::Submit as usize].total_chunks = prev_total;
        h0.data_ledgers[DataLedger::Submit as usize].tx_ids = H256List(vec![]);
        h0.test_sign();

        let mut h1 = IrysBlockHeader::new_mock_header();
        h1.height = 1;
        h1.previous_block_hash = h0.block_hash;
        h1.cumulative_diff = U256::from(1);
        h1.data_ledgers[DataLedger::Submit as usize].total_chunks = total;
        h1.data_ledgers[DataLedger::Submit as usize].tx_ids = H256List(vec![header.id]);
        h1.test_sign();

        let h0_hash = h0.block_hash;
        let h1_hash = h1.block_hash;
        let stored = h1.clone();
        db.update_eyre(|tx| {
            tx.put::<IrysBlockHeaders>(h0_hash, h0.into())?;
            tx.put::<IrysBlockHeaders>(h1_hash, h1.into())?;
            tx.put::<MigratedBlockHashes>(0, h0_hash)?;
            tx.put::<MigratedBlockHashes>(1, h1_hash)?;
            insert_tx_header(tx, header)?;
            set_data_tx_included_height(tx, &header.id, 1)?;
            Ok(())
        })?;
        Ok(stored)
    }

    fn persist_tree_block_as_canonical(harness: &Harness, height: u64) -> eyre::Result<()> {
        let header = {
            let hash = harness.anchor_at(height);
            harness
                .block_tree
                .read()
                .get_block(&hash)
                .cloned()
                .ok_or_else(|| eyre!("block tree is missing the header at height {height}"))?
        };
        let hash = header.block_hash;
        let stored_height = header.height;
        harness.db.update_eyre(|tx| {
            tx.put::<IrysBlockHeaders>(hash, header.into())?;
            tx.put::<MigratedBlockHashes>(stored_height, hash)?;
            Ok(())
        })?;
        Ok(())
    }

    fn signer_row(got: &IngressProofAssessment, address: IrysAddress) -> &IngressProofSigner {
        got.signers
            .iter()
            .find(|signer| signer.address == address)
            .unwrap_or_else(|| panic!("missing signer {address}"))
    }

    fn clear_submit_membership(db: &DatabaseProvider, block: &IrysBlockHeader) -> eyre::Result<()> {
        let mut rewritten = block.clone();
        // Same block hash, so the migrated-hash row still points here. Empty
        // Submit membership is the canonical "this tx is not in the block" answer.
        rewritten.data_ledgers[DataLedger::Submit as usize].tx_ids = H256List(vec![]);
        let hash = rewritten.block_hash;
        db.update_eyre(|tx| {
            tx.put::<IrysBlockHeaders>(hash, rewritten.into())?;
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn ingress_status_zero_proofs_is_confirmed_empty() -> eyre::Result<()> {
        let harness = Harness::testing()?;
        let header = data_header(H256::random(), H256::random());

        let empty = harness.assess(&header, &[], TS)?;
        assert_eq!(empty.observed(), 0);
        assert_eq!(empty.assigned(), 0);
        assert_eq!(empty.unassigned(), 0);
        assert_eq!(empty.eligible(), 0);
        assert_eq!(empty.required_total, 1);
        assert_eq!(empty.required_assigned, 0);
        assert_eq!(empty.remaining_total(), 1);
        assert_eq!(empty.remaining_assigned(), 0);
        assert!(empty.signers.is_empty());
        assert!(!empty.proof_requirements_met());

        write_submit_range(&harness.db, &header, 10, 25)?;
        let still_empty = harness.assess(&header, &[], TS)?;
        assert_eq!(still_empty, empty);
        Ok(())
    }

    /// From Delta the total requirement is clamped to the active stakes: with
    /// 2 stakes and a configured total of 6, two eligible proofs meet it.
    #[test]
    fn ingress_status_total_clamped_to_active_stakes_from_delta() -> eyre::Result<()> {
        let node = NodeConfig::testing().with_consensus(|consensus| {
            consensus.hardforks.frontier.number_of_ingress_proofs_total = 6;
            consensus.hardforks.delta = Some(irys_types::hardfork_config::Delta {
                activation_timestamp: UnixTimestamp::from_secs(0),
                initial_slots_per_new_ledger:
                    irys_types::hardfork_config::Delta::default_initial_slots_per_new_ledger(),
                capacity_chunk_reward_percent:
                    irys_types::hardfork_config::Delta::default_capacity_chunk_reward_percent(),
                ingress_proof_reward_pool_percent:
                    irys_types::hardfork_config::Delta::default_ingress_proof_reward_pool_percent(),
            });
        });
        let mut harness = Harness::open(Config::new_with_random_peer_id(node), 0, 5)?;
        let data_root = H256::random();
        let header = data_header(H256::random(), data_root);
        write_submit_range(&harness.db, &header, 10, 25)?;
        let anchor = harness.anchor_at(0);
        let mut proofs = Vec::new();
        for marker in 1_u8..=2 {
            let signer = harness.signer();
            harness.stake(signer.address());
            proofs.push(harness.proof(&signer, data_root, anchor, marker)?);
        }

        let got = harness.assess(&header, &proofs, TS)?;
        assert_eq!(got.required_total, 2, "6 clamps to the 2 active stakes");
        assert_eq!(got.eligible(), 2);
        assert!(got.proof_requirements_met());
        Ok(())
    }

    #[test]
    fn ingress_status_partial_eligible_proofs() -> eyre::Result<()> {
        let mut harness = Harness::open(config_with(6, 2), 0, 5)?;
        let data_root = H256::random();
        let header = data_header(H256::random(), data_root);
        write_submit_range(&harness.db, &header, 10, 25)?;
        let anchor = harness.anchor_at(0);

        let mut proofs = Vec::new();
        let mut assigned_addresses = Vec::new();
        for (marker, stake_signer) in [(1_u8, true), (2, false)] {
            let signer = harness.signer();
            if stake_signer {
                harness.stake(signer.address());
            }
            harness.assign_slot(signer.address(), 1);
            assigned_addresses.push(signer.address());
            proofs.push(harness.proof(&signer, data_root, anchor, marker)?);
        }
        let mut unassigned_staked = Vec::new();
        for marker in 3_u8..=4 {
            let signer = harness.signer();
            harness.stake(signer.address());
            unassigned_staked.push(signer.address());
            proofs.push(harness.proof(&signer, data_root, anchor, marker)?);
        }
        let unstaked = harness.signer();
        proofs.push(harness.proof(&unstaked, data_root, anchor, 5)?);

        let got = harness.assess(&header, &proofs, TS)?;
        assert_eq!(got.observed(), 5);
        assert_eq!(
            got.assigned(),
            2,
            "assigned and unassigned partition observed"
        );
        assert_eq!(got.unassigned(), 3);
        assert_eq!(got.eligible(), 4);
        assert_eq!(
            got.eligible_assigned(),
            2,
            "unstaked assigned proof still counts"
        );
        assert_eq!(
            got.eligible_unassigned(),
            2,
            "unstaked unassigned proof does not"
        );
        assert_eq!(got.required_total, 6);
        assert_eq!(got.required_assigned, 2);
        assert_eq!(got.remaining_total(), 2);
        assert_eq!(got.remaining_assigned(), 0);
        assert!(!got.proof_requirements_met());
        assert_eq!(
            got.signers
                .iter()
                .map(|signer| signer.address)
                .collect::<Vec<_>>(),
            {
                let mut addresses = assigned_addresses.clone();
                addresses.extend(unassigned_staked.iter().copied());
                addresses.push(unstaked.address());
                addresses
            },
            "miners follow first-seen cache order"
        );
        for address in &assigned_addresses {
            let signer = signer_row(&got, *address);
            assert!(signer.assigned && signer.eligible);
        }
        for address in &unassigned_staked {
            let signer = signer_row(&got, *address);
            assert!(!signer.assigned && signer.eligible);
        }
        let inactive = signer_row(&got, unstaked.address());
        assert!(!inactive.assigned && !inactive.eligible);
        let active = got
            .signers
            .iter()
            .filter(|signer| signer.eligible)
            .map(|signer| signer.address)
            .collect::<Vec<_>>();
        assert_eq!(active.len(), 4, "the four active proofs name their miners");
        assert_eq!(got.eligible(), active.len() as u64);
        Ok(())
    }

    #[test]
    fn ingress_status_sufficient_eligible_proofs() -> eyre::Result<()> {
        let mut harness = Harness::testing()?;
        let data_root = H256::random();
        let header = data_header(H256::random(), data_root);
        let anchor = harness.anchor_at(0);
        let signer = harness.signer();
        harness.stake(signer.address());
        let proof = harness.proof(&signer, data_root, anchor, 1)?;

        let exact = harness.assess(&header, std::slice::from_ref(&proof), TS)?;
        assert_eq!(exact.eligible(), 1);
        assert_eq!(exact.eligible_unassigned(), 1);
        assert_eq!(exact.remaining_total(), 0);
        assert_eq!(exact.remaining_assigned(), 0);
        assert!(exact.proof_requirements_met());

        let extra_signer = harness.signer();
        harness.stake(extra_signer.address());
        let extra = harness.proof(&extra_signer, data_root, anchor, 2)?;
        let surplus = harness.assess(&header, &[proof, extra], TS)?;
        assert_eq!(surplus.eligible(), 2);
        assert_eq!(
            surplus.remaining_total(),
            0,
            "the deficit saturates at zero"
        );
        assert!(surplus.proof_requirements_met());
        Ok(())
    }

    #[test]
    fn ingress_status_duplicate_signers_count_once() -> eyre::Result<()> {
        let mut harness = Harness::testing()?;
        let data_root = H256::random();
        let header = data_header(H256::random(), data_root);
        let anchor = harness.anchor_at(0);
        let signer = harness.signer();
        harness.stake(signer.address());
        let first = harness.proof(&signer, data_root, anchor, 1)?;
        let second = harness.proof(&signer, data_root, anchor, 2)?;
        let first_id = first.proof.id();

        let (kept, dropped) = crate::tx_selector::helpers::dedup_ingress_proofs_by_signer(vec![
            first.0.clone(),
            second.0.clone(),
        ]);
        assert_eq!(dropped, 1);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].proof.id(), first_id);

        let got = harness.assess(&header, &[first, second], TS)?;
        assert_eq!(got.observed(), 1);
        assert_eq!(got.eligible(), 1);
        assert_eq!(
            got.signers,
            vec![IngressProofSigner {
                address: signer.address(),
                assigned: false,
                eligible: true,
            }]
        );
        Ok(())
    }

    #[test]
    fn ingress_status_later_valid_proof_replaces_an_expired_duplicate() -> eyre::Result<()> {
        let mut harness = Harness::testing()?;
        let data_root = H256::random();
        let header = data_header(H256::random(), data_root);
        let signer = harness.signer();
        harness.stake(signer.address());
        let expired = harness.proof(&signer, data_root, H256::random(), 1)?;
        let valid = harness.proof(&signer, data_root, harness.anchor_at(0), 2)?;

        let got = harness.assess(&header, &[expired, valid], TS)?;
        assert_eq!(got.observed(), 1);
        assert_eq!(got.eligible(), 1, "expiry runs before signer dedup");
        Ok(())
    }

    #[test]
    fn ingress_status_dedup_keeps_the_invalid_anchor_over_a_later_valid_one() -> eyre::Result<()> {
        let mut harness = Harness::open(config_with(1, 0), 201, 201)?;
        let data_root = H256::random();
        let header = data_header(H256::random(), data_root);
        let signer = harness.signer();
        harness.stake(signer.address());
        // Height 1 is inside the expiry window (min 1) and outside the inclusion
        // window (min 2) at tip 201. The canonical window does not reach height 1,
        // so persist that header or inclusion treats the anchor as unknown.
        persist_tree_block_as_canonical(&harness, 1)?;
        let too_old_for_inclusion = harness.proof(&signer, data_root, harness.anchor_at(1), 1)?;
        let valid = harness.proof(&signer, data_root, harness.anchor_at(201), 2)?;

        let got = harness.assess(&header, &[too_old_for_inclusion, valid], TS)?;
        assert_eq!(got.observed(), 1);
        assert_eq!(got.eligible(), 0);
        assert_eq!(got.unassigned(), 1);
        Ok(())
    }

    #[test]
    fn ingress_status_unknown_anchor_is_observed_not_eligible() -> eyre::Result<()> {
        let mut harness = Harness::testing()?;
        let data_root = H256::random();
        let header = data_header(H256::random(), data_root);
        let signer = harness.signer();
        harness.stake(signer.address());
        let proof = harness.proof(&signer, data_root, H256::random(), 1)?;

        let got = harness.assess(&header, &[proof], TS)?;
        assert_eq!(got.observed(), 1);
        assert_eq!(got.eligible(), 0);
        assert_eq!(got.unassigned(), 1);
        assert_eq!(got.assigned(), 0);
        Ok(())
    }

    #[test]
    fn ingress_status_expired_assigned_signer_still_owes_the_assignee_proof() -> eyre::Result<()> {
        let mut harness = Harness::open(config_with(1, 2), 201, 201)?;
        let data_root = H256::random();
        let header = data_header(H256::random(), data_root);
        write_submit_range(&harness.db, &header, 10, 25)?;
        let signer = harness.signer();
        harness.stake(signer.address());
        harness.assign_slot(signer.address(), 1);
        // Genesis height is below the expiry minimum once the tip is 201.
        let expired = harness.proof(&signer, data_root, harness.anchor_at(0), 1)?;

        let got = harness.assess(&header, &[expired], TS)?;
        assert_eq!(got.observed(), 1);
        assert_eq!(
            got.assigned(),
            1,
            "the stored address still partitions observed"
        );
        assert_eq!(got.eligible(), 0);
        assert_eq!(got.eligible_assigned(), 0);
        assert_eq!(
            got.required_assigned, 1,
            "one miner holds the intersecting slot, so 2 clamps to 1 with or without a candidate"
        );
        assert_eq!(got.remaining_assigned(), 1);
        assert!(!got.proof_requirements_met());
        Ok(())
    }

    #[test]
    fn ingress_status_assigned_deficit_with_enough_total_proofs() -> eyre::Result<()> {
        let mut harness = Harness::open(config_with(6, 2), 0, 5)?;
        let data_root = H256::random();
        let header = data_header(H256::random(), data_root);
        write_submit_range(&harness.db, &header, 10, 25)?;
        let anchor = harness.anchor_at(0);

        let assigned_signer = harness.signer();
        harness.stake(assigned_signer.address());
        harness.assign_slot(assigned_signer.address(), 1);
        let other_miner = harness.signer();
        harness.assign_slot(other_miner.address(), 1);
        let mut proofs = vec![harness.proof(&assigned_signer, data_root, anchor, 1)?];
        for marker in 2_u8..=6 {
            let signer = harness.signer();
            harness.stake(signer.address());
            proofs.push(harness.proof(&signer, data_root, anchor, marker)?);
        }

        let got = harness.assess(&header, &proofs, TS)?;
        assert_eq!(got.observed(), 6);
        assert_eq!(got.eligible(), 6);
        assert_eq!(got.eligible_assigned(), 1);
        assert_eq!(got.eligible_unassigned(), 5);
        assert_eq!(got.required_total, 6);
        assert_eq!(
            got.required_assigned, 2,
            "two slot miners, so the gate is not clamped"
        );
        assert_eq!(got.remaining_total(), 0);
        assert_eq!(got.remaining_assigned(), 1);
        assert!(!got.proof_requirements_met());
        Ok(())
    }

    #[test]
    fn ingress_status_multi_slot_assignment_and_clamp() -> eyre::Result<()> {
        let mut harness = Harness::open(config_with(2, 2), 0, 5)?;
        let data_root = H256::random();
        let header = data_header(H256::random(), data_root);
        // [5, 24] intersects slots 0, 1, and 2 when a partition is 10 chunks.
        write_submit_range(&harness.db, &header, 5, 25)?;
        let anchor = harness.anchor_at(0);

        let low = harness.signer();
        harness.stake(low.address());
        harness.assign_slot(low.address(), 0);
        let high = harness.signer();
        harness.stake(high.address());
        harness.assign_slot(high.address(), 2);
        for _ in 0..2 {
            let extra = harness.signer();
            harness.assign_slot(extra.address(), 2);
        }
        let low_proof = harness.proof(&low, data_root, anchor, 1)?;
        let high_proof = harness.proof(&high, data_root, anchor, 2)?;

        let low_then_high =
            harness.assess(&header, &[low_proof.clone(), high_proof.clone()], TS)?;
        assert_eq!(
            low_then_high.assigned(),
            2,
            "both slots intersect the submit range"
        );
        assert_eq!(low_then_high.eligible_assigned(), 2);
        assert_eq!(
            low_then_high.required_assigned, 2,
            "4 unique miners hold intersecting slots, so the configured 2 is not clamped"
        );

        // The gate is a property of the range and the epoch, not of proof order.
        // `signers` keeps first-seen order, so compare the order-free fields.
        let high_then_low = harness.assess(&header, &[high_proof, low_proof], TS)?;
        assert_eq!(high_then_low.assigned(), 2);
        assert_eq!(high_then_low.eligible_assigned(), 2);
        assert_eq!(
            high_then_low.required_assigned, low_then_high.required_assigned,
            "proof order must not change the assignee gate"
        );
        Ok(())
    }

    #[test]
    fn ingress_status_no_canonical_range_clamps_assignees_to_zero() -> eyre::Result<()> {
        let mut harness = Harness::open(config_with(1, 2), 0, 5)?;
        let data_root = H256::random();
        let header = data_header(H256::random(), data_root);
        let signer = harness.signer();
        harness.stake(signer.address());
        let proof = harness.proof(&signer, data_root, harness.anchor_at(0), 1)?;

        let got = harness.assess(&header, &[proof], TS)?;
        assert_eq!(got.assigned(), 0);
        assert_eq!(got.unassigned(), 1);
        assert_eq!(got.eligible_unassigned(), 1);
        assert_eq!(got.required_assigned, 0);
        assert!(got.proof_requirements_met());
        Ok(())
    }

    #[test]
    fn ingress_status_reorg_drops_assignment() -> eyre::Result<()> {
        let mut harness = Harness::open(config_with(1, 2), 0, 5)?;
        let data_root = H256::random();
        let header = data_header(H256::random(), data_root);
        let inclusion = write_submit_range(&harness.db, &header, 10, 25)?;
        let signer = harness.signer();
        harness.stake(signer.address());
        harness.assign_slot(signer.address(), 1);
        let proof = harness.proof(&signer, data_root, harness.anchor_at(0), 1)?;

        let assigned = harness.assess(&header, std::slice::from_ref(&proof), TS)?;
        assert_eq!(assigned.assigned(), 1);
        assert_eq!(assigned.eligible_assigned(), 1);
        assert_eq!(assigned.required_assigned, 1);

        clear_submit_membership(&harness.db, &inclusion)?;
        let unassigned = harness.assess(&header, &[proof], TS)?;
        assert_eq!(unassigned.assigned(), 0);
        assert_eq!(unassigned.unassigned(), 1);
        assert_eq!(unassigned.eligible_unassigned(), 1);
        assert_eq!(unassigned.required_assigned, 0);
        assert!(unassigned.proof_requirements_met());
        Ok(())
    }

    #[test]
    fn ingress_status_corrupt_submit_range_is_an_error() -> eyre::Result<()> {
        let harness = Harness::testing()?;
        let data_root = H256::random();
        let header = data_header(H256::random(), data_root);
        write_submit_range(&harness.db, &header, 10, 5)?;

        let empty = harness.assess(&header, &[], TS);
        let empty_err = empty.expect_err("corrupt metadata is not a zero count");
        assert!(
            empty_err.to_string().contains("total_chunks"),
            "unexpected error: {empty_err:#}"
        );

        let signer = IrysSigner::random_signer(&ConsensusConfig::testing());
        let proof = harness.proof(&signer, data_root, harness.anchor_at(0), 1)?;
        let with_proof = harness.assess(&header, &[proof], TS);
        let with_proof_err = with_proof.expect_err("a stored proof does not hide corruption");
        assert!(with_proof_err.to_string().contains("total_chunks"));
        Ok(())
    }

    #[test]
    fn ingress_status_pre_validate_failure_is_observed_not_fatal() -> eyre::Result<()> {
        let mut harness = Harness::testing()?;
        let data_root = H256::random();
        let header = data_header(H256::random(), data_root);
        let anchor = harness.anchor_at(0);
        let good_signer = harness.signer();
        harness.stake(good_signer.address());
        let good = harness.proof(&good_signer, data_root, anchor, 1)?;
        let bad_signer = harness.signer();
        harness.stake(bad_signer.address());
        let bad = harness.proof(&bad_signer, H256::random(), anchor, 2)?;

        let got = harness.assess(&header, &[good, bad], TS)?;
        assert_eq!(got.observed(), 2);
        assert_eq!(got.eligible(), 1);
        assert_eq!(got.unassigned(), 2);
        assert_eq!(
            got.signers,
            vec![
                IngressProofSigner {
                    address: good_signer.address(),
                    assigned: false,
                    eligible: true,
                },
                IngressProofSigner {
                    address: bad_signer.address(),
                    assigned: false,
                    eligible: false,
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn ingress_status_load_reads_only_the_data_root() -> eyre::Result<()> {
        let mut harness = Harness::testing()?;
        let data_root = H256::random();
        let header = data_header(H256::random(), data_root);
        let anchor = harness.anchor_at(0);
        let signer = harness.signer();
        harness.stake(signer.address());
        let proof = harness.proof(&signer, data_root, anchor, 1)?;
        let other_root = H256::random();
        let other_signer = harness.signer();
        let other = harness.proof(&other_signer, other_root, anchor, 2)?;

        harness.db.update_cache_eyre(|tx| {
            tx.put::<IngressProofs>(data_root, proof.clone())?;
            tx.put::<IngressProofs>(other_root, other)?;
            Ok(())
        })?;

        let loaded = load_submit_ingress_assessment(&header, &harness.query(TS))?;
        let in_memory = harness.assess(&header, std::slice::from_ref(&proof), TS)?;
        assert_eq!(loaded, in_memory);
        assert_eq!(loaded.observed(), 1);
        Ok(())
    }
}
