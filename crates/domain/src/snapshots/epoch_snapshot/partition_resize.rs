//! Partition-resize transition kept beside the epoch allocation paths it replaces.
use super::{EpochSnapshot, EpochSnapshotError};
use irys_types::{
    IrysBlockHeader,
    consensus_context::EpochConsensusContext,
    partition_resize::{PartitionGeometry, PartitionResizeError, PartitionResizeState},
};
use num_bigfloat::BigFloat;

impl EpochSnapshot {
    /// The single resolution point for the geometry, sampling origin, and
    /// difficulty window of blocks mined against this snapshot. Consensus code
    /// takes the returned context rather than re-deriving any of them.
    pub fn consensus_context(&self) -> EpochConsensusContext {
        EpochConsensusContext::new(&self.config.consensus, self.partition_resize)
    }

    /// Partition geometry for blocks mined against this snapshot.
    pub fn geometry(&self) -> PartitionGeometry {
        self.consensus_context().geometry()
    }

    /// Partition size of the geometry above.
    pub fn partition_chunks(&self) -> u64 {
        self.geometry().partition_chunks
    }

    pub(super) fn activate_partition_resize(
        &mut self,
        block: &IrysBlockHeader,
    ) -> Result<(), EpochSnapshotError> {
        if !self
            .consensus_context()
            .activates_at(block.height, block.timestamp_secs())
        {
            return Ok(());
        }
        let params = self
            .config
            .consensus
            .hardforks
            .estuary
            .expect("activation requires resize parameters");
        let state = PartitionResizeState::activate(
            params,
            &self.config.consensus,
            block,
            self.all_active_partitions.len() as u64,
        )?;
        self.partition_resize = Some(state);
        Ok(())
    }

    pub(super) fn allocate_resize_slots(
        &mut self,
        block: &IrysBlockHeader,
    ) -> Result<(), EpochSnapshotError> {
        let params = self.partition_resize.expect("active resize").params;
        for ledger in self.ledgers.active_ledgers() {
            // A ledger first activated here is absent from this header and has no reservations yet.
            let reserved = block.ledger_total_chunks(ledger);
            let allocated = self.ledgers[ledger].slot_count() as u64;
            let additional =
                params.additional_slots(reserved, allocated, &self.config.consensus)?;
            self.ledgers[ledger].allocate_slots(additional, block.height);
        }
        Ok(())
    }

    /// Evaluate the legacy reserve policy in original-partition storage units.
    /// Its result is an original-size reserve budget, converted to current IDs once.
    pub fn resize_capacity_reserve(
        &self,
        data_partitions: u64,
    ) -> Result<u64, PartitionResizeError> {
        let state = self.partition_resize.expect("active resize");
        let config = &self.config.consensus;
        let reference_demand = BigFloat::from_u64(data_partitions)
            * BigFloat::from_u64(state.params.partition_chunks)
            / BigFloat::from_u64(state.reference_partition_chunks);
        let min_count = (self.ledgers.active_ledgers().len() as u64)
            .checked_mul(config.num_partitions_per_slot)
            .ok_or(PartitionResizeError::Overflow("capacity minimum"))?;
        let budget = Self::capacity_partitions_for_demand(reference_demand, min_count, config)
            .ok_or(PartitionResizeError::Overflow("capacity reserve"))?;
        state
            .params
            .preserved_id_floor(budget, state.reference_partition_chunks)
    }

