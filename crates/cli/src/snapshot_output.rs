use irys_chain::utils::load_config;
use irys_types::{Config, H256, NodeConfig};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use tracing::info;

use crate::db_utils::load_block_commitments;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum PartitionAssignmentKindOutput {
    Capacity,
    Data { ledger_id: u32, slot_index: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PartitionAssignmentOutput {
    pub(crate) partition_hash: H256,
    pub(crate) kind: PartitionAssignmentKindOutput,
}

impl PartitionAssignmentOutput {
    fn detail_string(&self) -> String {
        match self.kind {
            PartitionAssignmentKindOutput::Capacity => {
                format!("[capacity] {}", self.partition_hash)
            }
            PartitionAssignmentKindOutput::Data {
                ledger_id,
                slot_index,
            } => format!(
                "[data L{ledger_id} S{slot_index}] {}   (ledger_id={ledger_id}, slot={slot_index})",
                self.partition_hash
            ),
        }
    }
}

impl fmt::Display for PartitionAssignmentOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail_string())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SnapshotPartitionState {
    pub(crate) epoch_block: irys_types::IrysBlockHeader,
    pub(crate) total_commitments: usize,
    pub(crate) stake_count: usize,
    pub(crate) pledge_count: usize,
    pub(crate) by_miner: BTreeMap<irys_types::IrysAddress, Vec<PartitionAssignmentOutput>>,
}

impl SnapshotPartitionState {
    fn from_snapshot(
        snapshot: &irys_domain::EpochSnapshot,
        commitments: &[irys_types::CommitmentTransaction],
    ) -> Self {
        use irys_types::CommitmentTypeV2;

        let mut by_miner: BTreeMap<irys_types::IrysAddress, Vec<PartitionAssignmentOutput>> =
            BTreeMap::new();

        for (&hash, assignment) in &snapshot.partition_assignments.capacity_partitions {
            by_miner
                .entry(assignment.miner_address)
                .or_default()
                .push(PartitionAssignmentOutput {
                    partition_hash: hash,
                    kind: PartitionAssignmentKindOutput::Capacity,
                });
        }

        for (&hash, assignment) in &snapshot.partition_assignments.data_partitions {
            by_miner
                .entry(assignment.miner_address)
                .or_default()
                .push(PartitionAssignmentOutput {
                    partition_hash: hash,
                    kind: PartitionAssignmentKindOutput::Data {
                        ledger_id: assignment.ledger_id.unwrap_or(0),
                        slot_index: assignment.slot_index.unwrap_or(0),
                    },
                });
        }

        for partitions in by_miner.values_mut() {
            // Sort: data partitions first (by ledger/slot), then capacity, then by hash.
            let sort_key = |p: &PartitionAssignmentOutput| match p.kind {
                PartitionAssignmentKindOutput::Data {
                    ledger_id,
                    slot_index,
                } => (0_u8, ledger_id, slot_index),
                PartitionAssignmentKindOutput::Capacity => (1_u8, 0, 0),
            };
            partitions.sort_by(|a, b| {
                sort_key(a)
                    .cmp(&sort_key(b))
                    .then_with(|| a.partition_hash.cmp(&b.partition_hash))
            });
        }

        let stake_count = commitments
            .iter()
            .filter(|c| matches!(c.commitment_type(), CommitmentTypeV2::Stake))
            .count();
        let pledge_count = commitments
            .iter()
            .filter(|c| matches!(c.commitment_type(), CommitmentTypeV2::Pledge { .. }))
            .count();

        Self {
            epoch_block: snapshot.epoch_block.clone(),
            total_commitments: commitments.len(),
            stake_count,
            pledge_count,
            by_miner,
        }
    }

    fn total_partitions(&self) -> usize {
        self.by_miner.values().map(Vec::len).sum()
    }

    fn capacity_count(&self) -> usize {
        self.by_miner
            .values()
            .flatten()
            .filter(|partition| matches!(partition.kind, PartitionAssignmentKindOutput::Capacity))
            .count()
    }

    fn data_count(&self) -> usize {
        self.total_partitions() - self.capacity_count()
    }

    fn all_partition_hashes(&self) -> BTreeSet<H256> {
        self.by_miner
            .values()
            .flat_map(|assignments| assignments.iter().map(|a| a.partition_hash))
            .collect()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SnapshotInspectionOutput {
    block_title: &'static str,
    commitment_label: &'static str,
    state: SnapshotPartitionState,
}

impl SnapshotInspectionOutput {
    fn from_snapshot(
        block_title: &'static str,
        commitment_label: &'static str,
        snapshot: &irys_domain::EpochSnapshot,
        commitments: &[irys_types::CommitmentTransaction],
    ) -> Self {
        Self {
            block_title,
            commitment_label,
            state: SnapshotPartitionState::from_snapshot(snapshot, commitments),
        }
    }
}

impl fmt::Display for SnapshotInspectionOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}", self.block_title)?;
        writeln!(f, "  Hash:        {}", self.state.epoch_block.block_hash)?;
        writeln!(f, "  Timestamp:   {}", self.state.epoch_block.timestamp)?;
        writeln!(f, "  Difficulty:  {}", self.state.epoch_block.diff)?;
        writeln!(f)?;
        writeln!(
            f,
            "{}: {} total ({} stakes, {} pledges)",
            self.commitment_label,
            self.state.total_commitments,
            self.state.stake_count,
            self.state.pledge_count
        )?;
        writeln!(f)?;

        writeln!(f, "Partition Assignments:")?;
        for (miner, partitions) in &self.state.by_miner {
            writeln!(f, "  Miner {miner}")?;
            for partition in partitions {
                writeln!(f, "    {partition}")?;
            }
            writeln!(f)?;
        }

        writeln!(f, "Summary:")?;
        writeln!(f, "  Miners:              {}", self.state.by_miner.len())?;
        writeln!(
            f,
            "  Total partitions:    {}",
            self.state.total_partitions()
        )?;
        writeln!(f, "  Capacity partitions: {}", self.state.capacity_count())?;
        writeln!(f, "  Data partitions:     {}", self.state.data_count())?;

        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MinerPartitionDiff {
    unchanged: Vec<PartitionAssignmentOutput>,
    role_changes: Vec<(PartitionAssignmentOutput, PartitionAssignmentOutput)>,
    wipe: Vec<PartitionAssignmentOutput>,
    add: Vec<PartitionAssignmentOutput>,
}

impl MinerPartitionDiff {
    fn has_changes(&self) -> bool {
        !(self.role_changes.is_empty() && self.wipe.is_empty() && self.add.is_empty())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SnapshotComparisonOutput {
    current_label: &'static str,
    target_label: &'static str,
    current: SnapshotPartitionState,
    target: SnapshotPartitionState,
    per_miner: BTreeMap<irys_types::IrysAddress, MinerPartitionDiff>,
}

impl SnapshotComparisonOutput {
    pub(crate) fn from_states(
        current_label: &'static str,
        current: SnapshotPartitionState,
        target_label: &'static str,
        target: SnapshotPartitionState,
    ) -> Self {
        let miners = current
            .by_miner
            .keys()
            .chain(target.by_miner.keys())
            .copied()
            .collect::<BTreeSet<_>>();

        let mut per_miner = BTreeMap::new();

        for miner in miners {
            let current_assignments = current
                .by_miner
                .get(&miner)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|assignment| (assignment.partition_hash, assignment))
                .collect::<BTreeMap<_, _>>();
            let target_assignments = target
                .by_miner
                .get(&miner)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|assignment| (assignment.partition_hash, assignment))
                .collect::<BTreeMap<_, _>>();

            let hashes = current_assignments
                .keys()
                .chain(target_assignments.keys())
                .copied()
                .collect::<BTreeSet<_>>();

            let mut diff = MinerPartitionDiff::default();

            for hash in hashes {
                match (
                    current_assignments.get(&hash),
                    target_assignments.get(&hash),
                ) {
                    (Some(current_assignment), Some(target_assignment)) => {
                        if current_assignment == target_assignment {
                            diff.unchanged.push(*current_assignment);
                        } else {
                            diff.role_changes
                                .push((*current_assignment, *target_assignment));
                        }
                    }
                    (Some(current_assignment), None) => diff.wipe.push(*current_assignment),
                    (None, Some(target_assignment)) => diff.add.push(*target_assignment),
                    (None, None) => {}
                }
            }

            per_miner.insert(miner, diff);
        }

        Self {
            current_label,
            target_label,
            current,
            target,
            per_miner,
        }
    }

    pub(crate) fn affected_miners(&self) -> usize {
        self.per_miner
            .values()
            .filter(|diff| diff.has_changes())
            .count()
    }

    pub(crate) fn unchanged_count(&self) -> usize {
        self.per_miner
            .values()
            .map(|diff| diff.unchanged.len())
            .sum()
    }

    pub(crate) fn role_change_count(&self) -> usize {
        self.per_miner
            .values()
            .map(|diff| diff.role_changes.len())
            .sum()
    }

    pub(crate) fn wipe_count(&self) -> usize {
        self.per_miner.values().map(|diff| diff.wipe.len()).sum()
    }

    pub(crate) fn add_count(&self) -> usize {
        self.per_miner.values().map(|diff| diff.add.len()).sum()
    }

    pub(crate) fn retained_partition_hashes(&self) -> BTreeSet<H256> {
        let current = self.current.all_partition_hashes();
        let target = self.target.all_partition_hashes();
        current.intersection(&target).copied().collect()
    }
}

impl fmt::Display for SnapshotComparisonOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}", self.current_label)?;
        writeln!(f, "  Hash:        {}", self.current.epoch_block.block_hash)?;
        writeln!(f, "  Timestamp:   {}", self.current.epoch_block.timestamp)?;
        writeln!(f, "  Difficulty:  {}", self.current.epoch_block.diff)?;
        writeln!(
            f,
            "  Commitments: {} total ({} stakes, {} pledges)",
            self.current.total_commitments, self.current.stake_count, self.current.pledge_count
        )?;
        writeln!(f, "  Miners:      {}", self.current.by_miner.len())?;
        writeln!(f, "  Partitions:  {}", self.current.total_partitions())?;
        writeln!(f)?;

        writeln!(f, "{}", self.target_label)?;
        writeln!(f, "  Hash:        {}", self.target.epoch_block.block_hash)?;
        writeln!(f, "  Timestamp:   {}", self.target.epoch_block.timestamp)?;
        writeln!(f, "  Difficulty:  {}", self.target.epoch_block.diff)?;
        writeln!(
            f,
            "  Commitments: {} total ({} stakes, {} pledges)",
            self.target.total_commitments, self.target.stake_count, self.target.pledge_count
        )?;
        writeln!(f, "  Miners:      {}", self.target.by_miner.len())?;
        writeln!(f, "  Partitions:  {}", self.target.total_partitions())?;
        writeln!(f)?;

        writeln!(f, "Reset Diff:")?;
        writeln!(f, "  Affected miners:      {}", self.affected_miners())?;
        writeln!(f, "  Partitions to wipe:   {}", self.wipe_count())?;
        writeln!(f, "  Partitions to add:    {}", self.add_count())?;
        writeln!(f, "  Role changes:         {}", self.role_change_count())?;
        writeln!(f, "  Unchanged partitions: {}", self.unchanged_count())?;
        writeln!(f)?;

        if self.affected_miners() == 0 {
            writeln!(
                f,
                "No reset actions required. Current network assignments already match the target genesis."
            )?;
            return Ok(());
        }

        writeln!(f, "Per-Miner Actions:")?;
        for (miner, diff) in &self.per_miner {
            if !diff.has_changes() {
                continue;
            }

            writeln!(f, "  Miner {miner}")?;

            if !diff.wipe.is_empty() {
                writeln!(f, "    Wipe {} partition(s):", diff.wipe.len())?;
                for assignment in &diff.wipe {
                    writeln!(f, "      {assignment}")?;
                }
            }

            if !diff.add.is_empty() {
                writeln!(f, "    Add {} partition(s):", diff.add.len())?;
                for assignment in &diff.add {
                    writeln!(f, "      {assignment}")?;
                }
            }

            if !diff.role_changes.is_empty() {
                writeln!(
                    f,
                    "    Role change {} partition(s):",
                    diff.role_changes.len()
                )?;
                for (current_assignment, target_assignment) in &diff.role_changes {
                    writeln!(f, "      {current_assignment} -> {target_assignment}")?;
                }
            }

            if !diff.unchanged.is_empty() {
                writeln!(
                    f,
                    "    Keep {} unchanged partition(s):",
                    diff.unchanged.len()
                )?;
                for assignment in &diff.unchanged {
                    writeln!(f, "      {assignment}")?;
                }
            }

            writeln!(f)?;
        }

        Ok(())
    }
}

