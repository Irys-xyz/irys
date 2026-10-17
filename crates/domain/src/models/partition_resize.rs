//! Finalized, prefix-preserving storage migration. The journal precedes truncation;
//! reopening a module completes an interrupted migration before accepting writes.
use super::*;
use irys_types::partition_resize::PartitionResizeState;
use serde::{Deserialize, Serialize};

const RESIZE_JOURNAL: &str = "partition_resize.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ResizeJournal {
    version: u32,
    chain_id: u64,
    activation_height: u64,
    activation_hash: H256,
    old_chunks: u64,
    pub(super) retained_chunks: u64,
}

impl ResizeJournal {
    fn new(state: &PartitionResizeState, chain_id: u64) -> Self {
        Self {
            version: 1,
            chain_id,
            activation_height: state.activation_height,
            activation_hash: state.activation_hash,
            old_chunks: state.reference_partition_chunks,
            retained_chunks: state.params.partition_chunks,
        }
    }

    fn validate(&self, config: &Config) -> Result<()> {
        ensure!(
            self.version == 1,
            "unsupported partition resize journal version"
        );
        ensure!(
            self.chain_id == config.consensus.chain_id,
            "partition resize journal belongs to another chain"
        );
        ensure!(
            self.old_chunks == config.consensus.num_chunks_in_partition,
            "partition resize journal has different original geometry"
        );
        ensure!(
            self.activation_height > 0
                && self
                    .activation_height
                    .is_multiple_of(config.consensus.epoch.num_blocks_in_epoch),
            "invalid partition resize activation height"
        );
        ensure!(
            config
                .consensus
                .hardforks
                .partition_resize
                .is_some_and(|r| r.partition_chunks == self.retained_chunks),
            "storage resize journal does not match the configured partition-resize geometry"
        );
        Ok(())
    }
}

impl StorageModule {
    pub fn partition_chunks(&self) -> u64 {
        self.partition_chunks.load(Ordering::Acquire)
    }

    /// Logical geometry may roll back before finality; physical geometry may not.
    pub fn set_partition_chunks(&self, chunks: u64) -> Result<()> {
        ensure!(
            chunks > 0 && chunks <= self.config.consensus.num_chunks_in_partition,
            "invalid logical partition size"
        );
        ensure!(
            chunks <= self.retained_chunks.load(Ordering::Acquire),
            "cannot restore a partition suffix after finality"
        );
        self.partition_chunks.store(chunks, Ordering::Release);
        Ok(())
    }

    fn resize_journal(&self) -> Result<PathBuf> {
        let (_, submodule) = self
            .submodules
            .first_key_value()
            .ok_or_eyre("module has no submodules")?;
        Ok(self
            .config
            .node_config
            .base_directory
            .join(&submodule.path)
            .join(RESIZE_JOURNAL))
    }

    pub(super) fn read_resize_journal(
        info: &StorageModuleInfo,
        config: &Config,
    ) -> Result<Option<ResizeJournal>> {
        let (_, path) = info
            .submodules
            .iter()
            .min_by_key(|(range, _)| *range.start())
            .ok_or_eyre("module has no submodules")?;
        let path = config
            .node_config
            .base_directory
            .join(path)
            .join(RESIZE_JOURNAL);
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let journal: ResizeJournal = serde_json::from_slice(&bytes)?;
        journal.validate(config)?;
        Ok(Some(journal))
    }

    /// Caller must establish finality. The record binds every retry to that
    /// activation; recovery uses the same record committed before truncation.
    /// Returns whether this call completed a migration rather than finding it done.
    pub fn finalize_partition_resize(&self, state: &PartitionResizeState) -> Result<bool> {
        self.resume_partition_resize(ResizeJournal::new(state, self.config.consensus.chain_id))
    }

    pub(super) fn resume_partition_resize(&self, journal: ResizeJournal) -> Result<bool> {
        journal.validate(&self.config)?;
        let mut completed = self.resize_complete.lock().unwrap();
        if let Some(previous) = *completed {
            ensure!(
                previous == journal,
                "partition was resized for a different activation"
            );
            return Ok(false);
        }
        self.truncate_finalized_partition(journal)?;
        *completed = Some(journal);
        Ok(true)
    }

