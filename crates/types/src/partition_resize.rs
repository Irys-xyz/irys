//! Estuary hardfork: partition-resize geometry and deterministic capacity arithmetic.
//! These rules are resolved from the parent epoch snapshot, never wall-clock time.
use crate::{
    ConsensusConfig, H256, IrysBlockHeader, U256, UnixTimestamp,
    serialization::unix_timestamp_string_serde,
};
use eyre::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Why a partition-resize epoch transition is invalid. Consensus errors: an
/// epoch block hitting one of these is rejected, never a node panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PartitionResizeError {
    /// The rollout assumes existing partitions hold entropy only; activating
    /// under live ledger data would orphan reserved offsets above the new size.
    #[error("partition resize requires empty data ledgers at activation")]
    NonEmptyLedgers,
    /// Checked arithmetic overflowed. Counts are config-bounded, so reaching
    /// this means a misconfiguration.
    #[error("partition resize arithmetic overflow: {0}")]
    Overflow(&'static str),
    #[error("chunk size must be nonzero")]
    ZeroChunkSize,
}

/// The geometry for children of a particular epoch snapshot. Network/genesis
/// configuration stays immutable; callers resolve this once from their ancestry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartitionGeometry {
    #[serde(with = "crate::serialization::string_u64")]
    pub partition_chunks: u64,
    #[serde(with = "crate::serialization::string_u64")]
    pub recall_chunks: u64,
    #[serde(with = "crate::serialization::string_u64")]
    pub sampling_origin: u64,
}

impl PartitionGeometry {
    pub fn legacy(config: &ConsensusConfig) -> Self {
        Self {
            partition_chunks: config.num_chunks_in_partition,
            recall_chunks: config.num_chunks_in_recall_range,
            sampling_origin: 1,
        }
    }