fn count_pledge_commitments(commitments: &[irys_types::CommitmentTransaction]) -> usize {
    commitments
        .iter()
        .filter(|c| {
            matches!(
                c.commitment_type(),
                irys_types::CommitmentTypeV2::Pledge { .. }
            )
        })
        .count()
}

/// Create placeholder submodule paths for partition assignment computation.
/// These paths are never accessed on disk — `EpochSnapshot::new` only uses
/// the path count and `is_using_hardcoded_paths` flag to derive partition hashes.
fn hardcoded_submodules(prefix: &str, count: usize) -> irys_config::StorageSubmodulesConfig {
    irys_config::StorageSubmodulesConfig {
        is_using_hardcoded_paths: true,
        submodule_paths: (0..count)
            .map(|i| PathBuf::from(format!("/tmp/{prefix}-{i}")))
            .collect(),
    }
}

pub(crate) fn cli_snapshot_output(
    block_title: &'static str,
    commitment_label: &'static str,
    snapshot: &irys_domain::EpochSnapshot,
    commitments: &[irys_types::CommitmentTransaction],
) -> SnapshotInspectionOutput {
    SnapshotInspectionOutput::from_snapshot(block_title, commitment_label, snapshot, commitments)
}

pub(crate) fn cli_compare_output(
    current_label: &'static str,
    current: &irys_domain::EpochSnapshot,
    current_commitments: &[irys_types::CommitmentTransaction],
    target_label: &'static str,
    target: &irys_domain::EpochSnapshot,
    target_commitments: &[irys_types::CommitmentTransaction],
) -> SnapshotComparisonOutput {
    SnapshotComparisonOutput::from_states(
        current_label,
        SnapshotPartitionState::from_snapshot(current, current_commitments),
        target_label,
        SnapshotPartitionState::from_snapshot(target, target_commitments),
    )
}

