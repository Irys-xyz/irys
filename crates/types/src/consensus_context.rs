//! The consensus parameters for blocks mined against one epoch snapshot, behind
//! one interface.
//!
//! Everything that changes at the partition-resize activation — partition
//! geometry, the sampling origin, and the difficulty window — is resolved
//! **once**, from the parent epoch snapshot (`EpochSnapshot::consensus_context`),
//! and consensus code takes the result as a value. Callers never re-derive these
//! pieces from raw config fields (always the legacy geometry) or from ancestry
//! themselves, so producer, validator, and miner cannot disagree about which side
//! of the activation a block is on.
use crate::{
    AdjustmentStats, ConsensusConfig, DifficultyAdjustmentConfig, IrysBlockHeader, U256,
    UnixTimestamp, UnixTimestampMs, calculate_difficulty,
    hardfork_config::Estuary,
    next_cumulative_diff,
    partition_resize::{PartitionGeometry, PartitionResizeState},
};

/// Consensus context for the children of blocks governed by one epoch snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochConsensusContext {
    /// Partition-resize state on this branch, once activated.
    resize: Option<PartitionResizeState>,
    /// Fork parameters while configured but not yet activated on this branch:
    /// the next epoch block meeting the timestamp is activation block E.
    pending: Option<Estuary>,
    legacy_geometry: PartitionGeometry,
    num_blocks_in_epoch: u64,
    difficulty: DifficultyAdjustmentConfig,
}

impl EpochConsensusContext {
    /// `resize` is the activation recorded by the epoch snapshot this context
    /// is resolved from; `None` before activation on that branch.
    pub fn new(config: &ConsensusConfig, resize: Option<PartitionResizeState>) -> Self {
        Self {
            resize,
            pending: config.hardforks.estuary.filter(|_| resize.is_none()),
            legacy_geometry: PartitionGeometry::legacy(config),
            num_blocks_in_epoch: config.epoch.num_blocks_in_epoch,
            difficulty: config.difficulty_adjustment.clone(),
        }
    }

    /// Partition geometry for blocks mined against this snapshot. Activation
    /// block E itself is mined under the legacy geometry; its children under
    /// the resized one.
    pub fn geometry(&self) -> PartitionGeometry {
        self.resize
            .map_or(self.legacy_geometry, |state| PartitionGeometry {
                partition_chunks: state.params.partition_chunks,
                recall_chunks: state.params.recall_chunks,
                sampling_origin: state.sampling_origin,
            })
    }

    /// Partition-resize activation on this branch, once activated.
    pub fn resize(&self) -> Option<PartitionResizeState> {
        self.resize
    }

    /// Whether the Estuary fork is configured at all (activated on
    /// this branch or still pending).
    pub fn resize_configured(&self) -> bool {
        self.resize.is_some() || self.pending.is_some()
    }

    /// Whether the fork is scheduled but has not yet activated on this branch.
    /// Data ledgers must stay empty while this holds, so activation block E
    /// can never fail `NonEmptyLedgers`.
    pub fn pending_activation(&self) -> bool {
        self.pending.is_some()
    }

    /// Whether a block at `height` with `timestamp` is activation block E.
    pub fn activates_at(&self, height: u64, timestamp: UnixTimestamp) -> bool {
        self.pending.is_some_and(|resize| {
            resize.is_activation_block(self.num_blocks_in_epoch, height, timestamp)
        })
    }

    /// Difficulty for the block at `height`. E publishes the transition
    /// difficulty for its children (a block's solution is checked against its
    /// parent's `diff`): the normal old-regime adjustment if one is due, then
    /// the winning-hash scaling for the smaller recall range. After E,
    /// adjustments run on a window re-anchored at E.
    pub fn child_difficulty(
        &self,
        height: u64,
        timestamp: UnixTimestampMs,
        previous: &IrysBlockHeader,
    ) -> (U256, Option<AdjustmentStats>) {
        let (difficulty, stats) = calculate_difficulty(
            self.adjustment_height(height),
            previous.last_diff_timestamp,
            timestamp,
            previous.diff,
            &self.difficulty,
        );
        match self.pending {
            Some(resize) if self.activates_at(height, timestamp.to_secs()) => (
                resize.activation_difficulty(difficulty, self.legacy_geometry.recall_chunks),
                stats,
            ),
            _ => (difficulty, stats),
        }
    }

