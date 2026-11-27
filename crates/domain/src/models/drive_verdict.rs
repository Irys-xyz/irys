//! Pure verdict on what each configured drive should hold.
//!
//! The verdict depends only on what is recorded on each drive and on the
//! canonical epoch assignments, never on drive order, except which duplicate
//! holder wins and which free drive receives a new placement. So appending,
//! removing or reordering drives cannot change which partition a drive keeps.

use std::collections::HashMap;
use std::path::PathBuf;

use irys_types::{
    Config, DataLedger, H256, IrysAddress, PartitionChunkOffset, partition::PartitionAssignment,
    partition_chunk_offset_ie,
};

use super::storage_module::{PACKING_PARAMS_FILE_NAME, PackingParams, StorageModuleInfo};

/// What was found at a drive's `packing_params.toml`. A file that cannot be
/// read or parsed is not a drive state: it stops the node (see
/// [`drive_verdict_inputs_from_disk`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveFile {
    Missing,
    Recorded(PackingParams),
}

/// Who and what chain this node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainIdentity {
    pub chain_id: u64,
    pub genesis_hash: H256,
    pub miner: IrysAddress,
}

impl ChainIdentity {
    /// This node's identity on the chain whose genesis block is `genesis_hash`.
    pub fn new(config: &Config, genesis_hash: H256) -> Self {
        Self {
            chain_id: config.consensus.chain_id,
            genesis_hash,
            miner: config.node_config.miner_address(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct VerdictInputs<'a> {
    /// Configured order; index = module id.
    pub drives: &'a [(PathBuf, DriveFile)],
    /// This miner's assignments, from the canonical epoch snapshot.
    pub assignments: &'a [PartitionAssignment],
    pub epoch_height: u64,
    /// Node is in sync with the network head (false at startup).
    pub in_sync: bool,
    pub identity: &'a ChainIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveVerdict {
    Ours,
    Empty,
    Released,
    Wait(WaitReason),
}

/// Why a drive is left idle and untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitReason {
    /// Another drive (lower id) holds the same assigned partition.
    DuplicateHolder(H256),
    /// The drive's file records an assignment installed after the epoch it
    /// is judged on (for example a reorg that orphaned the epoch the drive
    /// applied). Only a later epoch can change that.
    RecordedNewerThanEpoch,
    /// The drive is unassigned and its v2 file records no trusted assigned
    /// height, so a release cannot be proven. An in-sync epoch that assigns
    /// it to this miner stamps one.
    NoTrustedHeight,
    /// The drive is unassigned and its file is v1 or names another chain.
    NotProvablyReleased,
}

impl std::fmt::Display for WaitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateHolder(hash) => write!(f, "duplicate holder of {hash}"),
            Self::RecordedNewerThanEpoch => f.write_str("recorded state is newer than the epoch"),
            Self::NoTrustedHeight => f.write_str("no trusted assigned height"),
            Self::NotProvablyReleased => {
                f.write_str("unassigned, but not provably released on this chain")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveAction {
    /// Open (or keep) with no assignment; file untouched.
    Idle,
    /// Hold this partition (same hash: keep bytes; Empty/Released: wipe and bind).
    Assign(PartitionAssignment),
    /// Released and receiving nothing: wipe, file becomes v2 with no partition.
    Clear,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveDecision {
    pub id: usize,
    pub path: PathBuf,
    pub verdict: DriveVerdict,
    pub action: DriveAction,
    /// Rewrite the drive's file as v2 at this epoch (ours, in sync).
    pub stamp_v2: bool,
}

impl DriveVerdict {
    /// Lower-case name for logs.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Ours => "ours",
            Self::Empty => "empty",
            Self::Released => "released",
            Self::Wait(_) => "wait",
        }
    }

    /// Why a drive waits; empty otherwise.
    pub fn reason(&self) -> String {
        match self {
            Self::Wait(why) => why.to_string(),
            Self::Ours | Self::Empty | Self::Released => String::new(),
        }
    }
}

impl DriveAction {
    /// Lower-case name for logs.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Assign(_) => "assign",
            Self::Clear => "clear",
        }
    }
}

/// An error naming the first drive whose recorded file is foreign (newer
/// version, other chain id, other packing address). The node does not start
/// with such a drive, and no drive is skipped. Open runs the same check, and
/// every runtime store records this node's miner and chain, so [`decide`]
/// never sees a foreign file.
pub fn refuse_foreign_drives(
    drives: &[(PathBuf, DriveFile)],
    identity: &ChainIdentity,
) -> eyre::Result<()> {
    for (path, file) in drives {
        if let DriveFile::Recorded(p) = file
            && let Some(why) = p.foreign_reason(identity.chain_id, identity.miner)
        {
            eyre::bail!(
                "drive {} is not this node's: {why} (packing params at {})",
                path.display(),
                path.join(PACKING_PARAMS_FILE_NAME).display()
            );
        }
    }
    Ok(())
}

/// Ledger priority for placement: known ledgers in id order (Publish, Submit,
/// OneYear, ThirtyDay), then other ids, then capacity.
fn placement_key(a: &PartitionAssignment) -> (u32, H256) {
    let rank = match a.ledger_id {
        Some(l) if l == DataLedger::Publish => 0,
        Some(l) if l == DataLedger::Submit => 1,
        Some(l) if l == DataLedger::OneYear => 2,
        Some(l) if l == DataLedger::ThirtyDay => 3,
        Some(l) => 4_u32.saturating_add(l),
        None => u32::MAX,
    };
    (rank, a.partition_hash)
}

