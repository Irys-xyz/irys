//! Separates protocol allocation, replica assignments and this node's packing.
use crate::{ApiState, error::ApiError};
use actix_web::web::{Data, Json};
use irys_domain::{ChunkType, EpochSnapshot, StorageModule};
use irys_types::{H256, IrysBlockHeader, partition_resize::PartitionGeometry};
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalCapacity {
    partitions: usize,
    /// Observed entropy + data chunks, including pending writes, clipped to geometry.
    #[serde(with = "irys_types::serialization::string_u64")]
    packed_chunks: u64,
    /// Entropy on ledger-assigned modules accepting data writes. Capacity-pool
    /// entropy is packed but cannot accept ledger data until it is assigned.
    #[serde(with = "irys_types::serialization::string_u64")]
    packed_writable_chunks: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LedgerCapacity {
    ledger_id: u32,
    #[serde(with = "irys_types::serialization::string_u64")]
    reserved_chunks: u64,
    allocated_slots: usize,
    #[serde(with = "irys_types::serialization::string_u64")]
    allocated_headroom_chunks: u64,
    #[serde(with = "irys_types::serialization::string_u64")]
    required_replicas: u64,
    #[serde(with = "irys_types::serialization::string_u64")]
    assigned_replicas: u64,
    fully_assigned_slots: usize,
    local: LocalCapacity,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageCapacityResponse {
    #[serde(with = "irys_types::serialization::string_u64")]
    chain_height: u64,
    #[serde(with = "irys_types::serialization::string_u64")]
    epoch_height: u64,
    epoch_hash: H256,
    geometry: PartitionGeometry,
    #[serde(with = "irys_types::serialization::string_u64")]
    chunk_bytes: u64,
    #[serde(with = "irys_types::serialization::optional_string_u64")]
    target_ingress_bytes_per_second: Option<u64>,
    #[serde(with = "irys_types::serialization::optional_string_u64")]
    reserve_chunks_per_epoch: Option<u64>,
    total_partition_ids: usize,
    ledgers: Vec<LedgerCapacity>,
    local: LocalCapacity,
}

fn capacity_report(
    epoch: &EpochSnapshot,
    tip: &IrysBlockHeader,
    modules: &[Arc<StorageModule>],
) -> eyre::Result<StorageCapacityResponse> {
    let geometry = epoch.consensus_context().geometry();
    let mut ledgers = Vec::new();
    for ledger in epoch.ledgers.active_ledgers() {
        let meta = epoch
            .ledgers
            .ledger_meta_for(ledger)
            .expect("active ledger");
        let slots = epoch.ledgers.get_slots(ledger);
        let active: Vec<_> = slots.iter().filter(|s| !s.is_expired).collect();
        let assigned_replicas = active.iter().map(|s| s.partitions.len() as u64).sum();
        let required_replicas = (active.len() as u64)
            .checked_mul(meta.num_partitions_per_slot)
            .ok_or_else(|| eyre::eyre!("replica count overflow"))?;
        let reserved_chunks = tip.ledger_total_chunks(ledger);
        let allocated_chunks = (slots.len() as u64)
            .checked_mul(geometry.partition_chunks)
            .ok_or_else(|| eyre::eyre!("allocated capacity overflow"))?;
        ledgers.push(LedgerCapacity {
            ledger_id: ledger as u32,
            reserved_chunks,
            allocated_slots: slots.len(),
            allocated_headroom_chunks: allocated_chunks.saturating_sub(reserved_chunks),
            assigned_replicas,
            required_replicas,
            fully_assigned_slots: active
                .iter()
                .filter(|s| s.partitions.len() as u64 >= meta.num_partitions_per_slot)
                .count(),
            local: LocalCapacity::default(),
        });
    }
    let mut local = LocalCapacity::default();
    for module in modules {
        let Some(assignment) = module.partition_assignment() else {
            continue;
        };
        // Local assignments can lag an epoch/reorg. Count only those matching
        // the captured canonical snapshot; never turn them into network readiness.
        if epoch.get_data_partition_assignment(assignment.partition_hash) != Some(assignment) {
            continue;
        }
        let count = |kind| -> u64 {
            module
                .get_intervals(kind)
                .iter()
                .map(|range| {
                    let start = u64::from(*range.start());
                    let end = (u64::from(*range.end()) + 1).min(geometry.partition_chunks);
                    end.saturating_sub(start)
                })
                .sum()
        };
        let entropy = count(ChunkType::Entropy).min(geometry.partition_chunks);
        let packed = (entropy + count(ChunkType::Data)).min(geometry.partition_chunks);
        let writable = if assignment.ledger_id.is_some() && !module.data_writes_paused() {
            entropy
        } else {
            0
        };
        local.partitions += 1;
        local.packed_chunks += packed;
        local.packed_writable_chunks += writable;
        if let Some(ledger) = ledgers
            .iter_mut()
            .find(|l| Some(l.ledger_id) == assignment.ledger_id)
        {
            ledger.local.partitions += 1;
            ledger.local.packed_chunks += packed;
            ledger.local.packed_writable_chunks += writable;
        }
    }
    Ok(StorageCapacityResponse {
        chain_height: tip.height,
        epoch_height: epoch.epoch_height,
        epoch_hash: epoch.epoch_block.block_hash,
        geometry,
        chunk_bytes: epoch.config.consensus.chunk_size,
        target_ingress_bytes_per_second: epoch
            .partition_resize
            .map(|r| r.params.target_ingress_bytes_per_second),
        reserve_chunks_per_epoch: epoch
            .partition_resize
            .map(|r| r.params.reserve_chunks(&epoch.config.consensus))
            .transpose()?,
        total_partition_ids: epoch.all_active_partitions.len(),
        ledgers,
        local,
    })
}

pub async fn get_capacity(
    state: Data<ApiState>,
) -> Result<Json<StorageCapacityResponse>, ApiError> {
    let (epoch, tip) = {
        let tree = state.block_tree.read();
        (
            tree.canonical_epoch_snapshot(),
            tree.get_latest_canonical_entry().header().clone(),
        )
    };
    let modules = state.chunk_provider.storage_modules_guard.read().clone();
    // Interval aggregation may be substantial on a fragmented disk. No chain or
    // registry lock is held while sampling it on the blocking pool.
    let report = tokio::task::spawn_blocking(move || capacity_report(&epoch, &tip, &modules))
        .await
        .map_err(|e| ApiError::Internal { err: e.to_string() })?
        .map_err(|e| ApiError::Internal { err: e.to_string() })?;
    Ok(Json(report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{
        Config, DataLedger, NodeConfig, PartitionChunkOffset, partition::PartitionAssignment,
        partition_chunk_offset_ie,
    };

    #[test]
    fn partition_resize_capacity_reports_allocation_assignment_and_local_packing_separately()
    -> eyre::Result<()> {
        let dir = TempDirBuilder::new().prefix("capacity_report").build();
        let mut node = NodeConfig::testing();
        node.base_directory = dir.path().into();
        node.consensus.get_mut().num_chunks_in_partition = 10;
        let config = Config::new_with_random_peer_id(node);
        let mut epoch = EpochSnapshot {
            config: config.clone(),
            ..Default::default()
        };
        epoch.ledgers[DataLedger::Submit].allocate_slots(2, 0);
        let assignment = PartitionAssignment {
            partition_hash: H256::repeat_byte(1),
            miner_address: config.node_config.miner_address(),
            ledger_id: Some(DataLedger::Submit as u32),
            slot_index: Some(0),
        };
        epoch
            .partition_assignments
            .data_partitions
            .insert(assignment.partition_hash, assignment);
        epoch
            .ledgers
            .push_partition_to_slot(DataLedger::Submit, 0, assignment.partition_hash);
        let module = Arc::new(StorageModule::new(
            &irys_domain::StorageModuleInfo {
                id: 0,
                partition_assignment: Some(assignment),
                submodules: vec![(partition_chunk_offset_ie!(0, 10), "disk".into())],
            },
            &config,
        )?);
        for offset in 0..3 {
            module.write_chunk(
                offset.into(),
                vec![0; config.consensus.chunk_size as usize],
                ChunkType::Entropy,
            );
        }
        let tip = IrysBlockHeader::default();
        let report = capacity_report(&epoch, &tip, std::slice::from_ref(&module))?;
        let ledger = report
            .ledgers
            .iter()
            .find(|l| l.ledger_id == DataLedger::Submit as u32)
            .unwrap();
        assert_eq!(ledger.allocated_slots, 2);
        assert_eq!(ledger.allocated_headroom_chunks, 20);
        assert_eq!(ledger.assigned_replicas, 1);
        assert_eq!(ledger.local.packed_chunks, 3);
        assert_eq!(ledger.local.packed_writable_chunks, 3);
        assert_eq!(report.target_ingress_bytes_per_second, None);
        // An assignment from an old branch must not inflate current readiness.
        epoch.partition_assignments.data_partitions.clear();
        assert_eq!(
            capacity_report(&epoch, &tip, &[module])?
                .local
                .packed_chunks,
            0
        );
        Ok(())
    }
}
