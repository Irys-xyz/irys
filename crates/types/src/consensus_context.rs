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
        assert!(!ctx.activates_at(5, UnixTimestamp::from_secs(10)));
        assert!(!ctx.activates_at(6, UnixTimestamp::from_secs(9)));
        assert!(ctx.activates_at(6, UnixTimestamp::from_secs(10)));

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
        assert!(!ctx.activates_at(6, UnixTimestamp::from_secs(u64::MAX)));
    }
}