    /// Activation starts a complete new difficulty window even when E is not
    /// on the old adjustment schedule.
    pub fn child_last_diff_timestamp(
        &self,
        height: u64,
        timestamp: UnixTimestampMs,
        previous: &IrysBlockHeader,
    ) -> UnixTimestampMs {
        if self.activates_at(height, timestamp.to_secs())
            || self
                .adjustment_height(height)
                .is_multiple_of(self.difficulty.difficulty_adjustment_interval)
        {
            timestamp
        } else {
            previous.last_diff_timestamp
        }
    }

    /// Fork-choice weight of the block at `height` carrying `diff`.
    ///
    /// The legacy increment `MAX / (MAX - diff)` counts expected hashes per
    /// block, so a branch gains weight at its hash rate. The resize cuts the
    /// hashes per partition-step by `old_recall / new_recall` and only `diff`
    /// compensates, so uncorrected, a branch still mining under the old recall
    /// range would out-weigh an activated one by that factor at equal capacity.
    /// Blocks whose `diff` is a resized-regime target (E and its descendants)
    /// are therefore credited per partition-step. Factor 1 is the legacy
    /// formula, unchanged.
    pub fn child_cumulative_diff(
        &self,
        height: u64,
        timestamp: UnixTimestampMs,
        previous: &IrysBlockHeader,
        diff: U256,
    ) -> U256 {
        let resized_recall = match (self.resize, self.pending) {
            (Some(state), _) => Some(state.params.recall_chunks),
            (None, Some(resize)) if self.activates_at(height, timestamp.to_secs()) => {
                Some(resize.recall_chunks)
            }
            _ => None,
        };
        let factor = resized_recall.map_or(1, |recall| self.legacy_geometry.recall_chunks / recall);
        if factor == 1 {
            return next_cumulative_diff(previous.cumulative_diff, diff);
        }
        // `(factor * (MAX - D) + factor - 1) / factor == MAX - D`, so E weighs
        // exactly what its legacy sibling would.
        let target = ((U256::MAX - diff) / U256::from(factor)).max(U256::from(1));
        previous.cumulative_diff + U256::MAX / target
    }