    /// Caller must establish that the activation block is finalized on this branch.
    fn truncate_finalized_partition(&self, record: ResizeJournal) -> Result<()> {
        let chunks = record.retained_chunks;
        ensure!(
            chunks > 0 && chunks <= self.retained_chunks.load(Ordering::Acquire),
            "invalid physical partition shrink"
        );
        // Serializes with flushes. Pending writers see the retained bound under
        // their own lock, so neither a queued batch nor an in-flight packing job
        // can regrow the suffix after the truncate.
        let _sync = self.sync_in_progress.lock().unwrap();
        let mut pending = self.pending_writes.write().unwrap();
        self.index_write_generation.fetch_add(1, Ordering::SeqCst);
        for (_, submodule) in self.submodules.iter() {
            submodule.index_drain.wait_idle();
        }
        let journal_path = self.resize_journal()?;
        match fs::read(&journal_path) {
            Ok(bytes) => ensure!(
                serde_json::from_slice::<ResizeJournal>(&bytes)? == record,
                "partition resize journal has a different activation"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let mut journal = get_atomic_file(journal_path)?;
        journal.write_all(&serde_json::to_vec(&record)?)?;
        journal.commit()?;
        self.retained_chunks.store(chunks, Ordering::Release);
        self.partition_chunks.store(chunks, Ordering::Release);
        pending
            .chunks
            .retain(|offset, _| u64::from(**offset) < chunks);
        pending.occupancy.clear();
        drop(pending);
        {
            let mut intervals = self.intervals.write().unwrap();
            let _ = intervals
                .cut(partition_chunk_offset_ii!(chunks as u32, u32::MAX))
                .collect::<Vec<_>>();
        }
        // Persist prefix status before cutting bytes. A crash at either point
        // is completed from the journal on reopen, without resetting packing.
        self.write_intervals_to_submodules()?;
        for (range, submodule) in self.submodules.iter() {
            let keep = chunks
                .saturating_sub(u64::from(*range.start()))
                .min(u64::from(*range.end()) - u64::from(*range.start()) + 1);
            let file = submodule.file.lock().unwrap();
            let length = keep
                .checked_mul(self.config.consensus.chunk_size)
                .ok_or_eyre("partition byte length overflow")?;
            // Sparse/partially packed files must not grow during a shrink.
            if file.metadata()?.len() > length {
                file.set_len(length)?;
            }
            file.sync_all()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{NodeConfig, UnixTimestamp, partition_resize::PartitionResize};

    #[test]
    fn partition_resize_preserves_prefix_and_resumes_interrupted_shrink() -> Result<()> {
        for interrupted in [false, true] {
            let dir = TempDirBuilder::new().prefix("partition_resize").build();
            let mut node = NodeConfig::testing();
            node.base_directory = dir.path().to_path_buf();
            let c = node.consensus.get_mut();
            c.chunk_size = 32;
            c.num_chunks_in_partition = 10;
            c.hardforks.partition_resize = Some(PartitionResize {
                activation_timestamp: UnixTimestamp::from_secs(0),
                partition_chunks: 4,
                recall_chunks: 1,
                target_ingress_bytes_per_second: 1,
                minimum_partition_ids: 0,
            });
            let config = Config::new_with_random_peer_id(node);
            let state = PartitionResizeState {
                params: config.consensus.hardforks.partition_resize.unwrap(),
                activation_height: config.consensus.epoch.num_blocks_in_epoch,
                activation_hash: H256::repeat_byte(7),
                sampling_origin: 2,
                preserved_id_floor: 3,
                reference_partition_chunks: 10,
            };
            let info = StorageModuleInfo {
                id: 0,
                partition_assignment: Some(irys_types::partition::PartitionAssignment {
                    partition_hash: H256::repeat_byte(1),
                    miner_address: config.node_config.miner_address(),
                    ledger_id: Some(DataLedger::Submit.into()),
                    slot_index: Some(1),
                }),
                submodules: vec![
                    (partition_chunk_offset_ii!(0, 2), "a".into()),
                    (partition_chunk_offset_ii!(3, 6), "b".into()),
                    (partition_chunk_offset_ii!(7, 9), "c".into()),
                ],
            };
            let module = StorageModule::new(&info, &config)?;
            module.set_partition_chunks(4)?;
            assert_eq!(module.reset()?, partition_chunk_offset_ii!(0, 3));
            module.set_partition_chunks(10)?;
            for offset in 0..10 {
                assert!(module.write_chunk(
                    PartitionChunkOffset(offset),
                    vec![offset as u8; 32],
                    ChunkType::Entropy
                ));
            }
            module.force_sync_pending_chunks()?;
            let packing = fs::read(dir.path().join("a").join(PACKING_PARAMS_FILE_NAME))?;
            module.set_partition_chunks(4)?;
            // Before finality a reorg can restore the full packed extent.
            module.set_partition_chunks(10)?;
            assert_eq!(
                module.read_chunks(partition_chunk_offset_ii!(9, 9))?.len(),
                1
            );
            if interrupted {
                // Simulate death after journal commit but before interval/file truncation.
                fs::write(
                    module.resize_journal()?,
                    serde_json::to_vec(&ResizeJournal::new(&state, config.consensus.chain_id))?,
                )?;
            } else {
                // A previously queued packing write cannot regrow the tail.
                assert!(module.write_chunk(
                    PartitionChunkOffset(9),
                    vec![99; 32],
                    ChunkType::Entropy
                ));
                module.finalize_partition_resize(&state)?;
                assert!(!module.write_chunk(
                    PartitionChunkOffset(9),
                    vec![99; 32],
                    ChunkType::Entropy
                ));
                module.force_sync_pending_chunks()?;
                assert!(module.set_partition_chunks(10).is_err());
            }
            drop(module);
            let reopened = StorageModule::new(&info, &config)?;
            reopened.finalize_partition_resize(&state)?;
            let mut other = state;
            other.activation_hash = H256::repeat_byte(8);
            assert!(reopened.finalize_partition_resize(&other).is_err());
            assert_eq!(reopened.partition_assignment(), info.partition_assignment);
            assert_eq!(reopened.partition_chunks(), 4);
            assert_eq!(
                reopened.get_intervals(ChunkType::Entropy),
                vec![partition_chunk_offset_ii!(0, 3)]
            );
            let chunks = reopened.read_chunks(partition_chunk_offset_ii!(0, 3))?;
            for offset in 0..4 {
                assert_eq!(
                    chunks.get(&PartitionChunkOffset(offset)).unwrap().0,
                    vec![offset as u8; 32]
                );
            }
            for (path, length) in [("a", 96), ("b", 32), ("c", 0)] {
                assert_eq!(
                    fs::metadata(dir.path().join(path).join("chunks.dat"))?.len(),
                    length
                );
            }
            assert_eq!(
                fs::read(dir.path().join("a").join(PACKING_PARAMS_FILE_NAME))?,
                packing
            );
            assert!(reopened.contains_offset(LedgerChunkOffset::from(4)));
            assert!(!reopened.contains_offset(LedgerChunkOffset::from(8)));
        }
        Ok(())
    }
}