    pub(super) fn allocate_resize_capacity(&mut self) -> Result<(), EpochSnapshotError> {
        let state = self.partition_resize.expect("active resize");
        let data = self.partition_assignments.data_partitions.len() as u64;
        let policy_total = data
            .checked_add(self.resize_capacity_reserve(data)?)
            .ok_or(PartitionResizeError::Overflow("capacity total"))?;
        let required = policy_total
            .max(state.preserved_id_floor)
            .max(state.params.minimum_partition_ids);
        let additional = required.saturating_sub(self.all_active_partitions.len() as u64);
        self.add_capacity_partitions(additional);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eyre::Result;
    use irys_types::{
        Config, DataLedger, H256, NodeConfig, UnixTimestamp, UnixTimestampMs,
        hardfork_config::Estuary,
    };

    fn parent() -> EpochSnapshot {
        let mut node = NodeConfig::testing();
        let c = node.consensus.get_mut();
        c.num_chunks_in_partition = 100;
        c.num_chunks_in_recall_range = 10;
        c.epoch.num_blocks_in_epoch = 6;
        c.difficulty_adjustment.difficulty_adjustment_interval = 4;
        c.difficulty_adjustment.block_time = 1;
        c.epoch.capacity_scalar = 0; // Isolate the one-time baseline from the reserve policy.
        c.hardforks.estuary = Some(Estuary {
            activation_timestamp: UnixTimestamp::from_secs(10),
            partition_chunks: 40,
            recall_chunks: 2,
            target_ingress_bytes_per_second: 32,
            minimum_partition_ids: 0,
        });
        let mut snapshot = EpochSnapshot {
            config: Config::new_with_random_peer_id(node),
            ..Default::default()
        };
        snapshot.add_capacity_partitions(3);
        snapshot
    }

    fn activation() -> IrysBlockHeader {
        let mut block = IrysBlockHeader::default();
        block.height = 12;
        block.timestamp = UnixTimestampMs::from_millis(10_000);
        block.block_hash = H256::repeat_byte(7);
        block.data_ledgers = vec![irys_types::DataTransactionLedger {
            ledger_id: DataLedger::Submit as u32,
            ..Default::default()
        }];
        block.vdf_limiter_info.global_step_number = 31;
        block
    }

    #[test]
    fn partition_resize_branch_baseline_replay_and_later_epochs() -> Result<()> {
        let parent = parent();
        let block = activation();
        let mut first = parent.clone();
        first.activate_partition_resize(&block)?;
        first.allocate_resize_capacity()?;
        assert_eq!(first.all_active_partitions.len(), 8); // ceil(3 * 100 / 40)
        assert_eq!(
            &first.all_active_partitions[..3],
            parent.all_active_partitions.as_slice()
        );
        assert_eq!(first.partition_resize.unwrap().sampling_origin, 32);
        assert_eq!(first.consensus_context().geometry().partition_chunks, 40);
        assert_eq!(first.consensus_context().geometry().recall_chunks, 2);
        assert_eq!(first.config.consensus.num_chunks_in_partition, 100);
        assert_eq!(
            first
                .config
                .node_config
                .consensus_config()
                .num_chunks_in_partition,
            100
        );
        let mut replay = parent.clone();
        replay.activate_partition_resize(&block)?;
        replay.allocate_resize_capacity()?;
        assert_eq!(first.all_active_partitions, replay.all_active_partitions);
        let mut later = block.clone();
        later.height = 24;
        first.activate_partition_resize(&later)?;
        first.allocate_resize_capacity()?;
        assert_eq!(first.all_active_partitions, replay.all_active_partitions);
        // A reorg selects its own parent inventory and sampling origin.
        let mut other_branch = parent.clone();
        other_branch.add_capacity_partitions(2);
        let mut other_block = block;
        other_block.block_hash = H256::repeat_byte(8);
        other_block.vdf_limiter_info.global_step_number = 50;
        other_branch.activate_partition_resize(&other_block)?;
        other_branch.allocate_resize_capacity()?;
        assert_eq!(other_branch.all_active_partitions.len(), 13);
        assert_eq!(other_branch.partition_resize.unwrap().sampling_origin, 51);
        assert!(parent.partition_resize.is_none());
        assert_eq!(parent.config.consensus.num_chunks_in_partition, 100);
        Ok(())
    }

    #[test]
    fn partition_resize_reanchors_difficulty_at_timestamp_gated_epoch() -> Result<()> {
        let mut snapshot = parent(); // epochs 6, difficulty interval 4, timestamp 10s
        let mut previous = IrysBlockHeader::default();
        previous.height = 5;
        previous.diff = irys_types::U256::MAX - irys_types::U256::from(999);
        previous.last_diff_timestamp = UnixTimestampMs::from_millis(8_000);
        let mut block = activation();
        block.height = 6;
        // Crossing the timestamp in a non-epoch block does not activate.
        assert!(
            !snapshot
                .consensus_context()
                .activates_at(5, block.timestamp_secs())
        );
        assert!(
            !snapshot
                .consensus_context()
                .activates_at(6, UnixTimestamp::from_secs(9))
        );
        let (difficulty, stats) =
            snapshot
                .consensus_context()
                .child_difficulty(6, block.timestamp, &previous);
        assert!(stats.is_none()); // no partial old-window adjustment
        assert_eq!(
            difficulty,
            snapshot
                .config
                .consensus
                .hardforks
                .estuary
                .unwrap()
                .activation_difficulty(previous.diff, 10)
        );
        block.diff = difficulty;
        block.last_diff_timestamp =
            snapshot
                .consensus_context()
                .child_last_diff_timestamp(6, block.timestamp, &previous);
        assert_eq!(block.last_diff_timestamp, block.timestamp);
        snapshot.activate_partition_resize(&block)?;
        let at_eight = UnixTimestampMs::from_millis(12_000);
        assert!(
            snapshot
                .consensus_context()
                .child_difficulty(8, at_eight, &block)
                .1
                .is_none()
        );
        assert_eq!(
            snapshot
                .consensus_context()
                .child_last_diff_timestamp(8, at_eight, &block),
            block.timestamp
        );
        let at_ten = UnixTimestampMs::from_millis(14_000);
        let (next_diff, stats) = snapshot
            .consensus_context()
            .child_difficulty(10, at_ten, &block);
        assert!(stats.is_some());
        assert_eq!(next_diff, block.diff); // four blocks in four target seconds
        assert_eq!(
            snapshot
                .consensus_context()
                .child_last_diff_timestamp(10, at_ten, &block),
            at_ten
        );
        Ok(())
    }

    #[test]
    fn partition_resize_reservations_and_new_ledgers_need_no_assignees() -> Result<()> {
        let mut snapshot = parent();
        let mut block = activation();
        snapshot.activate_partition_resize(&block)?;
        snapshot.allocate_resize_slots(&block)?;
        assert_eq!(snapshot.ledgers[DataLedger::Submit].slot_count(), 1);
        // Synthetic cumulative reservation frontier, not a transaction fixture.
        block.data_ledgers[DataLedger::Submit].total_chunks = 400;
        snapshot.allocate_resize_slots(&block)?;
        assert_eq!(snapshot.ledgers[DataLedger::Submit].slot_count(), 11);
        snapshot.allocate_resize_slots(&block)?;
        assert_eq!(snapshot.ledgers[DataLedger::Submit].slot_count(), 11);
        block.height = 100_000;
        snapshot.expire_ledger_slots(&block, irys_types::hardfork_config::ForBlock::inactive());
        assert!(
            snapshot
                .ledgers
                .get_slots(DataLedger::Submit)
                .iter()
                .any(|s| s.is_expired)
        );
        snapshot.allocate_resize_slots(&block)?;
        assert_eq!(snapshot.ledgers[DataLedger::Submit].slot_count(), 11);
        // Newly activated Cascade ledgers have no header yet but receive a target reserve.
        snapshot
            .ledgers
            .activate_cascade(&snapshot.config.consensus);
        snapshot.allocate_resize_slots(&block)?;
        assert_eq!(snapshot.ledgers[DataLedger::OneYear].slot_count(), 1);
        assert_eq!(snapshot.ledgers[DataLedger::ThirtyDay].slot_count(), 1);
        snapshot.backfill_missing_partitions();
        assert!(
            snapshot
                .ledgers
                .get_slots(DataLedger::Submit)
                .iter()
                .all(|s| s.partitions.is_empty())
        );
        Ok(())
    }

    #[test]
    fn partition_resize_requires_empty_ledgers_and_honors_higher_floor() -> Result<()> {
        let mut snapshot = parent();
        let mut block = activation();
        block.data_ledgers[DataLedger::Submit].total_chunks = 1;
        assert!(matches!(
            snapshot.activate_partition_resize(&block),
            Err(EpochSnapshotError::PartitionResize(
                PartitionResizeError::NonEmptyLedgers
            ))
        ));
        assert!(snapshot.partition_resize.is_none());
        block.data_ledgers[DataLedger::Submit].total_chunks = 0;
        snapshot.activate_partition_resize(&block)?;
        snapshot
            .partition_resize
            .as_mut()
            .unwrap()
            .params
            .minimum_partition_ids = 20;
        snapshot.allocate_resize_capacity()?;
        assert_eq!(snapshot.all_active_partitions.len(), 20);
        Ok(())
    }

    #[test]
    fn partition_resize_reserve_preserves_equivalent_storage_budget() -> Result<()> {
        let mut snapshot = parent();
        let mut node = snapshot.config.node_config.clone();
        node.consensus.get_mut().epoch.capacity_scalar = 100;
        snapshot.config = Config::new_with_random_peer_id(node);
        let original = snapshot.config.consensus.clone();
        snapshot.activate_partition_resize(&activation())?;
        for old_data in [0, 2, 20, 200] {
            let budget = EpochSnapshot::get_num_capacity_partitions(
                old_data,
                &original,
                snapshot.ledgers.active_ledgers().len() as u64,
            ) * 100;
            let new_reserve = snapshot.resize_capacity_reserve(old_data * 5 / 2)? * 40;
            assert!(new_reserve >= budget && new_reserve - budget < 40);
        }
        Ok(())
    }
}