/// First holder (in id order) of each assigned hash.
///
/// A drive that could take the assignment at this epoch is preferred over one
/// that must wait because it leaves its recorded ledger slot with a record
/// newer than the epoch: otherwise the first drive in config order would
/// wait and leave the partition unserved, and reversing the order would serve
/// it. Among the drives that can take it, one whose recorded slot already
/// matches the assignment is preferred: another holder would have to leave
/// its slot and repack its data first. Config order only breaks ties.
fn first_holders(inputs: &VerdictInputs<'_>) -> HashMap<H256, usize> {
    let by_hash: HashMap<H256, PartitionAssignment> = inputs
        .assignments
        .iter()
        .map(|a| (a.partition_hash, *a))
        .collect();
    let mut exact = HashMap::new();
    let mut eligible = HashMap::new();
    let mut first = HashMap::new();
    for (i, (_, f)) in inputs.drives.iter().enumerate() {
        if let DriveFile::Recorded(p) = f
            && let Some(h) = p.partition_hash
            && let Some(new) = by_hash.get(&h)
        {
            first.entry(h).or_insert(i);
            if !(p.leaves_ledger_for(new) && recorded_newer(inputs, p)) {
                eligible.entry(h).or_insert(i);
                if (p.ledger, p.slot) == (new.ledger_id, new.slot_index) {
                    exact.entry(h).or_insert(i);
                }
            }
        }
    }
    first.extend(eligible);
    first.extend(exact);
    first
}

/// The file records an assignment installed after the epoch judged here.
///
/// A drive that records `new`'s partition on another ledger slot with such
/// a record waits for a later epoch. Only leaving a ledger slot waits. A
/// drive that records capacity and is given a ledger slot by an older epoch
/// takes it: a reorg made that epoch canonical again, the slot is this
/// drive's on that chain, and gaining a slot destroys nothing (data is
/// written over entropy only where data goes). Leaving would relabel and
/// repack the slot's data, so it is held back. A height recorded on another
/// genesis says nothing about this chain's epochs and never makes a drive
/// wait.
fn recorded_newer(inputs: &VerdictInputs<'_>, p: &PackingParams) -> bool {
    p.genesis_hash == Some(inputs.identity.genesis_hash)
        && p.trusted_assigned_height()
            .is_some_and(|h| h > inputs.epoch_height)
}

/// Every drive's decision, in id order, and the assignments no drive holds
/// or receives, in placement order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub decisions: Vec<DriveDecision>,
    pub unplaced: Vec<PartitionAssignment>,
}

/// Decide, for every drive, what it keeps or receives. Exactly one decision per
/// drive, in id order. Pure: no I/O, clocks or global state. Foreign drives
/// are refused before this runs (see [`refuse_foreign_drives`]).
pub fn decide(inputs: &VerdictInputs<'_>) -> Verdict {
    let holders = first_holders(inputs);
    let by_hash: HashMap<H256, PartitionAssignment> = inputs
        .assignments
        .iter()
        .map(|a| (a.partition_hash, *a))
        .collect();

    let mut decisions: Vec<DriveDecision> = inputs
        .drives
        .iter()
        .enumerate()
        .map(|(id, (path, file))| {
            let c = match file {
                DriveFile::Missing => Classified::new(DriveVerdict::Empty, DriveAction::Idle),
                DriveFile::Recorded(p) => classify(inputs, id, p, &holders, &by_hash),
            };
            DriveDecision {
                id,
                path: path.clone(),
                verdict: c.verdict,
                action: c.action,
                stamp_v2: c.stamp_v2,
            }
        })
        .collect();

    // Assignments no drive holds, in placement order: empty drives first,
    // then released ones, receive them. What is left is unplaced.
    let mut queue: Vec<_> = inputs
        .assignments
        .iter()
        .filter(|a| !holders.contains_key(&a.partition_hash))
        .copied()
        .collect();
    queue.sort_by_key(placement_key);
    let mut queue = queue.into_iter();
    for want in [DriveVerdict::Empty, DriveVerdict::Released] {
        for d in decisions.iter_mut().filter(|d| d.verdict == want) {
            if let Some(a) = queue.next() {
                d.action = DriveAction::Assign(a);
            }
        }
    }
    Verdict {
        decisions,
        unplaced: queue.collect(),
    }
}

/// One drive's verdict, action and stamp flags.
struct Classified {
    verdict: DriveVerdict,
    action: DriveAction,
    stamp_v2: bool,
}

impl Classified {
    fn new(verdict: DriveVerdict, action: DriveAction) -> Self {
        Self {
            verdict,
            action,
            stamp_v2: false,
        }
    }

    fn wait(reason: WaitReason) -> Self {
        Self::new(DriveVerdict::Wait(reason), DriveAction::Idle)
    }
}

