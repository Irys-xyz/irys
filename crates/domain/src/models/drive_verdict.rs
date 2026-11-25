//! Pure verdict on what each configured drive should hold.
//!
//! The verdict depends only on what is recorded on each drive and on the
//! canonical epoch assignments, never on drive order, so appending, removing
//! or reordering drives cannot change which partition a drive keeps.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use irys_types::{DataLedger, H256, IrysAddress, partition::PartitionAssignment};

use super::storage_module::{PACKING_PARAMS_VERSION, PackingParams};

/// What was found at a drive's `packing_params.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveFile {
    Missing,
    Unreadable(String),
    Recorded(PackingParams),
}

/// Who and what chain this node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainIdentity {
    pub chain_id: u64,
    pub genesis_hash: H256,
    pub miner: IrysAddress,
}

#[derive(Debug, Clone, Copy)]
pub struct VerdictInputs<'a> {
    /// Configured order; index = module id.
    pub drives: &'a [(PathBuf, DriveFile)],
    /// This miner's assignments, from the canonical epoch snapshot.
    pub assignments: &'a [PartitionAssignment],
    pub epoch_height: u64,
    /// `epoch_height <= finalized_height`.
    pub epoch_is_final: bool,
    /// Node is in sync with the network head (false at startup).
    pub in_sync: bool,
    pub identity: &'a ChainIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Foreign(String),
    Ours,
    Empty,
    Released,
    Wait(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveAction {
    /// Foreign: never open, never touch.
    Skip,
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
    pub verdict: Verdict,
    pub action: DriveAction,
    pub stamp_v2: bool,
}

fn is_foreign(p: &PackingParams, id: &ChainIdentity) -> Option<String> {
    if p.version.is_some_and(|v| v > PACKING_PARAMS_VERSION) {
        return Some(format!("newer file version {:?}", p.version));
    }
    if p.chain_id.is_some_and(|c| c != id.chain_id) {
        return Some(format!("other chain id {:?}", p.chain_id));
    }
    if p.packing_address != id.miner {
        return Some(format!("other packing address {}", p.packing_address));
    }
    None
}

/// Ledger priority for placement: Publish, Submit, OneYear, ThirtyDay, then
/// capacity. Equals ascending ledger id with capacity last.
fn placement_key(a: &PartitionAssignment) -> (u32, H256) {
    let rank = match a.ledger_id {
        Some(l) if l == DataLedger::Publish => 0,
        Some(l) if l == DataLedger::Submit => 1,
        Some(l) if l == DataLedger::OneYear => 2,
        Some(l) if l == DataLedger::ThirtyDay => 3,
        Some(l) => 4 + l,
        None => u32::MAX,
    };
    (rank, a.partition_hash)
}

/// First holder (in id order) of each assigned hash among non-foreign drives.
fn first_holders(inputs: &VerdictInputs<'_>) -> HashMap<H256, usize> {
    let assigned: HashSet<H256> = inputs
        .assignments
        .iter()
        .map(|a| a.partition_hash)
        .collect();
    let mut m = HashMap::new();
    for (i, (_, f)) in inputs.drives.iter().enumerate() {
        if let DriveFile::Recorded(p) = f
            && is_foreign(p, inputs.identity).is_none()
            && let Some(h) = p.partition_hash
            && assigned.contains(&h)
        {
            m.entry(h).or_insert(i);
        }
    }
    m
}

fn any_unreadable(inputs: &VerdictInputs<'_>) -> bool {
    inputs
        .drives
        .iter()
        .any(|(_, f)| matches!(f, DriveFile::Unreadable(_)))
}

/// Assignments no drive holds, in placement order.
fn unheld(inputs: &VerdictInputs<'_>) -> Vec<PartitionAssignment> {
    let held = first_holders(inputs);
    let mut v: Vec<_> = inputs
        .assignments
        .iter()
        .filter(|a| !held.contains_key(&a.partition_hash))
        .copied()
        .collect();
    v.sort_by_key(placement_key);
    v.dedup_by_key(|a| a.partition_hash);
    v
}

/// Decide, for every drive, what it keeps or receives. Exactly one decision per
/// drive, in id order. Pure: no I/O, clocks or global state.
///
/// Unreadable-suppression rule: while any drive is `Unreadable` we cannot know
/// it does not hold an unheld assignment, so no unheld assignment is placed on
/// an Empty or Released drive; leftovers are reported by [`unplaced`].
pub fn decide(inputs: &VerdictInputs<'_>) -> Vec<DriveDecision> {
    let holders = first_holders(inputs);
    let by_hash: HashMap<H256, PartitionAssignment> = inputs
        .assignments
        .iter()
        .map(|a| (a.partition_hash, *a))
        .collect();

    let mut out: Vec<DriveDecision> = inputs
        .drives
        .iter()
        .enumerate()
        .map(|(id, (path, file))| {
            let (verdict, action, stamp_v2) = match file {
                DriveFile::Unreadable(e) => (
                    Verdict::Foreign(format!("unreadable: {e}")),
                    DriveAction::Skip,
                    false,
                ),
                DriveFile::Missing => (Verdict::Empty, DriveAction::Idle, false),
                DriveFile::Recorded(p) => classify(inputs, id, p, &holders, &by_hash),
            };
            DriveDecision {
                id,
                path: path.clone(),
                verdict,
                action,
                stamp_v2,
            }
        })
        .collect();

    if !any_unreadable(inputs) {
        let mut queue = unheld(inputs).into_iter();
        for want in [Verdict::Empty, Verdict::Released] {
            for d in out.iter_mut().filter(|d| d.verdict == want) {
                if let Some(a) = queue.next() {
                    d.action = DriveAction::Assign(a);
                }
            }
        }
    }
    out
}

fn classify(
    inputs: &VerdictInputs<'_>,
    id: usize,
    p: &PackingParams,
    holders: &HashMap<H256, usize>,
    by_hash: &HashMap<H256, PartitionAssignment>,
) -> (Verdict, DriveAction, bool) {
    let ident = inputs.identity;
    if let Some(why) = is_foreign(p, ident) {
        return (Verdict::Foreign(why), DriveAction::Skip, false);
    }
    let Some(hash) = p.partition_hash else {
        return (Verdict::Empty, DriveAction::Idle, false);
    };
    if let Some(&new) = by_hash.get(&hash) {
        if holders.get(&hash) != Some(&id) {
            return (
                Verdict::Wait(format!("duplicate holder of {hash}")),
                DriveAction::Idle,
                false,
            );
        }
        let leaving = p.ledger.is_some() && (p.ledger, p.slot) != (new.ledger_id, new.slot_index);
        if leaving && !inputs.epoch_is_final {
            return (
                Verdict::Wait("ledger exit pending finality".into()),
                DriveAction::Idle,
                false,
            );
        }
        let stamp = inputs.in_sync && (!p.is_v2() || p.genesis_hash != Some(ident.genesis_hash));
        return (Verdict::Ours, DriveAction::Assign(new), stamp);
    }
    // Not assigned to this miner.
    let same_chain = p.is_v2()
        && p.chain_id == Some(ident.chain_id)
        && p.genesis_hash == Some(ident.genesis_hash);
    if !same_chain {
        return (
            Verdict::Wait("unassigned, but not provably released on this chain".into()),
            DriveAction::Idle,
            false,
        );
    }
    let old_enough = p.assigned_height.is_some_and(|h| h <= inputs.epoch_height);
    if !old_enough || !inputs.epoch_is_final {
        return (
            Verdict::Wait("release pending finality".into()),
            DriveAction::Idle,
            false,
        );
    }
    (Verdict::Released, DriveAction::Clear, false)
}

/// Assignments that no drive holds or receives, in placement order.
pub fn unplaced(
    inputs: &VerdictInputs<'_>,
    decisions: &[DriveDecision],
) -> Vec<PartitionAssignment> {
    let received: HashSet<H256> = decisions
        .iter()
        .filter_map(|d| match &d.action {
            DriveAction::Assign(a) => Some(a.partition_hash),
            _ => None,
        })
        .collect();
    unheld(inputs)
        .into_iter()
        .filter(|a| !received.contains(&a.partition_hash))
        .collect()
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
        fin: bool,
        sync: bool,
    ) -> (Vec<DriveDecision>, Vec<PartitionAssignment>) {
        let id = ident();
        let inputs = VerdictInputs {
            drives,
            assignments,
            epoch_height: 100,
            epoch_is_final: fin,
            in_sync: sync,
            identity: &id,
        };
        let d = decide(&inputs);
        let u = unplaced(&inputs, &d);
        (d, u)
    }
    fn assigned_hash(d: &DriveDecision) -> Option<H256> {
        match &d.action {
            DriveAction::Assign(p) => Some(p.partition_hash),
            _ => None,
        }
    }

    #[test]
    fn foreign_other_miner_is_skipped() {
        let mut p = v2(1, Some(DataLedger::Publish), Some(0), 10);
        p.packing_address = IrysAddress::from([0xBB; 20]);
        let (d, _) = run(
            &[drive(0, rec(p))],
            &[pa(1, Some(DataLedger::Publish), Some(0))],
            true,
            true,
        );
        assert!(matches!(d[0].verdict, Verdict::Foreign(_)));
        assert_eq!(d[0].action, DriveAction::Skip);
    }

    #[test]
    fn foreign_other_chain_is_skipped() {
        let mut p = v2(1, Some(DataLedger::Publish), Some(0), 10);
        p.chain_id = Some(CHAIN + 1);
        let (d, _) = run(
            &[drive(0, rec(p))],
            &[pa(1, Some(DataLedger::Publish), Some(0))],
            true,
            true,
        );
        assert!(matches!(d[0].verdict, Verdict::Foreign(_)));
        assert_eq!(d[0].action, DriveAction::Skip);
    }

    #[test]
    fn foreign_newer_version_is_skipped() {
        let mut p = v2(1, Some(DataLedger::Publish), Some(0), 10);
        p.version = Some(3);
        let (d, _) = run(
            &[drive(0, rec(p))],
            &[pa(1, Some(DataLedger::Publish), Some(0))],
            true,
            true,
        );
        assert!(matches!(d[0].verdict, Verdict::Foreign(_)));
        assert_eq!(d[0].action, DriveAction::Skip);
    }

    #[test]
    fn unreadable_is_skipped_and_its_partition_is_not_placed() {
        let drives = [
            drive(0, DriveFile::Unreadable("EIO".into())),
            drive(1, DriveFile::Missing),
        ];
        let a = [pa(1, Some(DataLedger::Publish), Some(0))];
        let (d, u) = run(&drives, &a, true, true);
        assert_eq!(d[0].action, DriveAction::Skip);
        assert_eq!(d[1].verdict, Verdict::Empty);
        assert_eq!(d[1].action, DriveAction::Idle);
        assert_eq!(u, a.to_vec());
    }

    #[test]
    fn held_partition_stays_whatever_the_order() {
        let files: Vec<(usize, DriveFile)> = vec![
            (0, rec(v1(1, Some(DataLedger::Publish), Some(0)))),
            (1, rec(v2(2, Some(DataLedger::Submit), Some(0), 10))),
            (2, rec(v1(3, None, None))),
            (3, DriveFile::Missing),
            (4, rec(v1(4, Some(DataLedger::OneYear), Some(1)))),
        ];
        let a = [
            pa(1, Some(DataLedger::Publish), Some(0)),
            pa(2, Some(DataLedger::Submit), Some(0)),
            pa(3, None, None),
            pa(4, Some(DataLedger::OneYear), Some(1)),
        ];
        let by_path = |order: &[usize]| {
            let drives: Vec<_> = order
                .iter()
                .map(|&i| drive(i, files[i].1.clone()))
                .collect();
            let (d, _) = run(&drives, &a, true, true);
            let mut m: Vec<_> = d
                .into_iter()
                .map(|x| (x.path, x.verdict, x.action, x.stamp_v2))
                .collect();
            m.sort_by(|a, b| a.0.cmp(&b.0));
            m
        };
        let base = by_path(&[0, 1, 2, 3, 4]);
        assert_eq!(base, by_path(&[4, 3, 2, 1, 0]));
        assert_eq!(base, by_path(&[2, 4, 0, 3, 1]));
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

    #[test]
    fn node1_shape_restart_changes_nothing() {
        let orphan = pa(9_999, None, None);
        let (drives, a) = node1(orphan);
        let (d, u) = run(&drives, &a, true, false);
        assert_eq!(d.len(), 43);
        for (i, x) in d.iter().enumerate() {
            assert_eq!(x.verdict, Verdict::Ours);
            assert_eq!(assigned_hash(x), Some(h(100 + i as u32)));
            assert!(!x.stamp_v2, "not in sync, never stamp");
        }
        assert_eq!(u, vec![orphan]);
    }

    #[test]
    fn orphan_in_data_ledger_does_not_stop_mapping() {
        let orphan = pa(1, Some(DataLedger::Publish), Some(7));
        let (drives, a) = node1(orphan);
        let (d, u) = run(&drives, &a, true, false);
        assert!(d.iter().all(|x| x.verdict == Verdict::Ours));
        assert_eq!(u, vec![orphan]);
    }

    #[test]
    fn appended_empty_drive_takes_only_the_orphan() {
        let orphan = pa(9_999, None, None);
        let (mut drives, a) = node1(orphan);
        drives.push(drive(43, DriveFile::Missing));
        let (d, u) = run(&drives, &a, true, true);
        assert_eq!(d.len(), 44);
        for (i, x) in d.iter().take(43).enumerate() {
            assert_eq!(assigned_hash(x), Some(h(100 + i as u32)));
        }
        assert_eq!(d[43].verdict, Verdict::Empty);
        assert_eq!(d[43].action, DriveAction::Assign(orphan));
        assert!(u.is_empty());
    }

    #[test]
    fn genesis_mismatch_assigned_is_ours_and_restamped() {
        let mut p = v2(1, Some(DataLedger::Publish), Some(0), 10);
        p.genesis_hash = Some(H256::from([0x11; 32]));
        let a = [pa(1, Some(DataLedger::Publish), Some(0))];
        let (d, _) = run(&[drive(0, rec(p))], &a, true, true);
        assert_eq!(d[0].verdict, Verdict::Ours);
        assert_eq!(d[0].action, DriveAction::Assign(a[0]));
        assert!(d[0].stamp_v2);
    }

    #[test]
    fn genesis_mismatch_unassigned_waits() {
        let mut p = v2(1, Some(DataLedger::Publish), Some(0), 10);
        p.genesis_hash = Some(H256::from([0x11; 32]));
        let (d, _) = run(&[drive(0, rec(p))], &[], true, true);
        assert!(matches!(d[0].verdict, Verdict::Wait(_)));
        assert_eq!(d[0].action, DriveAction::Idle);
    }

    #[test]
    fn v1_unassigned_waits() {
        let (d, _) = run(
            &[drive(0, rec(v1(1, Some(DataLedger::Publish), Some(0))))],
            &[],
            true,
            true,
        );
        assert!(matches!(d[0].verdict, Verdict::Wait(_)));
        assert_eq!(d[0].action, DriveAction::Idle);
    }

    #[test]
    fn released_only_when_final() {
        let f = rec(v2(1, Some(DataLedger::Publish), Some(0), 10));
        let (d, _) = run(&[drive(0, f.clone())], &[], false, true);
        assert!(matches!(d[0].verdict, Verdict::Wait(_)));
        assert_eq!(d[0].action, DriveAction::Idle);
        let (d, _) = run(&[drive(0, f)], &[], true, true);
        assert_eq!(d[0].verdict, Verdict::Released);
        assert_eq!(d[0].action, DriveAction::Clear);
    }

    #[test]
    fn ledger_exit_waits_for_finality() {
        let f = rec(v2(1, Some(DataLedger::Submit), Some(0), 10));
        let a = [pa(1, Some(DataLedger::Publish), Some(0))];
        let (d, _) = run(&[drive(0, f.clone())], &a, false, true);
        assert!(matches!(d[0].verdict, Verdict::Wait(_)));
        assert_eq!(d[0].action, DriveAction::Idle);
        let (d, _) = run(&[drive(0, f)], &a, true, true);
        assert_eq!(d[0].verdict, Verdict::Ours);
        assert_eq!(d[0].action, DriveAction::Assign(a[0]));
    }

    #[test]
    fn capacity_gaining_ledger_applies_now() {
        let f = rec(v2(1, None, None, 10));
        let a = [pa(1, Some(DataLedger::Publish), Some(3))];
        let (d, _) = run(&[drive(0, f)], &a, false, true);
        assert_eq!(d[0].verdict, Verdict::Ours);
        assert_eq!(d[0].action, DriveAction::Assign(a[0]));
    }

    #[test]
    fn duplicate_holder_waits() {
        let f = rec(v2(1, Some(DataLedger::Publish), Some(0), 10));
        let a = [pa(1, Some(DataLedger::Publish), Some(0))];
        let (d, u) = run(&[drive(0, f.clone()), drive(1, f)], &a, true, true);
        assert_eq!(d[0].verdict, Verdict::Ours);
        assert!(matches!(d[1].verdict, Verdict::Wait(_)));
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
        let (d, u) = run(&drives, &a, true, true);
        let got: Vec<_> = d.iter().map(|x| assigned_hash(x).unwrap()).collect();
        assert_eq!(got, vec![h(7), h(6), h(8), h(4)]);
        assert_eq!(u, vec![a[1], a[0]]);
    }

    #[test]
    fn stamp_only_when_in_sync() {
        let a = [pa(1, Some(DataLedger::Publish), Some(0))];
        let f = rec(v1(1, Some(DataLedger::Publish), Some(0)));
        let (d, _) = run(&[drive(0, f.clone())], &a, true, false);
        assert!(!d[0].stamp_v2);
        let (d, _) = run(&[drive(0, f)], &a, true, true);
        assert!(d[0].stamp_v2);
        let ok = rec(v2(1, Some(DataLedger::Publish), Some(0), 10));
        let (d, _) = run(&[drive(0, ok)], &a, true, true);
        assert!(!d[0].stamp_v2);
    }
}