    /// Height within the difficulty window: from genesis before activation,
    /// from E after it.
    fn adjustment_height(&self, height: u64) -> u64 {
        self.resize.map_or(height, |state| {
            debug_assert!(
                height >= state.activation_height,
                "height {height} precedes the resize activation at {}",
                state.activation_height
            );
            height.saturating_sub(state.activation_height)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::H256;

    fn config() -> ConsensusConfig {
        let mut config = ConsensusConfig::testing();
        config.num_chunks_in_partition = 100;
        config.num_chunks_in_recall_range = 10;
        config.epoch.num_blocks_in_epoch = 6;
        config.hardforks.estuary = Some(Estuary {
            partition_chunks: 40,
            recall_chunks: 2,
            target_ingress_bytes_per_second: 32,
            minimum_partition_ids: 0,
            ..Estuary::proposed(UnixTimestamp::from_secs(10))
        });
        config
    }

    #[test]
    fn pending_context_detects_e_and_scales_only_there() {
        let config = config();
        let ctx = EpochConsensusContext::new(&config, None);
        assert!(ctx.resize_configured());
        assert_eq!(ctx.geometry(), PartitionGeometry::legacy(&config));
        assert!(!ctx.activates_at(0, UnixTimestamp::from_secs(10)));
        assert!(!ctx.activates_at(5, UnixTimestamp::from_secs(10)));
        assert!(!ctx.activates_at(6, UnixTimestamp::from_secs(9)));
        assert!(ctx.activates_at(6, UnixTimestamp::from_secs(10)));
        assert!(ctx.activates_at(12, UnixTimestamp::from_secs(11)));

        let mut previous = IrysBlockHeader::default();
        previous.diff = U256::MAX - U256::from(999);
        let at_e = UnixTimestampMs::from_millis(10_000);
        let (scaled, _) = ctx.child_difficulty(6, at_e, &previous);
        let (unscaled, _) = ctx.child_difficulty(5, at_e, &previous);
        assert_eq!(unscaled, previous.diff);
        assert_eq!(U256::MAX - scaled + U256::from(1), U256::from(5_000));
        assert_eq!(ctx.child_last_diff_timestamp(6, at_e, &previous), at_e);
    }

    #[test]
    fn activated_context_never_rescales_and_uses_resized_geometry() {
        let config = config();
        let state = PartitionResizeState {
            params: config.hardforks.estuary.unwrap(),
            activation_height: 6,
            activation_hash: H256::repeat_byte(7),
            sampling_origin: 32,
            preserved_id_floor: 8,
            reference_partition_chunks: 100,
        };
        let ctx = EpochConsensusContext::new(&config, Some(state));
        assert!(ctx.resize_configured());
        assert_eq!(ctx.resize(), Some(state));
        assert_eq!(
            ctx.geometry(),
            PartitionGeometry {
                partition_chunks: 40,
                recall_chunks: 2,
                sampling_origin: 32,
            }
        );
        // A later epoch block past the timestamp is not a second E.
        assert!(!ctx.activates_at(12, UnixTimestamp::from_secs(100)));

        let unconfigured = ConsensusConfig::testing();
        let ctx = EpochConsensusContext::new(&unconfigured, None);
        assert!(!ctx.resize_configured());
        assert!(!ctx.pending_activation());
        assert!(!ctx.activates_at(6, UnixTimestamp::from_secs(u64::MAX)));
    }

    /// A legacy sibling of E, E itself, and E's children all add the weight of
    /// one legacy block, so fork choice stays commensurable across activation.
    #[test]
    fn activated_and_legacy_blocks_weigh_the_same() {
        let config = config(); // recall 10 -> 2: factor 5
        let pending = EpochConsensusContext::new(&config, None);
        assert!(pending.pending_activation());
        let mut parent = IrysBlockHeader::default();
        parent.height = 5;
        parent.diff = U256::MAX - U256::from(999);
        parent.cumulative_diff = U256::from(7);
        let legacy_weight = U256::MAX / U256::from(999);

        let before = UnixTimestampMs::from_millis(9_000);
        let (legacy_diff, _) = pending.child_difficulty(6, before, &parent);
        assert_eq!(legacy_diff, parent.diff);
        assert_eq!(
            pending.child_cumulative_diff(6, before, &parent, legacy_diff),
            parent.cumulative_diff + legacy_weight
        );

        let at_e = UnixTimestampMs::from_millis(10_000);
        let (scaled, _) = pending.child_difficulty(6, at_e, &parent);
        assert_eq!(U256::MAX - scaled, U256::from(4_999));
        assert_eq!(
            pending.child_cumulative_diff(6, at_e, &parent, scaled),
            parent.cumulative_diff + legacy_weight
        );

        let mut e = parent.clone();
        e.height = 6;
        e.diff = scaled;
        e.cumulative_diff = parent.cumulative_diff + legacy_weight;
        let activated = EpochConsensusContext::new(
            &config,
            Some(PartitionResizeState {
                params: config.hardforks.estuary.unwrap(),
                activation_height: 6,
                activation_hash: H256::repeat_byte(7),
                sampling_origin: 32,
                preserved_id_floor: 8,
                reference_partition_chunks: 100,
            }),
        );
        assert!(!activated.pending_activation());
        let at_seven = UnixTimestampMs::from_millis(11_000);
        let (child_diff, _) = activated.child_difficulty(7, at_seven, &e);
        assert_eq!(child_diff, scaled);
        assert_eq!(
            activated.child_cumulative_diff(7, at_seven, &e, child_diff),
            e.cumulative_diff + legacy_weight
        );

        // Without the fork configured the legacy formula is used as is.
        let unconfigured = EpochConsensusContext::new(&ConsensusConfig::testing(), None);
        assert_eq!(
            unconfigured.child_cumulative_diff(6, at_e, &parent, parent.diff),
            next_cumulative_diff(parent.cumulative_diff, parent.diff)
        );
    }
}
