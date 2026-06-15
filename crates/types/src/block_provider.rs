use crate::VDFLimiterInfo;

/// A consistent snapshot of the canonical chain for the VDF loop: the tip's VDF limiter
/// info (its `next_seed` and tip step), plus the global step number of the block that is
/// `block_migration_depth` deep — the most recent reset seed confirmed past the migration
/// threshold. The VDF reset-boundary gate compares against `confirmed_global_step_number`
/// so it never applies a seed pinned by a still-forkable block (issue #1447).
#[derive(Debug, Clone)]
pub struct CanonicalVdfSnapshot {
    pub vdf_info: VDFLimiterInfo,
    pub confirmed_global_step_number: u64,
}

/// A trait that is used to provide access to blocks by their hash. Used to avoid circular dependencies,
/// such as between VDF and the block index.
pub trait BlockProvider {
    fn latest_canonical_vdf_info(&self) -> Option<CanonicalVdfSnapshot>;
}