    pub fn recall_ranges(self) -> u64 {
        self.partition_chunks.div_ceil(self.recall_chunks)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Estuary {
    /// Timestamp (seconds since epoch) at which this hardfork activates. Like
    /// Cascade, activation block E is the first positive-height epoch block
    /// whose timestamp meets or exceeds it.
    #[serde(with = "unix_timestamp_string_serde")]
    pub activation_timestamp: UnixTimestamp,
    pub partition_chunks: u64,
    pub recall_chunks: u64,
    pub target_ingress_bytes_per_second: u64,
    #[serde(default)]
    pub minimum_partition_ids: u64,
}

impl Estuary {
    /// The proposed mainnet parameters, activating at `activation_timestamp`.
    /// There is deliberately no `Default`: an implicit activation time would be
    /// a consensus footgun.
    pub fn proposed(activation_timestamp: UnixTimestamp) -> Self {
        Self {
            activation_timestamp,
            partition_chunks: 14_500_000,
            recall_chunks: 80,
            target_ingress_bytes_per_second: 20_000_000,
            minimum_partition_ids: 0,
        }
    }

    pub fn validate(&self, config: &ConsensusConfig) -> Result<()> {
        ensure!(
            self.partition_chunks > 0 && self.partition_chunks <= config.num_chunks_in_partition,
            "partition resize must retain a nonempty prefix of the old partition"
        );
        ensure!(
            self.partition_chunks <= u32::MAX as u64,
            "partition offsets must fit u32"
        );
        ensure!(
            self.recall_chunks > 0 && self.partition_chunks.is_multiple_of(self.recall_chunks),
            "resized partition size must be divisible by its nonzero recall size"
        );
        ensure!(
            config
                .num_chunks_in_recall_range
                .is_multiple_of(self.recall_chunks),
            "partition resize recall reduction must be an integer factor"
        );
        ensure!(
            self.recall_chunks <= config.num_chunks_in_recall_range,
            "partition resize cannot increase the recall size"
        );
        ensure!(
            config.vdf.max_allowed_vdf_fork_steps
                >= self.partition_chunks.div_ceil(self.recall_chunks),
            "VDF cache is too small for resized sampling"
        );
        ensure!(
            config.epoch.num_blocks_in_epoch > 0
                && config.difficulty_adjustment.difficulty_adjustment_interval > 0,
            "partition resize requires nonzero epoch and difficulty intervals"
        );
        ensure!(
            self.target_ingress_bytes_per_second > 0,
            "partition resize ingress target must be nonzero"
        );
        self.reserve_chunks(config)?;
        Ok(())
    }

    /// Whether an epoch block at `height` with `timestamp` meets this fork's
    /// activation timestamp. E is the first such block on a branch; callers
    /// track whether the fork already activated.
    pub fn is_activation_block(
        &self,
        num_blocks_in_epoch: u64,
        height: u64,
        timestamp: UnixTimestamp,
    ) -> bool {
        height > 0
            && height.is_multiple_of(num_blocks_in_epoch)
            && timestamp >= self.activation_timestamp
    }

    pub fn reserve_chunks(&self, config: &ConsensusConfig) -> Result<u64, PartitionResizeError> {
        let bytes = u128::from(self.target_ingress_bytes_per_second)
            .checked_mul(u128::from(config.epoch.num_blocks_in_epoch))
            .and_then(|n| n.checked_mul(u128::from(config.difficulty_adjustment.block_time)))
            .ok_or(PartitionResizeError::Overflow("ingress reserve"))?;
        // `Config::validate` rejects `chunk_size == 0` before this can run.
        u64::try_from(bytes.div_ceil(u128::from(config.chunk_size)))
            .map_err(|_| PartitionResizeError::Overflow("ingress reserve"))
    }

    pub fn additional_slots(
        &self,
        reserved_chunks: u64,
        allocated_slots: u64,
        config: &ConsensusConfig,
    ) -> Result<u64, PartitionResizeError> {
        let required = (u128::from(reserved_chunks) + u128::from(self.reserve_chunks(config)?))
            .div_ceil(u128::from(self.partition_chunks));
        u64::try_from(required)
            .map(|required| required.saturating_sub(allocated_slots))
            .map_err(|_| PartitionResizeError::Overflow("slot requirement"))
    }

    pub fn preserved_id_floor(
        &self,
        count: u64,
        old_partition_chunks: u64,
    ) -> Result<u64, PartitionResizeError> {
        u64::try_from(
            (u128::from(count) * u128::from(old_partition_chunks))
                .div_ceil(u128::from(self.partition_chunks)),
        )
        .map_err(|_| PartitionResizeError::Overflow("partition-ID floor"))
    }

    /// Mining accepts hash >= difficulty: there are MAX - difficulty + 1 winners.
    /// Widen that inclusive set, saturating at the entire 256-bit hash space.
    pub fn activation_difficulty(&self, difficulty: U256, old_recall_chunks: u64) -> U256 {
        let factor = U256::from(old_recall_chunks / self.recall_chunks);
        (U256::MAX - difficulty)
            .checked_add(U256::from(1))
            .and_then(|winners| winners.checked_mul(factor))
            .map_or(U256::zero(), |winners| {
                U256::MAX - (winners - U256::from(1))
            })
    }
}

/// Rebuilt with each branch's epoch snapshots, including during cold replay.
/// Keeping the original inventory floor prevents repeated expansion on later epochs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartitionResizeState {
    pub params: Estuary,
    pub activation_height: u64,
    pub activation_hash: H256,
    pub sampling_origin: u64,
    pub preserved_id_floor: u64,
    pub reference_partition_chunks: u64,
}

impl PartitionResizeState {
    pub fn activate(
        params: Estuary,
        config: &ConsensusConfig,
        block: &IrysBlockHeader,
        inventory: u64,
    ) -> Result<Self, PartitionResizeError> {
        if block.data_ledgers.iter().any(|l| l.total_chunks != 0) {
            return Err(PartitionResizeError::NonEmptyLedgers);
        }
        Ok(Self {
            params,
            activation_height: block.height,
            activation_hash: block.block_hash,
            sampling_origin: block
                .vdf_limiter_info
                .global_step_number
                .checked_add(1)
                .ok_or(PartitionResizeError::Overflow("sampling origin"))?,
            preserved_id_floor: params
                .preserved_id_floor(inventory, config.num_chunks_in_partition)?,
            reference_partition_chunks: config.num_chunks_in_partition,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainnet_partition_resize_arithmetic() -> Result<()> {
        let config = ConsensusConfig::mainnet();
        let resize = Estuary::proposed(UnixTimestamp::from_secs(0));
        resize.validate(&config)?;
        assert_eq!(
            resize.partition_chunks * config.chunk_size,
            3_801_088_000_000
        );
        assert_eq!(resize.partition_chunks / resize.recall_chunks, 181_250);
        assert_eq!(resize.reserve_chunks(&config)?, 6_591_797);
        assert_eq!(
            resize.preserved_id_floor(1000, config.num_chunks_in_partition)?,
            5210
        );
        assert_eq!(resize.additional_slots(0, 0, &config)?, 1);
        let reserve = resize.reserve_chunks(&config)?;
        assert_eq!(
            resize.additional_slots(resize.partition_chunks - reserve, 1, &config)?,
            0
        );
        assert_eq!(
            resize.additional_slots(resize.partition_chunks - reserve + 1, 1, &config)?,
            1
        );
        // Aggregate reservations only; no transaction crosses a slot in these tests.
        let reserved = 500_000_000_000_000_u64.div_ceil(config.chunk_size);
        assert_eq!(resize.additional_slots(reserved, 0, &config)?, 132);
        assert_eq!(resize.additional_slots(reserved, 200, &config)?, 0);
        Ok(())
    }

    /// Scheduled independently of Delta: Delta alone never activates Estuary.
    #[test]
    fn delta_alone_never_activates_estuary() {
        let mut config = ConsensusConfig::testing();
        config.epoch.num_blocks_in_epoch = 6;
        config.hardforks.estuary = None;
        config.hardforks.delta = Some(crate::hardfork_config::Delta {
            activation_timestamp: UnixTimestamp::from_secs(0),
            initial_slots_per_new_ledger:
                crate::hardfork_config::Delta::default_initial_slots_per_new_ledger(),
        });
        let context = crate::consensus_context::EpochConsensusContext::new(&config, None);
        assert!(!context.resize_configured());
        assert!(!context.activates_at(12, UnixTimestamp::from_secs(100)));
    }

    #[test]
    fn partition_resize_difficulty_scales_inclusive_winners() {
        let resize = Estuary::proposed(UnixTimestamp::from_secs(0));
        for winners in [1_u64, 2, 100, 1_000_000] {
            let diff = U256::MAX - U256::from(winners - 1);
            let new = resize.activation_difficulty(diff, 400);
            assert_eq!(U256::MAX - new + U256::from(1), U256::from(winners * 5));
        }
        for diff in [U256::zero(), U256::from(1), U256::MAX / U256::from(2)] {
            assert_eq!(resize.activation_difficulty(diff, 400), U256::zero());
        }
    }

    #[test]
    fn partition_resize_rejects_invalid_geometry_and_overflow() {
        let config = ConsensusConfig::mainnet();
        for resize in [
            Estuary {
                recall_chunks: 0,
                ..Estuary::proposed(UnixTimestamp::from_secs(0))
            },
            Estuary {
                partition_chunks: 1,
                ..Estuary::proposed(UnixTimestamp::from_secs(0))
            },
            Estuary {
                target_ingress_bytes_per_second: 0,
                ..Estuary::proposed(UnixTimestamp::from_secs(0))
            },
        ] {
            assert!(resize.validate(&config).is_err());
        }
        let mut huge = config;
        huge.epoch.num_blocks_in_epoch = u64::MAX;
        huge.difficulty_adjustment.block_time = u64::MAX;
        assert!(
            Estuary::proposed(UnixTimestamp::from_secs(0))
                .reserve_chunks(&huge)
                .is_err()
        );
    }
}