fn classify(
    inputs: &VerdictInputs<'_>,
    id: usize,
    p: &PackingParams,
    holders: &HashMap<H256, usize>,
    by_hash: &HashMap<H256, PartitionAssignment>,
) -> Classified {
    let ident = inputs.identity;
    let Some(hash) = p.partition_hash else {
        return Classified::new(DriveVerdict::Empty, DriveAction::Idle);
    };
    let recorded_newer = recorded_newer(inputs, p);
    if let Some(&new) = by_hash.get(&hash) {
        if holders.get(&hash) != Some(&id) {
            return Classified::wait(WaitReason::DuplicateHolder(hash));
        }
        if p.leaves_ledger_for(&new) && recorded_newer {
            return Classified::wait(WaitReason::RecordedNewerThanEpoch);
        }
        // A v1 file, a file of another genesis, or a v2 file with no trusted
        // height is stamped, only at an in-sync epoch: at startup or while
        // catching up the epoch may not be the network's.
        let needs_stamp = !p.is_v2()
            || p.genesis_hash != Some(ident.genesis_hash)
            || p.trusted_assigned_height().is_none();
        return Classified {
            verdict: DriveVerdict::Ours,
            action: DriveAction::Assign(new),
            stamp_v2: inputs.in_sync && needs_stamp,
        };
    }
    // Not assigned to this miner.
    let same_chain = p.is_v2()
        && p.chain_id == Some(ident.chain_id)
        && p.genesis_hash == Some(ident.genesis_hash);
    if !same_chain {
        return Classified::wait(WaitReason::NotProvablyReleased);
    }
    if p.trusted_assigned_height().is_none() {
        return Classified::wait(WaitReason::NoTrustedHeight);
    }
    if recorded_newer {
        return Classified::wait(WaitReason::RecordedNewerThanEpoch);
    }
    Classified::new(DriveVerdict::Released, DriveAction::Clear)
}

/// Reads each configured drive's `packing_params.toml`, in configured order
/// (index = module id). `paths` are resolved as the storage submodules
/// config resolves them at load, the paths open reads and stores write. No
/// file is `Missing` (a fresh drive). A file that cannot be checked, read
/// or parsed is an error naming the drive: the node stops rather than skip a
/// drive whose contents it cannot know. Never writes.
pub fn drive_verdict_inputs_from_disk(
    paths: &[PathBuf],
) -> eyre::Result<Vec<(PathBuf, DriveFile)>> {
    paths
        .iter()
        .map(|path| {
            let file = path.join(PACKING_PARAMS_FILE_NAME);
            // The entry itself, not its target: a dangling symlink is a
            // record that cannot be read, not a fresh drive.
            let found = match std::fs::symlink_metadata(&file) {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => DriveFile::Missing,
                Ok(_) => DriveFile::Recorded(PackingParams::from_toml(&file).map_err(|err| {
                    eyre::eyre!(
                        "drive {}: packing params at {} cannot be read: {err:#}",
                        path.display(),
                        file.display()
                    )
                })?),
                Err(err) => eyre::bail!(
                    "drive {}: packing params at {} cannot be checked: {err}",
                    path.display(),
                    file.display()
                ),
            };
            Ok((path.clone(), found))
        })
        .collect()
}

/// One parked [`StorageModuleInfo`] per configured drive, in id order: every
/// drive is opened with no assignment, so open never changes what a drive
/// holds, and a module id is its configured path index. The storage module
/// service applies the startup verdict from the first assignment update.
pub fn parked_storage_module_infos(paths: &[PathBuf], num_chunks: u32) -> Vec<StorageModuleInfo> {
    paths
        .iter()
        .enumerate()
        .map(|(id, path)| StorageModuleInfo {
            id,
            partition_assignment: None,
            submodules: vec![(partition_chunk_offset_ie!(0, num_chunks), path.clone())],
        })
        .collect()
}

/// What startup knows before any module opens: this node's chain identity,
/// every configured drive's packing params file, and one parked module per
/// drive in `paths`.
#[derive(Debug, Clone)]
pub struct StartupDrives {
    pub identity: ChainIdentity,
    pub drives: Vec<(PathBuf, DriveFile)>,
    pub parked: Vec<StorageModuleInfo>,
}

/// Reads every configured drive's packing params before any is opened. A
/// drive whose file cannot be read or parsed, or that is foreign (newer
/// version, other chain, other miner), is an error naming it: no drive is
/// skipped. Each module opens parked (open never changes what a drive
/// holds); the storage module service applies the startup verdict from the
/// first assignment update. Nothing is written here.
pub fn read_startup_drives(
    config: &Config,
    paths: &[PathBuf],
    genesis_hash: H256,
) -> eyre::Result<StartupDrives> {
    let identity = ChainIdentity::new(config, genesis_hash);
    let drives = drive_verdict_inputs_from_disk(paths)?;
    refuse_foreign_drives(&drives, &identity)?;
    // Existing module paths retain their physical layout; logical bounds are
    // resolved separately and the finalized migration removes their suffix.
    let num_chunks = u32::try_from(config.consensus.num_chunks_in_partition)
        .map_err(|_| eyre::eyre!("num_chunks_in_partition exceeds u32"))?;
    Ok(StartupDrives {
        identity,
        drives,
        parked: parked_storage_module_infos(paths, num_chunks),
    })
}

