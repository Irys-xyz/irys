use crate::{H256, VDFLimiterInfo};

/// A consistent snapshot of the canonical chain for the VDF loop: the tip's VDF limiter
/// info (its `next_seed` and tip step), plus the global step number of the block that is
/// `block_migration_depth` deep — the most recent reset seed confirmed past the migration
/// threshold. The VDF reset-boundary gate compares against `confirmed_global_step_number`
/// so it never applies a seed pinned by a still-forkable block (issue #1447).
///
/// Invariant: `confirmed_global_step_number <= vdf_info.global_step_number` — the confirmed
/// step comes from a block at or below the tip.
#[derive(Debug, Clone)]
pub struct CanonicalVdfSnapshot {
    pub vdf_info: VDFLimiterInfo,
    pub confirmed_global_step_number: u64,
}

/// A trait that is used to provide access to blocks by their hash. Used to avoid circular dependencies,
/// such as between VDF and the block index.
pub trait BlockProvider {
    fn latest_canonical_vdf_info(&self) -> Option<CanonicalVdfSnapshot>;

    /// The reset seed (`next_seed`) of the canonical block with the greatest `global_step_number <=
    /// step_number` — the last block whose VDF range ENDS at or before `step_number` — rather than
    /// the chain TIP's. That `next_seed` pins the reset seed for the next boundary ABOVE
    /// `step_number`, i.e. the boundary the VDF is about to cross when it is at that step.
    ///
    /// The VDF loop uses this so that during partition-recovery CATCH-UP — when the re-anchored VDF
    /// local-steps across reset boundaries the canonical tip has already passed — it applies the
    /// seed pinned for each boundary it crosses, not the tip's `next_seed` (which targets a HIGHER
    /// boundary and would re-poison the post-boundary steps, the Finding #1 wedge). It is the
    /// block ENDING at or before the step (not the block whose range *covers* it) because a single
    /// block can SPAN a boundary and its `next_seed` targets the boundary above its range.
    ///
    /// In normal forward operation the VDF runs AHEAD of the canonical tip, so the greatest
    /// canonical `global_step_number <= step_number` is the tip itself — equal to the prior
    /// (tip-seed) behavior, making this a no-op there. The default returns `None` so non-canonical
    /// providers keep the prior behavior; callers fall back to the tip's seed on `None`.
    fn canonical_vdf_info_at_or_below_step(&self, step_number: u64) -> Option<H256> {
        let _ = step_number;
        None
    }
}