pub(crate) fn cli_config() -> eyre::Result<Config> {
    let node_config: NodeConfig = load_config()?;
    Ok(Config::new_with_random_peer_id(node_config))
}

pub(crate) fn snapshot_from_genesis_dir(
    genesis_dir: &PathBuf,
    config: &Config,
    submodule_prefix: &str,
) -> eyre::Result<(
    irys_domain::EpochSnapshot,
    Vec<irys_types::CommitmentTransaction>,
)> {
    use irys_chain::genesis_utilities::{
        load_genesis_block_from_disk, load_genesis_commitments_from_disk,
    };
    use irys_domain::EpochSnapshot;

    let genesis_block = load_genesis_block_from_disk(genesis_dir)
        .map_err(|e| eyre::eyre!("loading genesis block from {:?}: {e}", genesis_dir))?;

    let commitments = load_genesis_commitments_from_disk(genesis_dir)
        .map_err(|e| eyre::eyre!("loading genesis commitments from {:?}: {e}", genesis_dir))?;

    let submodules = hardcoded_submodules(submodule_prefix, count_pledge_commitments(&commitments));
    let snapshot = EpochSnapshot::new(
        &submodules,
        (*genesis_block).clone(),
        commitments.clone(),
        config,
    );

    Ok((snapshot, commitments))
}

