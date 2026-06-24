use crate::{H256, VDFLimiterInfo};

/// A consistent snapshot of the canonical chain for the VDF loop: the tip's VDF limiter
/// info (its `next_seed` and tip step), plus the global step number of the block that is
/// `block_migration_depth` deep — the most recent reset seed confirmed past the migration
/// threshold. Also includes the reset seed pinned by the canonical block with the greatest
/// `global_step_number <= requested_step_number`, so the loop reads the tip, confirmed step,
/// and per-step seed from one provider snapshot under one canonical-chain read.
///
/// The VDF reset-boundary gate compares against `confirmed_global_step_number` so it never
/// applies a seed pinned by a still-forkable block (issue #1447).
///
/// Invariant: `confirmed_global_step_number <= vdf_info.global_step_number` — the confirmed
/// step comes from a block at or below the tip.
#[derive(Debug, Clone)]
pub struct CanonicalVdfSnapshot {
    pub vdf_info: VDFLimiterInfo,
    pub confirmed_global_step_number: u64,
    pub reset_seed_for_step: H256,
}

/// A trait that is used to provide access to blocks by their hash. Used to avoid circular dependencies,
/// such as between VDF and the block index.
pub trait BlockProvider {
    /// A consistent canonical snapshot for the current VDF loop step. `step_number` is the
    /// loop's current global step; implementations return the canonical tip info, the deepest
    /// confirmed canonical step, and the reset seed pinned by the canonical block whose VDF
    /// range ends at or before `step_number`.
    ///
    /// During partition-recovery catch-up this lets the loop apply the seed pinned for the
    /// boundary it is actually about to cross, while preserving the confirmed-step gate.
    fn canonical_vdf_snapshot(&self, step_number: u64) -> Option<CanonicalVdfSnapshot>;
}
