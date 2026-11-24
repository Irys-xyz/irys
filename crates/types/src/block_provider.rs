use crate::{H256, H256List, VDFLimiterInfo};

/// A consistent snapshot of the canonical chain for the VDF loop: the tip's VDF limiter
/// info (its `seed`, `next_seed` and tip step), the last step number and output of the block
/// that is `block_migration_depth` deep, the reset seed pinned by the canonical block with the greatest
/// `global_step_number <= requested_step_number`, and the seeds of the canonical blocks above
/// the confirmed one. One canonical-chain read gives all of them, so the loop never mixes two
/// views of the chain.
///
/// The VDF loop checks the reset seeds it folded above `confirmed_global_step_number` against
/// this snapshot and recomputes from the first boundary that disagrees. See
/// `design/docs/vdf-reset-seed-fork-handling.md`.
///
/// Invariant: `confirmed_global_step_number <= vdf_info.global_step_number` — the confirmed
/// step comes from a block at or below the tip.
#[derive(Debug, Clone)]
pub struct CanonicalVdfSnapshot {
    pub vdf_info: VDFLimiterInfo,
    pub confirmed_global_step_number: u64,
    /// Last output of the confirmed block, anchoring the buffered lineage.
    pub confirmed_output: H256,
    /// Reset seed for the next boundary above `step_number`: `next_seed` of the
    /// canonical block whose VDF range ends at or before that step. Using the tip's
    /// `next_seed` alone is wrong when the local loop is behind the tip across one
    /// or more reset boundaries.
    pub reset_seed_for_step: H256,
    /// Canonical blocks above the confirmed one, oldest first, ending with the tip. Validation
    /// folds every reset boundary `b` with `b + 1` in a block's step range with that block's
    /// `seed`, so these entries give the canonical seed of every boundary above the confirmed step
    /// that a canonical block already covers.
    pub unconfirmed_blocks: Vec<CanonicalBlockVdf>,
}

/// The VDF step range of one canonical block, the reset seed its validation folds and,
/// when requested, its steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalBlockVdf {
    pub first_step: u64,
    pub last_step: u64,
    /// The block's `vdf_limiter_info.seed`.
    pub seed: H256,
    /// The block's `vdf_limiter_info.steps`; only filled by
    /// [`BlockProvider::canonical_vdf_snapshot_with_steps`].
    pub steps: Option<H256List>,
}

/// A trait that is used to provide access to blocks by their hash. Used to avoid circular dependencies,
/// such as between VDF and the block index.
pub trait BlockProvider {
    /// A consistent canonical snapshot for the current VDF loop step. `step_number` is the
    /// loop's current global step; implementations return the canonical tip info, the deepest
    /// confirmed canonical step, the reset seed pinned by the canonical block whose VDF
    /// range ends at or before `step_number`, and the seeds of the unconfirmed canonical blocks.
    ///
    /// During partition-recovery catch-up this lets the loop apply the seed pinned for the
    /// boundary it is actually about to cross.
    fn canonical_vdf_snapshot(&self, step_number: u64) -> Option<CanonicalVdfSnapshot>;

    /// [`Self::canonical_vdf_snapshot`] with the steps of every unconfirmed canonical block.
    /// Heavier: the VDF loop asks for it only when it must rewrite buffered steps, so it can
    /// copy the steps canonical blocks hold instead of recomputing them. The default gives no
    /// steps, and the loop then recomputes.
    fn canonical_vdf_snapshot_with_steps(&self, step_number: u64) -> Option<CanonicalVdfSnapshot> {
        self.canonical_vdf_snapshot(step_number)
    }
}