pub(crate) fn replay_current_network_snapshot<T: irys_database::reth_db::transaction::DbTx>(
    read_tx: &T,
    config: &Config,
    submodule_prefix: &str,
) -> eyre::Result<(
    irys_domain::EpochSnapshot,
    Vec<irys_types::CommitmentTransaction>,
)> {
    use irys_database::tables::{IrysCommitments, MigratedBlockHashes};
    use irys_database::{block_header_by_hash, block_index_latest_height, walk_all};
    use irys_domain::EpochSnapshot;

    // Count total pledge commitments upfront for submodule sizing. This uses DB
    // key order (fine — we only need the count, not the ordering).
    let total_pledge_count = {
        let all: Vec<_> = walk_all::<IrysCommitments, _>(read_tx)?;
        all.iter()
            .filter(|(_, c)| {
                matches!(
                    c.0.commitment_type(),
                    irys_types::CommitmentTypeV2::Pledge { .. }
                )
            })
            .count()
    };

    let latest_height =
        block_index_latest_height(read_tx)?.ok_or_else(|| eyre::eyre!("Block index is empty"))?;
    let epoch_len = config.consensus.epoch.num_blocks_in_epoch;
    let num_epoch_blocks = (latest_height / epoch_len) + 1;

    info!(
        "Replaying {num_epoch_blocks} epoch blocks (epoch_len={epoch_len}, chain_height={latest_height})"
    );

    let genesis_hash = read_tx
        .get::<MigratedBlockHashes>(0)?
        .ok_or_else(|| eyre::eyre!("No genesis block found at height 0"))?;
    let genesis_block = block_header_by_hash(read_tx, &genesis_hash, false)?
        .ok_or_else(|| eyre::eyre!("Genesis block header not found for {genesis_hash}"))?;

    let genesis_commitments = load_block_commitments(read_tx, &genesis_block)?;
    info!(
        "  Epoch 0 (height 0): {} commitments",
        genesis_commitments.len()
    );

    // Accumulate commitments in ledger order (the order they appear in each
    // epoch block's system_ledgers) rather than DB table key order. This
    // ensures dump-commitments → build-genesis --commitments round-trips
    // produce deterministic genesis hashes matching the original ledger order.
    let mut ordered_commitments = genesis_commitments.clone();

    let submodules = hardcoded_submodules(submodule_prefix, total_pledge_count);
    let mut snapshot = EpochSnapshot::new(
        &submodules,
        genesis_block.clone(),
        genesis_commitments,
        config,
    );

    let mut prev_epoch_block = genesis_block;
    for i in 1..num_epoch_blocks {
        let height = i * epoch_len;
        let block_hash = read_tx
            .get::<MigratedBlockHashes>(height)?
            .ok_or_else(|| eyre::eyre!("No block found at epoch height {height}"))?;
        let block = block_header_by_hash(read_tx, &block_hash, false)?.ok_or_else(|| {
            eyre::eyre!("Block header not found for {block_hash} at height {height}")
        })?;

        let epoch_commitments = load_block_commitments(read_tx, &block)?;
        info!(
            "  Epoch {i} (height {height}): {} commitments",
            epoch_commitments.len()
        );
        ordered_commitments.extend(epoch_commitments.iter().cloned());

        snapshot
            .perform_epoch_tasks(&Some(prev_epoch_block), &block, epoch_commitments)
            .map_err(|e| eyre::eyre!("Failed epoch tasks at height {height}: {e}"))?;

        prev_epoch_block = block;
    }

    info!("Found {} commitments total", ordered_commitments.len());

    Ok((snapshot, ordered_commitments))
}