/// For tests without a running node or service: [`read_startup_drives`],
/// then the startup verdict for `snapshot`'s assignments to this miner
/// applied to each parked module (`None` for a drive that waits, idles or is
/// cleared). It judges with no genesis hash, so no unassigned v2 drive is
/// provably released. Open accepts such an assignment only for a fresh drive
/// or one whose file already records it. Panics where startup would fail.
#[cfg(any(test, feature = "test-utils"))]
pub fn startup_storage_module_infos(
    snapshot: &crate::EpochSnapshot,
    paths: &[PathBuf],
) -> Vec<StorageModuleInfo> {
    let StartupDrives {
        identity,
        drives,
        mut parked,
    } = read_startup_drives(&snapshot.config, paths, H256::zero())
        .expect("drive files are readable and this node's");
    let assignments = snapshot.get_partition_assignments(identity.miner);
    let verdict = decide(&VerdictInputs {
        drives: &drives,
        assignments: &assignments,
        epoch_height: snapshot.epoch_height,
        in_sync: false,
        identity: &identity,
    });
    for (info, decision) in parked.iter_mut().zip(&verdict.decisions) {
        if let DriveAction::Assign(assignment) = decision.action {
            info.partition_assignment = Some(assignment);
        }
    }
    parked
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::DataLedger;

    const CHAIN: u64 = 1270;

    fn miner() -> IrysAddress {
        IrysAddress::from([0xAA; 20])
    }
    fn genesis() -> H256 {
        H256::from([0x77; 32])
    }
    fn ident() -> ChainIdentity {
        ChainIdentity {
            chain_id: CHAIN,
            genesis_hash: genesis(),
            miner: miner(),
        }
    }
    /// Hash whose byte order equals `n` order.
    fn h(n: u32) -> H256 {
        let mut b = [0_u8; 32];
        b[28..].copy_from_slice(&n.to_be_bytes());
        H256::from(b)
    }
    fn pa(n: u32, ledger: Option<DataLedger>, slot: Option<usize>) -> PartitionAssignment {
        PartitionAssignment {
            partition_hash: h(n),
            miner_address: miner(),
            ledger_id: ledger.map(Into::into),
            slot_index: slot,
        }
    }
    fn v1(n: u32, ledger: Option<DataLedger>, slot: Option<usize>) -> PackingParams {
        PackingParams {
            packing_address: miner(),
            partition_hash: Some(h(n)),
            ledger: ledger.map(Into::into),
            slot,
            last_updated_height: Some(5),
            ..Default::default()
        }
    }
    fn v2(n: u32, ledger: Option<DataLedger>, slot: Option<usize>, at: u64) -> PackingParams {
        PackingParams {
            version: Some(2),
            chain_id: Some(CHAIN),
            genesis_hash: Some(genesis()),
            assigned_height: Some(at),
            ..v1(n, ledger, slot)
        }
    }
    fn rec(p: PackingParams) -> DriveFile {
        DriveFile::Recorded(p)
    }
    fn drive(i: usize, f: DriveFile) -> (PathBuf, DriveFile) {
        (PathBuf::from(format!("/d{i}")), f)
    }
    fn run(
        drives: &[(PathBuf, DriveFile)],
        assignments: &[PartitionAssignment],
        sync: bool,
    ) -> (Vec<DriveDecision>, Vec<PartitionAssignment>) {
        let id = ident();
        let inputs = VerdictInputs {
            drives,
            assignments,
            epoch_height: 100,
            in_sync: sync,
            identity: &id,
        };
        let verdict = decide(&inputs);
        (verdict.decisions, verdict.unplaced)
    }
    fn assigned_hash(d: &DriveDecision) -> Option<H256> {
        match &d.action {
            DriveAction::Assign(p) => Some(p.partition_hash),
            _ => None,
        }
    }

    /// A foreign drive file (other miner, other chain, newer version) is an
    /// error naming the drive: the node does not start with it, and no drive
    /// is skipped.
    #[test]
    fn foreign_drive_is_an_error_naming_the_drive() {
        let mut other_miner = v2(1, Some(DataLedger::Publish), Some(0), 10);
        other_miner.packing_address = IrysAddress::from([0xBB; 20]);
        let mut other_chain = v2(1, Some(DataLedger::Publish), Some(0), 10);
        other_chain.chain_id = Some(CHAIN + 1);
        let mut newer = v2(1, Some(DataLedger::Publish), Some(0), 10);
        newer.version = Some(3);
        for (params, why) in [
            (other_miner, "other packing address"),
            (other_chain, "other chain id"),
            (newer, "newer file version"),
        ] {
            let drives = [drive(0, DriveFile::Missing), drive(1, rec(params))];
            let err = format!(
                "{:#}",
                refuse_foreign_drives(&drives, &ident())
                    .expect_err("a foreign drive stops the node")
            );
            assert!(err.contains("/d1"), "{err}");
            assert!(err.contains(why), "{err}");
        }
    }

    #[test]
    fn held_partition_stays_whatever_the_order() {
        let files: Vec<(usize, DriveFile)> = vec![
            (0, rec(v1(1, Some(DataLedger::Publish), Some(0)))),
            (1, rec(v2(2, Some(DataLedger::Submit), Some(0), 10))),
            (2, rec(v1(3, None, None))),
            (3, DriveFile::Missing),
            (4, rec(v1(4, Some(DataLedger::OneYear), Some(1)))),
            (5, rec(v2(60, Some(DataLedger::Submit), Some(9), 10))),
        ];
        let a = [
            pa(1, Some(DataLedger::Publish), Some(0)),
            pa(2, Some(DataLedger::Submit), Some(0)),
            pa(3, None, None),
            pa(4, Some(DataLedger::OneYear), Some(1)),
            pa(50, None, None),
        ];
        let by_path = |order: &[usize]| {
            let drives: Vec<_> = order
                .iter()
                .map(|&i| drive(i, files[i].1.clone()))
                .collect();
            let (d, _) = run(&drives, &a, true);
            // Only held drives: which free drive takes the unheld assignment
            // legitimately depends on order.
            let mut m: Vec<_> = d
                .into_iter()
                .filter(|x| x.verdict == DriveVerdict::Ours)
                .map(|x| (x.path, x.verdict, x.action, x.stamp_v2))
                .collect();
            m.sort_by(|a, b| a.0.cmp(&b.0));
            m
        };
        let base = by_path(&[0, 1, 2, 3, 4, 5]);
        assert_eq!(base.len(), 4);
        assert_eq!(base, by_path(&[5, 4, 3, 2, 1, 0]));
        assert_eq!(base, by_path(&[2, 5, 4, 0, 3, 1]));
        assert_eq!(base[0].2, DriveAction::Assign(a[0]));
    }

    fn node1(orphan: PartitionAssignment) -> (Vec<(PathBuf, DriveFile)>, Vec<PartitionAssignment>) {
        let mut drives = vec![];
        let mut a = vec![];
        for i in 0..43_u32 {
            let n = 100 + i;
            let (l, s) = if i % 2 == 0 {
                (Some(DataLedger::Submit), Some(i as usize))
            } else {
                (None, None)
            };
            let mut p = v1(n, l, s);
            p.last_updated_height = [None, Some(2_073_600), Some(2_822_400)][(i % 3) as usize];
            drives.push(drive(i as usize, rec(p)));
            a.push(pa(n, l, s));
        }
        a.push(orphan);
        (drives, a)
    }

    /// A restart changes nothing whether the unheld assignment is capacity
    /// or on a data ledger.
    #[test]
    fn node1_shape_restart_changes_nothing() {
        for orphan in [
            pa(9_999, None, None),
            pa(1, Some(DataLedger::Publish), Some(7)),
        ] {
            let (drives, a) = node1(orphan);
            let (d, u) = run(&drives, &a, false);
            assert_eq!(d.len(), 43);
            for (i, x) in d.iter().enumerate() {
                assert_eq!(x.verdict, DriveVerdict::Ours);
                assert_eq!(assigned_hash(x), Some(h(100 + i as u32)));
                assert!(!x.stamp_v2, "not in sync, never stamp");
            }
            assert_eq!(u, vec![orphan]);
        }
    }

    #[test]
    fn appended_empty_drive_takes_only_the_orphan() {
        let orphan = pa(9_999, None, None);
        let (mut drives, a) = node1(orphan);
        drives.push(drive(43, DriveFile::Missing));
        let (d, u) = run(&drives, &a, true);
        assert_eq!(d.len(), 44);
        for (i, x) in d.iter().take(43).enumerate() {
            assert_eq!(assigned_hash(x), Some(h(100 + i as u32)));
        }
        assert_eq!(d[43].verdict, DriveVerdict::Empty);
        assert_eq!(d[43].action, DriveAction::Assign(orphan));
        assert!(u.is_empty());
    }

    #[test]
    fn genesis_mismatch_assigned_is_ours_and_restamped() {
        let mut p = v2(1, Some(DataLedger::Publish), Some(0), 10);
        p.genesis_hash = Some(H256::from([0x11; 32]));
        let a = [pa(1, Some(DataLedger::Publish), Some(0))];
        let (d, _) = run(&[drive(0, rec(p))], &a, true);
        assert_eq!(d[0].verdict, DriveVerdict::Ours);
        assert_eq!(d[0].action, DriveAction::Assign(a[0]));
        assert!(d[0].stamp_v2);
    }

    #[test]
    fn genesis_mismatch_unassigned_waits() {
        let mut p = v2(1, Some(DataLedger::Publish), Some(0), 10);
        p.genesis_hash = Some(H256::from([0x11; 32]));
        let (d, _) = run(&[drive(0, rec(p))], &[], true);
        assert!(matches!(d[0].verdict, DriveVerdict::Wait(_)));
        assert_eq!(d[0].action, DriveAction::Idle);
    }

    /// A drive holding a partition this epoch does not assign (old format with
    /// no height, an older or a newer height) is never given the unheld
    /// partition, whatever the drive order: only the empty drive takes it.
    #[test]
    fn unassigned_holder_is_not_reused_whatever_the_order() {
        let mut legacy = v1(9, None, None);
        legacy.last_updated_height = None;
        let mut newer = v1(8, None, None);
        newer.last_updated_height = Some(200);
        let files = [
            rec(legacy),
            rec(newer),
            rec(v1(7, None, None)),
            rec(v1(2, None, None)),
            DriveFile::Missing,
        ];
        let a = [pa(1, None, None), pa(2, None, None)];
        for order in [[0, 1, 2, 3, 4], [4, 3, 2, 1, 0], [2, 4, 0, 3, 1]] {
            let drives: Vec<_> = order.iter().map(|&i| drive(i, files[i].clone())).collect();
            let (d, u) = run(&drives, &a, true);
            for x in &d {
                let (verdict, action) = match x.path.to_str().unwrap() {
                    "/d0" | "/d1" | "/d2" => (
                        DriveVerdict::Wait(WaitReason::NotProvablyReleased),
                        DriveAction::Idle,
                    ),
                    "/d3" => (DriveVerdict::Ours, DriveAction::Assign(a[1])),
                    "/d4" => (DriveVerdict::Empty, DriveAction::Assign(a[0])),
                    other => panic!("unexpected drive {other}"),
                };
                assert_eq!((&x.verdict, &x.action), (&verdict, &action), "{order:?}");
            }
            assert!(u.is_empty(), "{order:?}: {u:?}");
        }
    }

    #[test]
    fn released_is_cleared() {
        let f = rec(v2(1, Some(DataLedger::Publish), Some(0), 10));
        let (d, _) = run(&[drive(0, f)], &[], true);
        assert_eq!(d[0].verdict, DriveVerdict::Released);
        assert_eq!(d[0].action, DriveAction::Clear);
    }

    #[test]
    fn ledger_exit_applies_now() {
        let f = rec(v2(1, Some(DataLedger::Submit), Some(0), 10));
        let a = [pa(1, Some(DataLedger::Publish), Some(0))];
        let (d, _) = run(&[drive(0, f)], &a, true);
        assert_eq!(d[0].verdict, DriveVerdict::Ours);
        assert_eq!(d[0].action, DriveAction::Assign(a[0]));
    }

    #[test]
    fn capacity_gaining_ledger_applies_now() {
        let f = rec(v2(1, None, None, 10));
        let a = [pa(1, Some(DataLedger::Publish), Some(3))];
        let (d, _) = run(&[drive(0, f)], &a, true);
        assert_eq!(d[0].verdict, DriveVerdict::Ours);
        assert_eq!(d[0].action, DriveAction::Assign(a[0]));
    }

    #[test]
    fn duplicate_holder_waits() {
        let f = rec(v2(1, Some(DataLedger::Publish), Some(0), 10));
        let a = [pa(1, Some(DataLedger::Publish), Some(0))];
        let (d, u) = run(&[drive(0, f.clone()), drive(1, f)], &a, true);
        assert_eq!(d[0].verdict, DriveVerdict::Ours);
        assert!(matches!(d[1].verdict, DriveVerdict::Wait(_)));
        assert_eq!(d[1].action, DriveAction::Idle);
        assert!(u.is_empty());
    }

    #[test]
    fn placement_order_is_ledger_then_hash() {
        let a = [
            pa(5, None, None),
            pa(9, Some(DataLedger::ThirtyDay), Some(0)),
            pa(8, Some(DataLedger::Submit), Some(0)),
            pa(7, Some(DataLedger::Publish), Some(0)),
            pa(6, Some(DataLedger::Submit), Some(1)),
            pa(4, Some(DataLedger::OneYear), Some(0)),
        ];
        let drives: Vec<_> = (0..4).map(|i| drive(i, DriveFile::Missing)).collect();
        let (d, u) = run(&drives, &a, true);
        let got: Vec<_> = d.iter().map(|x| assigned_hash(x).unwrap()).collect();
        assert_eq!(got, vec![h(7), h(6), h(8), h(4)]);
        assert_eq!(u, vec![a[1], a[0]]);
    }

    #[test]
    fn empty_fills_before_released_and_released_then_gets_assign() {
        let a = [
            pa(2, Some(DataLedger::Submit), Some(0)),
            pa(1, Some(DataLedger::Publish), Some(0)),
        ];
        // Released listed first; Empty must still be filled first.
        let drives = [
            drive(0, rec(v2(60, Some(DataLedger::Submit), Some(9), 10))),
            drive(1, DriveFile::Missing),
        ];
        let (d, u) = run(&drives, &a, true);
        assert_eq!(d[1].verdict, DriveVerdict::Empty);
        assert_eq!(d[1].action, DriveAction::Assign(a[1]));
        assert_eq!(d[0].verdict, DriveVerdict::Released);
        assert_eq!(d[0].action, DriveAction::Assign(a[0]));
        assert!(u.is_empty());
    }

    #[test]
    fn duplicate_with_ledger_exit_keeps_hash_held() {
        let f = rec(v2(1, Some(DataLedger::Submit), Some(0), 10));
        let a = [pa(1, Some(DataLedger::Publish), Some(0))];
        let (d, u) = run(&[drive(0, f.clone()), drive(1, f)], &a, true);
        assert_eq!(d[0].verdict, DriveVerdict::Ours);
        assert_eq!(d[0].action, DriveAction::Assign(a[0]));
        assert_eq!(
            d[1].verdict,
            DriveVerdict::Wait(WaitReason::DuplicateHolder(h(1)))
        );
        assert_eq!(d[1].action, DriveAction::Idle);
        assert!(u.is_empty());
    }

    #[test]
    fn stamp_only_when_in_sync() {
        let a = [pa(1, Some(DataLedger::Publish), Some(0))];
        let f = rec(v1(1, Some(DataLedger::Publish), Some(0)));
        let (d, _) = run(&[drive(0, f.clone())], &a, false);
        assert!(!d[0].stamp_v2);
        let (d, _) = run(&[drive(0, f)], &a, true);
        assert!(d[0].stamp_v2);
        let ok = rec(v2(1, Some(DataLedger::Publish), Some(0), 10));
        let (d, _) = run(&[drive(0, ok)], &a, true);
        assert!(!d[0].stamp_v2);
    }

    /// A drive whose file records a ledger slot installed after the epoch it
    /// is judged on (the orphaned-epoch resend: a snapshot older than the
    /// drive's own state) does not leave that ledger: it waits, idle. A drive
    /// that keeps its ledger stays ours whatever its recorded height.
    #[test]
    fn ledger_exit_against_an_older_epoch_waits() {
        let newer = rec(v2(1, Some(DataLedger::Publish), Some(1), 150));
        let a = [pa(1, None, None)];
        let (d, _) = run(&[drive(0, newer.clone())], &a, true);
        assert_eq!(
            d[0].verdict,
            DriveVerdict::Wait(WaitReason::RecordedNewerThanEpoch)
        );
        assert_eq!(
            d[0].verdict.reason(),
            "recorded state is newer than the epoch"
        );
        assert_eq!(d[0].action, DriveAction::Idle);
        assert!(!d[0].stamp_v2);
        let keep = [pa(1, Some(DataLedger::Publish), Some(1))];
        let (d, _) = run(&[drive(0, newer)], &keep, true);
        assert_eq!(d[0].verdict, DriveVerdict::Ours);
        assert_eq!(d[0].action, DriveAction::Assign(keep[0]));
    }

    /// A v2 file that records no trusted assigned height (open rebound it) is
    /// re-stamped when it is ours at an in-sync epoch. Unassigned, it waits
    /// with its own reason: its release cannot be proven.
    #[test]
    fn v2_without_a_trusted_height_is_restamped() {
        let mut p = v2(1, Some(DataLedger::Publish), Some(0), 10);
        p.assigned_height = None;
        let a = [pa(1, Some(DataLedger::Publish), Some(0))];
        let (d, _) = run(&[drive(0, rec(p))], &a, true);
        assert_eq!(d[0].verdict, DriveVerdict::Ours);
        assert!(d[0].stamp_v2, "a missing trusted height is stamped");

        let (d, _) = run(&[drive(0, rec(p))], &[], true);
        assert_eq!(
            d[0].verdict,
            DriveVerdict::Wait(WaitReason::NoTrustedHeight)
        );
        assert_eq!(d[0].verdict.reason(), "no trusted assigned height");
        assert_eq!(d[0].action, DriveAction::Idle);
    }

    /// A release judged on an epoch older than the drive's assigned height
    /// waits: only a later epoch can prove it.
    #[test]
    fn release_against_an_older_epoch_waits() {
        let f = rec(v2(1, Some(DataLedger::Publish), Some(0), 150));
        let (d, _) = run(&[drive(0, f)], &[], true);
        assert_eq!(
            d[0].verdict,
            DriveVerdict::Wait(WaitReason::RecordedNewerThanEpoch)
        );
        assert_eq!(
            d[0].verdict.reason(),
            "recorded state is newer than the epoch"
        );
    }

    /// Startup reads each drive's file without writing: no file is Missing.
    /// A corrupt
    /// file is an error naming the drive and its file.
    #[test]
    fn inputs_from_disk_reads_missing_and_recorded_and_fails_on_corrupt() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("drive_inputs")
            .build();
        let missing = tmp.path().join("missing");
        let corrupt = tmp.path().join("corrupt");
        let held = tmp.path().join("held");
        for dir in [&missing, &corrupt, &held] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let corrupt_file = corrupt.join(PACKING_PARAMS_FILE_NAME);
        std::fs::write(&corrupt_file, b"not = toml [[[").unwrap();
        let params = v2(1, Some(DataLedger::Publish), Some(0), 10);
        let held_file = held.join(PACKING_PARAMS_FILE_NAME);
        params.write_to_disk(&held_file).unwrap();
        let held_bytes = std::fs::read(&held_file).unwrap();

        let inputs = drive_verdict_inputs_from_disk(&[missing.clone(), held.clone()])
            .expect("readable drives");
        assert_eq!(inputs[0], (missing.clone(), DriveFile::Missing));
        assert_eq!(inputs[1], (held.clone(), DriveFile::Recorded(params)));
        assert!(!missing.join(PACKING_PARAMS_FILE_NAME).exists());
        assert_eq!(std::fs::read(&held_file).unwrap(), held_bytes);

        let err = drive_verdict_inputs_from_disk(&[missing, corrupt.clone(), held])
            .expect_err("a corrupt file stops the node");
        let err = format!("{err:#}");
        assert!(
            err.contains(&format!("drive {}", corrupt.display())),
            "{err}"
        );
        assert!(err.contains(&corrupt_file.display().to_string()), "{err}");
    }

    /// An unreadable file (an I/O error, here a directory in its place) is an
    /// error naming the drive.
    #[test]
    fn inputs_from_disk_fails_on_an_unreadable_file() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("drive_inputs_eio")
            .build();
        let unreadable = tmp.path().join("unreadable");
        std::fs::create_dir_all(unreadable.join(PACKING_PARAMS_FILE_NAME)).unwrap();
        let err = drive_verdict_inputs_from_disk(std::slice::from_ref(&unreadable))
            .expect_err("an unreadable file stops the node");
        let err = format!("{err:#}");
        assert!(
            err.contains(&format!("drive {}", unreadable.display())),
            "{err}"
        );
    }

    /// Two eligible holders of one partition: the drive whose recorded slot
    /// already matches the assignment keeps it, whatever the config order.
    /// The other waits, so no drive leaves its slot and repacks for nothing.
    #[test]
    fn duplicate_holder_on_the_assigned_slot_wins_whatever_the_order() {
        let on_slot = rec(v2(1, Some(DataLedger::Publish), Some(0), 100));
        let off_slot = rec(v2(1, Some(DataLedger::Submit), Some(3), 50));
        let a = [pa(1, Some(DataLedger::Publish), Some(0))];
        for on_slot_first in [true, false] {
            let drives = if on_slot_first {
                [drive(0, on_slot.clone()), drive(1, off_slot.clone())]
            } else {
                [drive(0, off_slot.clone()), drive(1, on_slot.clone())]
            };
            let (on, off) = if on_slot_first { (0, 1) } else { (1, 0) };
            let (d, u) = run(&drives, &a, true);
            assert_eq!(d[on].verdict, DriveVerdict::Ours, "{on_slot_first}");
            assert_eq!(d[on].action, DriveAction::Assign(a[0]), "{on_slot_first}");
            assert_eq!(
                d[off].verdict,
                DriveVerdict::Wait(WaitReason::DuplicateHolder(h(1))),
                "{on_slot_first}"
            );
            assert_eq!(d[off].action, DriveAction::Idle, "{on_slot_first}");
            assert!(u.is_empty());
        }
    }

    /// Two drives record the same assigned partition. The one that must wait
    /// (it leaves its recorded slot with a record newer than the epoch) does
    /// not shadow the one that can take the assignment, in either order.
    #[test]
    fn eligible_duplicate_holder_wins_whatever_the_order() {
        let newer = rec(v2(1, Some(DataLedger::Publish), Some(1), 150));
        let current = rec(v2(1, Some(DataLedger::Publish), Some(0), 100));
        let assigned = [pa(1, Some(DataLedger::Publish), Some(0))];
        for (files, holder) in [
            ([newer.clone(), current.clone()], 1_usize),
            ([current, newer], 0_usize),
        ] {
            let drives: Vec<_> = files
                .into_iter()
                .enumerate()
                .map(|(i, f)| drive(i, f))
                .collect();
            let (d, u) = run(&drives, &assigned, false);
            assert_eq!(d[holder].verdict, DriveVerdict::Ours, "{d:?}");
            assert_eq!(assigned_hash(&d[holder]), Some(h(1)));
            assert_eq!(
                d[1 - holder].verdict,
                DriveVerdict::Wait(WaitReason::DuplicateHolder(h(1))),
                "{d:?}"
            );
            assert!(u.is_empty(), "the partition is served");
        }
    }

    /// A packing params entry that is a dangling symlink is not a fresh drive:
    /// it is an error naming the drive.
    #[test]
    fn inputs_from_disk_fails_on_a_dangling_symlink() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("drive_inputs_symlink")
            .build();
        let linked = tmp.path().join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::os::unix::fs::symlink(
            tmp.path().join("unmounted").join(PACKING_PARAMS_FILE_NAME),
            linked.join(PACKING_PARAMS_FILE_NAME),
        )
        .unwrap();
        let err = drive_verdict_inputs_from_disk(std::slice::from_ref(&linked))
            .expect_err("a dangling record is not a fresh drive");
        let err = format!("{err:#}");
        assert!(
            err.contains(&format!("drive {}", linked.display())),
            "{err}"
        );
    }

    /// A newer assigned height recorded on another genesis does not hold a
    /// drive back: it leaves its recorded slot for the assigned one.
    #[test]
    fn newer_height_of_another_genesis_does_not_wait() {
        let mut other = v2(1, Some(DataLedger::Publish), Some(1), 150);
        other.genesis_hash = Some(H256::from([0x11; 32]));
        let a = [pa(1, Some(DataLedger::Publish), Some(0))];
        let (d, u) = run(&[drive(0, rec(other))], &a, false);
        assert_eq!(d[0].verdict, DriveVerdict::Ours);
        assert_eq!(d[0].action, DriveAction::Assign(a[0]));
        assert!(u.is_empty());
    }

    /// Startup opens every drive parked, with its path index as module id.
    #[test]
    fn startup_opens_every_drive_parked() {
        let paths = [PathBuf::from("/d0"), PathBuf::from("/d1")];
        let infos = parked_storage_module_infos(&paths, 10);
        let got: Vec<(usize, Option<PartitionAssignment>, PathBuf)> = infos
            .iter()
            .map(|i| (i.id, i.partition_assignment, i.submodules[0].1.clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                (0, None, PathBuf::from("/d0")),
                (1, None, PathBuf::from("/d1")),
            ]
        );
    }
}
