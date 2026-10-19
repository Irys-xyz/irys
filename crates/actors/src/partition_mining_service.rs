use crate::{
    block_producer::BlockProducerCommand,
    metrics,
    mining_bus::{
        BroadcastDifficultyUpdate, BroadcastMiningSeed, BroadcastPartitionsExpiration,
        MiningBroadcastEvent,
    },
    packing_service::PackingRequest,
    services::ServiceSenders,
};
use eyre::WrapErr as _;
use irys_domain::{ChunkType, RecallChunks, StorageModule};
use irys_efficient_sampling::{Ranges, num_recall_ranges_in_partition};
use irys_packing::{capacity_single::compute_entropy_chunk, xor_vec_u8_arrays_in_place};
use irys_storage::ii;
use irys_types::{
    Base64, ChunkDataPath, Config, DataLedger, H256List, PartitionChunkOffset, PartitionChunkRange,
    ProofDeserialize as _, SendTraced as _, TokioServiceHandle, U256,
    app_state::DatabaseProvider,
    block_production::{Seed, SolutionContext, TxPathJob, TxPathSource},
    get_leaf_proof, partition_chunk_offset_ie, u256_from_le_bytes,
};
use irys_vdf::state::VdfStateReadonly;
use nodit::Interval;
use openssl::sha;
use reth::tasks::shutdown::Shutdown;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::{Instrument as _, debug, error, info, warn};

/// Commands that control the partition mining service
#[derive(Debug)]
pub enum PartitionMiningCommand {
    /// Enable/disable mining for this partition
    SetMining(bool),
}

/// Controller for a running PartitionMiningService
#[derive(Debug, Clone)]
pub struct PartitionMiningController {
    cmd_tx: UnboundedSender<PartitionMiningCommand>,
}

impl PartitionMiningController {
    /// Toggle mining on or off for this partition
    pub fn set_mining(&self, enabled: bool) {
        // Best-effort. If the service is gone, ignore the error.
        let _ = self.cmd_tx.send(PartitionMiningCommand::SetMining(enabled));
    }
}

/// Inner logic for the partition mining tokio service
#[derive(Debug)]
pub struct PartitionMiningServiceInner {
    config: Config,
    service_senders: ServiceSenders,
    storage_module: Arc<StorageModule>,
    should_mine: bool,
    difficulty: U256,
    ranges: Ranges,
    steps_guard: VdfStateReadonly,
    /// Consensus database for a tx path the index does not store.
    tx_path_db: Option<DatabaseProvider>,
    /// Header prefetch pool. `None` in tests that never derive.
    tx_path_pool: Option<TxPathPool>,
    /// Expiry `reset` did not finish. Retried until it does. Packing waits.
    expiry_reset_pending: bool,
    /// Epoch height of the expiry that is still pending. The retry stores
    /// the capacity assignment at this height.
    expiry_update_height: u64,
    /// Hash the expiry notice named. A retry resets only while this hash is
    /// still the one stored.
    expiry_partition_hash: Option<irys_types::H256>,
    /// Path lookups this miner issued. Proves a non-winning recall does not
    /// open the index.
    #[cfg(test)]
    data_path_lookups: std::sync::atomic::AtomicU64,
}

/// What the derive job must compare against the rebuilt path.
enum DerivePlacement {
    /// MDBX stores no placement identity on the derive path.
    // MDBX-SUBMODULE-INDEX: delete this variant. MDBX stores no placement identity on the derive path. RocksDB v1 checks the stored row.
    Skip,
    /// RocksDB v1 row for this offset.
    Check(irys_database::tx_path_derive::PlacementCheck),
    /// RocksDB v1 has no usable row. The job fails with this text.
    Missing(String),
}

/// Rayon pool. The pool type has no `Debug`, and the miner struct does.
#[derive(Clone)]
struct TxPathPool(Arc<rayon::ThreadPool>);

impl std::fmt::Debug for TxPathPool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TxPathPool")
            .field("threads", &self.0.current_num_threads())
            .finish()
    }
}

impl std::ops::Deref for TxPathPool {
    type Target = rayon::ThreadPool;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Ledger chunk offset of one partition offset.
///
/// `slot_index * num_chunks_in_partition + partition_offset`, the same sum PoA
/// validation uses. `None` when the partition has no slot or the sum overflows.
fn ledger_chunk_offset(
    slot_index: Option<usize>,
    chunks_in_partition: u64,
    partition_offset: u32,
) -> Option<u64> {
    let slot = u64::try_from(slot_index?).ok()?;
    slot.checked_mul(chunks_in_partition)?
        .checked_add(u64::from(partition_offset))
}

/// Fresh recall-range rotation state sized for the partition.
fn fresh_ranges(config: &irys_types::ConsensusConfig) -> Ranges {
    Ranges::new(
        num_recall_ranges_in_partition(config)
            .try_into()
            .expect("Recall ranges number exceeds usize representation"),
    )
    .expect("num_recall_ranges_in_partition is always > 0 for a valid ConsensusConfig")
}

fn check_startup_placement(
    db: DatabaseProvider,
    ledger: DataLedger,
    ledger_offset: u64,
    chunk_size: u64,
    pool: Option<TxPathPool>,
    placement: DerivePlacement,
) -> eyre::Result<()> {
    let derived = irys_database::tx_path_derive::derive_tx_path_at_offset_with(
        db.0.as_ref(),
        ledger,
        ledger_offset,
        chunk_size,
        pool.as_deref(),
    )?;
    let check = match placement {
        DerivePlacement::Skip => None,
        DerivePlacement::Check(check) => Some(check),
        DerivePlacement::Missing(reason) => {
            return Err(eyre::eyre!("{reason}"));
        }
    };
    let Some(check) = check else {
        return Ok(());
    };
    let tx_index =
        u32::try_from(derived.tx_index).map_err(|_| eyre::eyre!("tx index exceeds u32"))?;
    if check.data_root != derived.data_root
        || check.block_height != Some(derived.height)
        || check.tx_index != Some(tx_index)
        || check.ledger != Some(derived.ledger.get_id())
    {
        return Err(eyre::eyre!(
            "placement mismatch: derived data_root {} height {} tx_index {tx_index} ledger {}",
            derived.data_root,
            derived.height,
            derived.ledger.get_id()
        ));
    }
    Ok(())
}

impl PartitionMiningServiceInner {
    pub fn new(
        config: &Config,
        service_senders: ServiceSenders,
        storage_module: Arc<StorageModule>,
        start_mining: bool,
        steps_guard: VdfStateReadonly,
        initial_difficulty: U256,
        tx_path_db: Option<DatabaseProvider>,
        tx_path_pool: Option<Arc<rayon::ThreadPool>>,
    ) -> Self {
        Self {
            config: config.clone(),
            service_senders,
            storage_module,
            should_mine: start_mining,
            difficulty: initial_difficulty,
            ranges: fresh_ranges(&config.consensus),
            steps_guard,
            tx_path_db,
            tx_path_pool: tx_path_pool.map(TxPathPool),
            expiry_reset_pending: false,
            expiry_update_height: 0,
            expiry_partition_hash: None,
            #[cfg(test)]
            data_path_lookups: std::sync::atomic::AtomicU64::new(0),
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn set_mining(&mut self, should_mine: bool) {
        debug!(
            "Setting should_mine to {} from {}",
            should_mine, self.should_mine
        );
        self.should_mine = should_mine;
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn update_difficulty(&mut self, new_diff: U256) {
        debug!(
            "Updating difficulty target in partition miner {}: from {} to {} (diff: {})",
            self.storage_module.id,
            self.difficulty,
            new_diff,
            self.difficulty.abs_diff(new_diff)
        );
        self.difficulty = new_diff;
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn handle_partitions_expiration(&mut self, expired: &H256List, update_height: u64) {
        let Some(partition_hash) = self.storage_module.partition_hash() else {
            // Unassigned. An armed expiry must not fire later and stamp its
            // height onto whatever gets stored next.
            self.drop_armed_expiry();
            return;
        };
        if !expired.0.contains(&partition_hash) {
            // This notice does not name the hash. A failed expiry of an older
            // hash stays armed otherwise, and the retry stores capacity at
            // that older height.
            self.drop_armed_expiry();
            return;
        }
        // The assignment update is the last writer for a ledger slot. This
        // notice also names a hash that the same epoch backfilled onto a new
        // slot. The storage task can install that slot before this task reads
        // the notice. Resetting then would store capacity over it.
        if self.ledger_slot_stored_since(update_height) {
            self.expiry_reset_pending = false;
            debug!(
                storage_module.partition_hash = ?partition_hash,
                "Partition expiry notice left the ledger assignment in place"
            );
            return;
        }
        self.expiry_update_height = update_height;
        self.expiry_partition_hash = Some(partition_hash);
        // This call is the attempt. Do not also run the retry here: a
        // successful retry and this handler would pack twice.
        // The read above can be stale. `expire_to_capacity` reads the hash
        // and the height again under the assignment write lock.
        self.finish_or_arm_expiry(
            partition_hash,
            "Expiring partition hash",
            "Expiring partition hash, could not reset its storage module",
        );
    }

    /// Drop an armed expiry without resetting. The named hash is cleared so
    /// a later retry cannot store capacity for it.
    fn drop_armed_expiry(&mut self) {
        self.expiry_reset_pending = false;
        self.expiry_partition_hash = None;
    }

    /// Finish an expiry reset that returned an error. No-op when none is
    /// pending. `reset` returning `Ok`, or never running, is not completion.
    /// The expiry is done when the capacity assignment is stored, or when
    /// the slot now stored is one this notice must keep.
    fn finish_expiry_reset(&mut self) {
        if !self.expiry_reset_pending {
            return;
        }
        // A ledger installed while the reset was pending owns the module.
        // Retrying the capacity reset would remove that slot. A read error
        // here is not that ledger: the locked call below leaves the index
        // in place and keeps the expiry armed.
        if self.ledger_slot_stored_since(self.expiry_update_height) {
            self.expiry_reset_pending = false;
            return;
        }
        let Some(partition_hash) = self.expiry_partition_hash else {
            self.expiry_reset_pending = false;
            return;
        };
        self.finish_or_arm_expiry(
            partition_hash,
            "Finished a pending partition expiry reset",
            "Partition expiry reset still failing",
        );
    }

    /// Store capacity and pack, or leave the module alone.
    ///
    /// `Ok(None)` is a slot this notice must not replace. `Err` leaves the
    /// expiry armed. Packing runs only after the capacity assignment is stored.
    fn finish_or_arm_expiry(
        &mut self,
        partition_hash: irys_types::H256,
        done_message: &'static str,
        error_message: &'static str,
    ) {
        match self.expire_to_capacity() {
            Ok(Some(interval)) => {
                self.expiry_reset_pending = false;
                debug!(
                    storage_module.partition_hash = ?partition_hash,
                    storage_module.packing_interval = ?interval,
                    "{done_message}"
                );
                self.enqueue_expiry_packing(partition_hash, PartitionChunkRange(interval));
            }
            Ok(None) => {
                self.expiry_reset_pending = false;
                debug!(
                    storage_module.partition_hash = ?partition_hash,
                    "Partition expiry left the current assignment in place"
                );
            }
            Err(err) => {
                self.expiry_reset_pending = true;
                error!(
                    storage_module.partition_hash = ?partition_hash,
                    "{error_message}: {err}"
                );
            }
        }
    }

    /// The module is on a ledger slot stored at or after the expiry height.
    /// That is the epoch's new slot: the assignment update clears its index
    /// and this service must not replace it with capacity. A slot stored
    /// earlier is the one expiring, and is reset.
    fn ledger_slot_stored_since(&self, update_height: u64) -> bool {
        match self.storage_module.partition_assignment_with_height() {
            Ok((Some(assignment), Some(height))) => {
                assignment.ledger_id.is_some() && height >= update_height
            }
            Ok(_) => false,
            // Not a decision to reset. The locked call reads the file again
            // and leaves the index in place when the read still fails.
            Err(err) => {
                debug!("expiry check could not read packing params: {err}");
                false
            }
        }
    }

    /// Reset to capacity under the assignment write lock when the hash this
    /// notice named is still stored from before the notice. `Ok(None)` when
    /// that hash was replaced or the slot was stored at or after the notice.
    /// A failed reset leaves the previous assignment in place.
    fn expire_to_capacity(&self) -> eyre::Result<Option<Interval<PartitionChunkOffset>>> {
        let Some(expired_hash) = self.expiry_partition_hash else {
            eyre::bail!("expiry reset has no partition hash");
        };
        self.storage_module
            .reset_expired_to_capacity(expired_hash, self.expiry_update_height)
    }

    fn enqueue_expiry_packing(&self, partition_hash: irys_types::H256, range: PartitionChunkRange) {
        let Ok(req) = PackingRequest::new(self.storage_module.clone(), range) else {
            return;
        };
        match self.service_senders.packing_sender.try_send(req) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                warn!(
                    storage_module.id = %self.storage_module.id,
                    storage_module.partition_hash = ?partition_hash,
                    storage_module.packing_interval = ?range,
                    "Dropping packing request due to a saturated channel"
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_req)) => {
                error!(
                    storage_module.id = %self.storage_module.id,
                    storage_module.partition_hash = ?partition_hash,
                    storage_module.packing_interval = ?range,
                    "Packing channel closed; failed to enqueue repacking request"
                );
            }
        }
    }

    /// React to an in-place VDF re-anchor after a deep reorg. The seed buffer's
    /// VALUES were rewritten to canonical while the step numbers stayed the same
    /// (see `VdfState::reanchor_seeds`), so the recall-range rotation cached in
    /// `self.ranges` is now derived from poisoned seeds. Because the step numbers
    /// remain consecutive, `get_recall_range`'s gap-driven reconstruction never
    /// fires on its own — so reset the rotation to a fresh state here, forcing the
    /// next seed to reconstruct it from the healed buffer.
    #[tracing::instrument(level = "trace", skip_all)]
    fn handle_reanchored(&mut self) {
        self.ranges = fresh_ranges(&self.config.consensus);
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    fn get_recall_range(
        &mut self,
        step: u64,
        seed: &irys_types::H256,
        partition_hash: &irys_types::H256,
    ) -> eyre::Result<u64> {
        let next_ranges_step = self.ranges.last_step_num + 1; // next consecutive step expected to be calculated by ranges
        // Fast path only for the exact next step. A gap, or a step behind the
        // iterator, rebuilds the rotation. Advancing a rewound iterator mines a
        // recall range the VDF steps no longer match.
        if next_ranges_step == step {
            debug!("Step {} is the next consecutive step", step);
        } else {
            debug!(
                "Non consecutive step {} (iterator at {}) may need to reconstruct ranges",
                step, self.ranges.last_step_num
            );
            // calculate the nearest step lower or equal to step where recall ranges are reinitialized, as this is the step from where ranges will be recalculated
            let reset_step = self.ranges.reset_step(step);
            debug!(
                "Near reset step is {} num recall ranges in partition {}",
                reset_step, self.ranges.num_recall_ranges_in_partition
            );
            // Reinitialize when the iterator cannot incrementally reach `step` from
            // `next_ranges_step`: either the reset boundary is past where the iterator is (a
            // forward gap across a boundary), or the iterator is AHEAD of `step` (a backward
            // re-anchor rewind — its rotation must be rebuilt from the boundary, not advanced).
            let start = if reset_step > next_ranges_step || step < next_ranges_step {
                debug!(
                    "Step {} not incrementally reachable from last processed step {}, reinitializing ranges ...",
                    step, self.ranges.last_step_num
                );
                self.ranges.reinitialize();
                self.ranges.last_step_num = reset_step.saturating_sub(1); // advance last step number calculated by ranges to (reset_step - 1), so ranges next step will be reset_step line
                reset_step
            } else {
                next_ranges_step
            };
            // check if we need to reconstruct steps, that is interval start..=step-1 is not empty
            if start < step {
                debug!("Getting stored steps from ({}..={})", start, step - 1);
                let vdf_steps = self.steps_guard.read();
                let steps = vdf_steps.get_steps(ii(start, step - 1))?; // -1 because last step is calculated in next get_recall_range call, with its corresponding argument seed
                self.ranges.reconstruct(&steps, partition_hash)?;
            };
        }

        u64::try_from(self.ranges.get_recall_range(step, seed, partition_hash)?)
            .wrap_err("recall range larger than u64")
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    fn mine_partition_with_seed(
        &mut self,
        mining_seed: irys_types::H256,
        vdf_step: u64,
        checkpoints: &H256List,
    ) -> eyre::Result<Option<SolutionContext>> {
        let partition_hash = match self.storage_module.partition_hash() {
            Some(p) => p,
            None => {
                warn!("No partition assigned!");
                return Ok(None);
            }
        };

        // Pick a random recall range in the partition using efficient sampling
        let recall_range_index = self.get_recall_range(vdf_step, &mining_seed, &partition_hash)?;

        // Starting chunk index within partition
        let start_chunk_offset = (recall_range_index as u32)
            .saturating_mul(self.config.consensus.num_chunks_in_recall_range as u32);

        let read_range = partition_chunk_offset_ie!(
            start_chunk_offset,
            start_chunk_offset + self.config.consensus.num_chunks_in_recall_range as u32
        );

        let RecallChunks { chunks, rows: _ } =
            self.storage_module.read_recall_chunks(read_range)?;
        if chunks.is_empty() {
            warn!(
                "No chunks found - storage_module_id:{} {}-{}",
                self.storage_module.id,
                &read_range.start(),
                &read_range.end()
            );
        }

        // Hash the recall before any index read. Only a winning chunk needs
        // its proof. A data proof must bind these bytes: block production copies
        // the paths into the PoA and does not check them, and a packed write
        // between this read and the lookup can pair them with a new path.
        for (&partition_chunk_offset, (chunk_bytes, chunk_type)) in chunks.iter() {
            match *chunk_type {
                ChunkType::Uninitialized => {
                    return Err(eyre::eyre!("Cannot mine uninitialized chunks"));
                }
                ChunkType::Interrupted => {
                    return Err(eyre::eyre!("Cannot mine interrupted chunks"));
                }
                ChunkType::Entropy | ChunkType::Data => {}
            }

            let solution_hash = irys_types::compute_solution_hash(
                chunk_bytes,
                *partition_chunk_offset,
                &mining_seed,
            );
            let test_solution = u256_from_le_bytes(&solution_hash.0);
            if test_solution < self.difficulty {
                continue;
            }

            let (tx_path, data_path) = if *chunk_type == ChunkType::Data {
                let Some((tx_path, data_path)) =
                    self.paths_for_winning_data_chunk(partition_chunk_offset, chunk_bytes)
                else {
                    continue;
                };
                (tx_path, Some(data_path))
            } else {
                (TxPathSource::Stored(None), None)
            };

            let ledger_offset = ledger_chunk_offset(
                self.storage_module
                    .partition_assignment()
                    .and_then(|assignment| assignment.slot_index),
                self.config.consensus.num_chunks_in_partition,
                *partition_chunk_offset,
            );
            info!(
                "Solution Found - partition_id: {}, partition_offset: {}/{}, ledger_offset: {:?}, range_offset: {}/{} difficulty {}",
                self.storage_module.id,
                partition_chunk_offset,
                self.config.consensus.num_chunks_in_partition,
                ledger_offset,
                *partition_chunk_offset - start_chunk_offset,
                self.config.consensus.num_chunks_in_recall_range,
                self.difficulty
            );
            metrics::record_mining_solution_found();

            let solution = SolutionContext {
                partition_hash,
                chunk_offset: *partition_chunk_offset,
                mining_address: self.config.node_config.miner_address(),
                tx_path,
                data_path,
                chunk: chunk_bytes.clone(),
                vdf_step,
                checkpoints: checkpoints.clone(),
                seed: Seed(mining_seed),
                solution_hash,
            };

            return Ok(Some(solution));
        }

        Ok(None)
    }

    /// One index read for a winning data offset. `None` means do not submit
    /// this chunk: the lookup failed, the data path is missing or does not
    /// bind `chunk_bytes`, or a missing tx path cannot be derived. The caller
    /// keeps scanning the range.
    fn paths_for_winning_data_chunk(
        &self,
        offset: PartitionChunkOffset,
        chunk_bytes: &[u8],
    ) -> Option<(TxPathSource, ChunkDataPath)> {
        #[cfg(test)]
        self.data_path_lookups
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // The index key is the partition offset.
        let (stored_tx_path, data_path) =
            match self.storage_module.read_tx_data_paths(offset, offset) {
                Ok(paths) => paths.get(&offset).cloned().unwrap_or((None, None)),
                Err(error) => {
                    warn!(
                        storage_module.id = self.storage_module.id,
                        chunk_offset = offset.0,
                        %error,
                        "winning data chunk path lookup failed"
                    );
                    return None;
                }
            };
        let Some(data_path) = data_path else {
            warn!(
                storage_module.id = self.storage_module.id,
                chunk_offset = offset.0,
                "winning data chunk has no stored data path"
            );
            return None;
        };
        if !self.data_path_binds_chunk(offset, chunk_bytes, &data_path) {
            warn!(
                storage_module.id = self.storage_module.id,
                chunk_offset = offset.0,
                "winning data chunk path does not bind the recall bytes"
            );
            return None;
        }
        let tx_path = if let Some(stored) = stored_tx_path {
            // MDBX-SUBMODULE-INDEX: delete this arm. Only MDBX returns a stored data-chunk tx path. RocksDB v1 derives it.
            TxPathSource::Stored(Some(stored))
        } else {
            let job = self.spawn_tx_path_derive(offset, &data_path)?;
            TxPathSource::Derive(job)
        };
        Some((tx_path, data_path))
    }

    /// Start a derive for `offset`. `None` when this miner has no consensus
    /// database, the partition has no ledger slot, the submodule is missing,
    /// RocksDB v1 has no placement for the offset, or [`TxPathJob::try_spawn`]
    /// already has its threads running.
    ///
    /// The job checks the derived root against `data_path` and, on RocksDB v1,
    /// against the stored placement. A mismatch fails the job.
    fn spawn_tx_path_derive(
        &self,
        offset: PartitionChunkOffset,
        data_path: &[u8],
    ) -> Option<TxPathJob> {
        let Some((ledger, ledger_offset)) = self.assigned_ledger_offset(offset) else {
            warn!(
                storage_module.id = self.storage_module.id,
                chunk_offset = offset.0,
                "winning data chunk has no stored tx path and the partition has no ledger slot"
            );
            return None;
        };
        let Some(db) = self.tx_path_db.clone() else {
            warn!(
                storage_module.id = self.storage_module.id,
                chunk_offset = offset.0,
                "winning data chunk has no stored tx path and tx path derive has no consensus database"
            );
            return None;
        };
        let pool = self.tx_path_pool.clone();
        let chunk_size = self.config.consensus.chunk_size;
        let data_path = data_path.to_vec();
        let placement = match self.placement_for_derive(offset) {
            None => {
                warn!(
                    storage_module.id = self.storage_module.id,
                    chunk_offset = offset.0,
                    "winning data chunk has no stored tx path and the submodule is missing"
                );
                return None;
            }
            // The job would read every header and then reject this offset.
            // Skip it before taking a derive slot, and keep scanning.
            Some(DerivePlacement::Missing(reason)) => {
                warn!(
                    storage_module.id = self.storage_module.id,
                    chunk_offset = offset.0,
                    "winning data chunk has no stored tx path and {reason}"
                );
                return None;
            }
            Some(DerivePlacement::Skip) => None,
            Some(DerivePlacement::Check(check)) => Some(check),
        };
        let Some(job) = TxPathJob::try_spawn(move || {
            let derived = irys_database::tx_path_derive::derive_tx_path_at_offset_with(
                db.0.as_ref(),
                ledger,
                ledger_offset,
                chunk_size,
                pool.as_deref(),
            )?;
            irys_database::tx_path_derive::cross_check_derived(
                &derived,
                ledger_offset,
                chunk_size,
                &data_path,
                placement,
            )?;
            Ok(derived.proof.proof)
        }) else {
            debug!(
                storage_module.id = self.storage_module.id,
                chunk_offset = offset.0,
                "tx path derive is busy; this chunk is not submitted"
            );
            return None;
        };
        Some(job)
    }

    /// MDBX skips the placement check. RocksDB v1 requires the stored row.
    fn placement_for_derive(&self, offset: PartitionChunkOffset) -> Option<DerivePlacement> {
        let (_, submodule) = self.storage_module.get_submodule_for_offset(offset).ok()?;
        // MDBX-SUBMODULE-INDEX: delete this arm. MDBX stores the tx path and skips the placement check.
        if submodule.db.stores_tx_paths() {
            return Some(DerivePlacement::Skip);
        }
        use irys_database::submodule::SubmoduleStore as _;
        Some(match submodule.db.view(|tx| tx.placement_at(offset)) {
            Ok(Some(placement)) => {
                DerivePlacement::Check(irys_database::tx_path_derive::PlacementCheck {
                    data_root: placement.data_root,
                    ledger: placement.ledger,
                    block_height: placement.block_height,
                    tx_index: placement.tx_index,
                })
            }
            Ok(None) => DerivePlacement::Missing(
                "placement mismatch: no placement covers the winning offset".to_owned(),
            ),
            Err(error) => DerivePlacement::Missing(format!("placement mismatch: {error}")),
        })
    }

    /// Derive one stored data offset before the mining loop.
    ///
    /// A failure is logged and counted. The service still mines. An empty
    /// partition, or a capacity assignment with no ledger, is skipped.
    async fn check_startup_derive(&self) {
        let Some(offset) = self.first_data_offset() else {
            return;
        };
        let Some((ledger, ledger_offset)) = self.assigned_ledger_offset(offset) else {
            return;
        };
        let Some(db) = self.tx_path_db.clone() else {
            error!(
                storage_module.id = self.storage_module.id,
                chunk_offset = offset.0,
                "startup tx path derive has no consensus database"
            );
            metrics::record_tx_path_derive_failed("startup");
            return;
        };
        let Some(placement) = self.placement_for_derive(offset) else {
            error!(
                storage_module.id = self.storage_module.id,
                chunk_offset = offset.0,
                "startup tx path derive has no submodule for the data interval"
            );
            metrics::record_tx_path_derive_failed("startup");
            return;
        };
        let pool = self.tx_path_pool.clone();
        let chunk_size = self.config.consensus.chunk_size;
        let module_id = self.storage_module.id;
        let joined = tokio::task::spawn_blocking(move || {
            check_startup_placement(db, ledger, ledger_offset, chunk_size, pool, placement)
        })
        .await;
        let failed = match joined {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(error) => Some(eyre::eyre!("startup tx path derive task failed: {error}")),
        };
        if let Some(error) = failed {
            error!(
                storage_module.id = module_id,
                chunk_offset = offset.0,
                "startup tx path derive failed: {error:#}"
            );
            metrics::record_tx_path_derive_failed("startup");
        }
    }

    fn first_data_offset(&self) -> Option<PartitionChunkOffset> {
        self.storage_module
            .get_intervals(ChunkType::Data)
            .into_iter()
            .next()
            .map(|interval| interval.start())
    }

    fn assigned_ledger_offset(&self, offset: PartitionChunkOffset) -> Option<(DataLedger, u64)> {
        let assignment = self.storage_module.partition_assignment()?;
        let ledger_id = assignment.ledger_id?;
        let ledger = DataLedger::try_from(ledger_id).ok()?;
        let ledger_offset = ledger_chunk_offset(
            assignment.slot_index,
            self.config.consensus.num_chunks_in_partition,
            *offset,
        )?;
        Some((ledger, ledger_offset))
    }

    /// True when `data_path`'s leaf hash equals the sha256 of the unpacked
    /// recall prefix. The leaf stores the exclusive end offset. A full chunk
    /// ends on a chunk boundary, so the span is the chunk size; a short final
    /// chunk ends inside the chunk, and the span is that remainder.
    fn data_path_binds_chunk(
        &self,
        offset: PartitionChunkOffset,
        chunk_bytes: &[u8],
        data_path: &ChunkDataPath,
    ) -> bool {
        let chunk_size = self.config.consensus.chunk_size as usize;
        if chunk_bytes.len() != chunk_size {
            return false;
        }
        let Ok(leaf) = get_leaf_proof(&Base64(data_path.clone())) else {
            return false;
        };
        let Some(leaf_hash) = leaf.hash() else {
            return false;
        };
        let end = leaf.offset();
        let rem = end % chunk_size;
        let span = if rem == 0 { chunk_size } else { rem };
        let Some(partition_hash) = self.storage_module.partition_hash() else {
            return false;
        };
        let mut entropy = Vec::with_capacity(chunk_size);
        compute_entropy_chunk(
            self.config.node_config.miner_address(),
            u64::from(offset.0),
            partition_hash.0,
            self.config.consensus.entropy_packing_iterations,
            chunk_size,
            &mut entropy,
            self.config.consensus.chain_id,
        );
        if entropy.len() != chunk_size {
            return false;
        }
        let mut unpacked = chunk_bytes.to_vec();
        xor_vec_u8_arrays_in_place(&mut unpacked, &entropy);
        sha::sha256(&unpacked[..span]) == leaf_hash
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn handle_seed(&mut self, msg: &BroadcastMiningSeed) {
        let seed = msg.seed.clone();
        if !self.should_mine {
            debug!("Mining disabled, skipping seed {:?}", seed);
            return;
        }

        if self.storage_module.partition_assignment().is_none() {
            debug!("No partition assigned - skipping seed {:?}", seed);
            return;
        }

        let current_step = self.steps_guard.current_step();

        debug!(
            "Mining partition {} with seed {:?} step number {} current step {}",
            self.storage_module.partition_hash().unwrap(),
            seed,
            msg.global_step,
            current_step
        );

        let lag = current_step.saturating_sub(msg.global_step);
        if lag >= 3 {
            warn!(
                "Storage module {} is {} steps behind in mining. Skipping.",
                self.storage_module.id, lag
            );
            return;
        }

        debug!(
            "Partition {} -- looking for solution with difficulty >= {}",
            self.storage_module.partition_hash().unwrap(),
            self.difficulty
        );

        match self.mine_partition_with_seed(seed.into_inner(), msg.global_step, &msg.checkpoints) {
            Ok(Some(s)) => {
                let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
                let cmd = BlockProducerCommand::SolutionFound {
                    solution: s,
                    response: response_tx,
                };
                if let Err(err) = self.service_senders.block_producer.send_traced(cmd) {
                    error!(
                        "Error submitting solution to block producer for storage_module {} partition_hash {:?} step {}: {:?}",
                        self.storage_module.id,
                        self.storage_module.partition_hash(),
                        msg.global_step,
                        err
                    );
                }
            }
            Ok(None) => {
                // No solution found this step.
            }
            Err(err) => error!(
                "Error in handling mining solution for storage_module {} partition_hash {:?}: {:?}",
                self.storage_module.id,
                self.storage_module.partition_hash(),
                err
            ),
        };
    }
}

#[cfg(test)]
impl PartitionMiningServiceInner {
    /// Test-only helper to expose recall-range computation without starting the service.
    pub fn test_get_recall_range(
        &mut self,
        step: u64,
        seed: irys_types::H256,
        partition_hash: irys_types::H256,
    ) -> u64 {
        self.get_recall_range(step, &seed, &partition_hash)
            .expect("test_get_recall_range failed")
    }

    /// Test-only helper to drive the mining loop directly (bypassing the
    /// service event loop) so a sparse recall range with an `Uninitialized`
    /// hole can be exercised.
    pub fn test_mine_partition_with_seed(
        &mut self,
        mining_seed: irys_types::H256,
        vdf_step: u64,
        checkpoints: &H256List,
    ) -> Option<SolutionContext> {
        self.mine_partition_with_seed(mining_seed, vdf_step, checkpoints)
            .expect("test_mine_partition_with_seed failed")
    }

    fn clear_data_path_lookups(&self) {
        self.data_path_lookups
            .store(0, std::sync::atomic::Ordering::SeqCst);
    }

    fn data_path_lookup_count(&self) -> u64 {
        self.data_path_lookups
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Tokio service for partition mining
#[derive(Debug)]
pub struct PartitionMiningService {
    shutdown: Shutdown,
    state: PartitionMiningServiceInner,
    cmd_rx: UnboundedReceiver<PartitionMiningCommand>,
    broadcast_rx: UnboundedReceiver<Arc<MiningBroadcastEvent>>,
}

impl PartitionMiningService {
    /// Spawns a Tokio partition mining service and subscribes to the global broadcaster
    pub fn spawn_service(
        inner: PartitionMiningServiceInner,
        runtime_handle: tokio::runtime::Handle,
    ) -> (PartitionMiningController, TokioServiceHandle) {
        // Control channel
        let (cmd_tx, cmd_rx) = unbounded_channel();

        // Broadcast subscription channel
        let broadcast_rx = inner.service_senders.subscribe_mining_broadcast();

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let storage_module_id = inner.storage_module.id;
        let svc = Self {
            shutdown: shutdown_rx,
            state: inner,
            cmd_rx,
            broadcast_rx,
        };

        let handle = runtime_handle.spawn(
            async move {
                svc.start().await;
            }
            .instrument(tracing::info_span!("partition_mining_service", %storage_module_id)),
        );

        let controller = PartitionMiningController { cmd_tx };

        let svc_handle = TokioServiceHandle {
            name: "partition_mining_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        };

        (controller, svc_handle)
    }

    /// `handle_seed` reads `chunks.dat` and then hashes the recall range.
    /// A multi-thread runtime moves this worker to the blocking pool so
    /// other tasks still run during that read.
    fn mine_seed(&mut self, msg: &BroadcastMiningSeed) {
        let multi_thread = tokio::runtime::Handle::try_current().is_ok_and(|handle| {
            handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
        });
        if multi_thread {
            tokio::task::block_in_place(|| self.state.handle_seed(msg));
        } else {
            self.state.handle_seed(msg);
        }
    }

    async fn start(mut self) {
        info!("Starting partition mining service");
        self.state.check_startup_derive().await;
        // Monotonic. A stalled runtime retries once, then waits again.
        let mut expiry_retry = tokio::time::interval(Duration::from_secs(5));
        expiry_retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        expiry_retry.tick().await;
        loop {
            tokio::select! {
                // Shutdown
                _ = &mut self.shutdown => {
                    info!("Shutdown signal received for partition mining service");
                    break;
                }

                // Control commands
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(PartitionMiningCommand::SetMining(enabled)) => {
                            self.state.set_mining(enabled);
                        }
                        None => {
                            warn!("PartitionMiningService controller channel closed; stopping service");
                            break;
                        }
                    }
                }

                // Broadcast messages (seed/diff/expiration)
                evt = self.broadcast_rx.recv() => {
                    match evt {
                        Some(arc_evt) => match arc_evt.as_ref() {
                            MiningBroadcastEvent::Seed(msg) => {
                                self.state.finish_expiry_reset();
                                self.mine_seed(msg);
                            }
                            MiningBroadcastEvent::Difficulty(BroadcastDifficultyUpdate(h)) => {
                                self.state.finish_expiry_reset();
                                self.state.update_difficulty(h.diff);
                            }
                            MiningBroadcastEvent::PartitionsExpiration(BroadcastPartitionsExpiration {
                                partition_hashes,
                                update_height,
                            }) => {
                                self.state
                                    .handle_partitions_expiration(partition_hashes, *update_height);
                            }
                            MiningBroadcastEvent::Reanchored => {
                                self.state.finish_expiry_reset();
                                self.state.handle_reanchored();
                            }
                        },
                        None => {
                            warn!("Mining broadcaster channel closed; stopping service");
                            break;
                        }
                    }
                }

                _ = expiry_retry.tick(), if self.state.expiry_reset_pending => {
                    self.state.finish_expiry_reset();
                }
            }
        }
        info!("Partition mining service stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_domain::{PACKING_PARAMS_FILE_NAME, PackingParams, StorageModuleInfo};
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{
        ConsensusConfig, ConsensusOptions, DataLedger, DataTransactionLedger, H256, H256List,
        LedgerChunkOffset, LedgerChunkRange, NodeConfig, StorageSyncConfig, SubmoduleIndexEngine,
        irys::IrysSigner, ledger_chunk_offset_ii, partition::PartitionAssignment,
    };
    use irys_vdf::state::VdfState;
    use nodit::interval::ii;
    use std::sync::RwLock;
    use std::sync::atomic::AtomicBool;

    /// One capacity or ledger module (10 chunks, recall range 2) and the
    /// absolute submodule path an expiry reset writes `packing_params.toml` into.
    fn open_expiry_module(
        hash_byte: u8,
        ledger: Option<(DataLedger, usize)>,
    ) -> (
        irys_testing_utils::utils::tempfile::TempDir,
        Config,
        Arc<StorageModule>,
        H256,
    ) {
        let tmp_dir = TempDirBuilder::new().build();
        let node_config = NodeConfig {
            consensus: ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 10,
                num_chunks_in_recall_range: 2,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let partition_hash = H256::repeat_byte(hash_byte);
        let (ledger_id, slot_index) = match ledger {
            Some((ledger, slot)) => (Some(ledger.into()), Some(slot)),
            None => (None, None),
        };
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id,
                slot_index,
            }),
            submodules: vec![(
                partition_chunk_offset_ie!(0, 10),
                tmp_dir.path().join("hdd0"),
            )],
        };
        let storage_module =
            Arc::new(StorageModule::new(&info, &config).expect("test storage module"));
        (tmp_dir, config, storage_module, partition_hash)
    }

    fn expiry_service(
        config: &Config,
        storage_module: Arc<StorageModule>,
    ) -> (
        PartitionMiningServiceInner,
        crate::services::ServiceReceivers,
    ) {
        let vdf_state = Arc::new(RwLock::new(VdfState::new(
            8,
            0,
            Arc::new(AtomicBool::new(false)),
        )));
        let (service_senders, receivers) = ServiceSenders::new();
        let inner = PartitionMiningServiceInner::new(
            config,
            service_senders,
            storage_module,
            false,
            VdfStateReadonly::new(vdf_state),
            U256::zero(),
            None,
            None,
        );
        (inner, receivers)
    }

    fn write_synced_data(storage: &StorageModule, byte: u8) {
        assert!(storage.write_chunk(
            PartitionChunkOffset::from(0),
            vec![byte; 32],
            ChunkType::Data,
        ));
        storage.force_sync_pending_chunks().expect("sync data");
    }

    /// A failed expiry reset must not enqueue packing. The retry does.
    #[test]
    fn failed_expiry_reset_does_not_pack_until_retry() {
        let (_tmp, config, storage_module, partition_hash) = open_expiry_module(0x77, None);
        let (mut inner, mut receivers) = expiry_service(&config, Arc::clone(&storage_module));

        inner.handle_partitions_expiration(&H256List(vec![H256::repeat_byte(0x01)]), 11);
        assert!(!inner.expiry_reset_pending);

        storage_module.fail_next_reset_for_test();
        inner.handle_partitions_expiration(&H256List(vec![partition_hash]), 11);
        assert!(inner.expiry_reset_pending);
        assert!(storage_module.reset_incomplete());
        assert!(receivers.packing.try_recv().is_err());

        inner.finish_expiry_reset();
        assert!(!inner.expiry_reset_pending);
        assert!(!storage_module.reset_incomplete());
        assert!(receivers.packing.try_recv().is_ok());
        inner.finish_expiry_reset();
        assert!(receivers.packing.try_recv().is_err());
    }

    /// The storage task can install the epoch's new ledger before this task
    /// reads the expiry notice. The notice must not replace that ledger with
    /// capacity, and it must not clear the index the assignment update owns.
    #[test]
    fn expiry_notice_keeps_a_ledger_installed_before_it() {
        let (_tmp, config, storage_module, partition_hash) =
            open_expiry_module(0x77, Some((DataLedger::Submit, 0)));
        let miner = config.node_config.miner_address();
        let offset = PartitionChunkOffset::from(0);
        write_synced_data(&storage_module, 0x11);
        assert_eq!(
            storage_module.get_chunk_type(&offset),
            Some(ChunkType::Data)
        );
        storage_module
            .assign_partition(
                PartitionAssignment {
                    partition_hash,
                    miner_address: miner,
                    ledger_id: Some(DataLedger::Publish.into()),
                    slot_index: Some(1),
                },
                20,
            )
            .expect("install the reassignment");
        let (mut inner, mut receivers) = expiry_service(&config, Arc::clone(&storage_module));

        inner.handle_partitions_expiration(&H256List(vec![partition_hash]), 20);

        let assignment = storage_module.partition_assignment().expect("assigned");
        assert_eq!(assignment.partition_hash, partition_hash);
        assert_eq!(assignment.ledger_id, Some(DataLedger::Publish.into()));
        assert_eq!(assignment.slot_index, Some(1));
        assert_eq!(
            storage_module.get_chunk_type(&offset),
            Some(ChunkType::Data),
            "the expiry notice must not clear the index of the new ledger"
        );
        assert!(!inner.expiry_reset_pending);
        assert!(receivers.packing.try_recv().is_err());
    }

    /// A ledger slot stored before the notice is the slot that expired. The
    /// notice resets it to capacity and queues packing.
    #[test]
    fn expiry_notice_resets_a_ledger_stored_before_it() {
        let (_tmp, config, storage_module, partition_hash) =
            open_expiry_module(0x78, Some((DataLedger::Submit, 0)));
        let submit = storage_module.partition_assignment().expect("assigned");
        storage_module
            .assign_partition(submit, 10)
            .expect("store the slot before the notice");
        let (mut inner, mut receivers) = expiry_service(&config, Arc::clone(&storage_module));

        inner.handle_partitions_expiration(&H256List(vec![partition_hash]), 20);

        let assignment = storage_module.partition_assignment().expect("assigned");
        assert_eq!(assignment.partition_hash, partition_hash);
        assert_eq!(assignment.ledger_id, None);
        assert_eq!(assignment.slot_index, None);
        assert!(!inner.expiry_reset_pending);
        assert!(receivers.packing.try_recv().is_ok());
    }

    /// The height check can see the expiring slot and then drop its lock.
    /// The storage task can store this epoch's ledger before the capacity
    /// reset runs. That reset must see the new slot and leave it in place.
    #[test]
    fn expire_to_capacity_keeps_a_ledger_stored_after_the_height_check() {
        let (_tmp, config, storage_module, partition_hash) =
            open_expiry_module(0x79, Some((DataLedger::Submit, 0)));
        let miner = config.node_config.miner_address();
        let submit = storage_module.partition_assignment().expect("assigned");
        storage_module
            .assign_partition(submit, 10)
            .expect("store the expiring slot");
        let (mut inner, _receivers) = expiry_service(&config, Arc::clone(&storage_module));
        // The notice sampled this slot. The sample says "reset".
        assert!(!inner.ledger_slot_stored_since(20));

        storage_module
            .assign_partition(
                PartitionAssignment {
                    partition_hash,
                    miner_address: miner,
                    ledger_id: Some(DataLedger::Publish.into()),
                    slot_index: Some(1),
                },
                20,
            )
            .expect("install the epoch ledger");
        let offset = PartitionChunkOffset::from(0);
        write_synced_data(&storage_module, 0x21);

        inner.expiry_update_height = 20;
        inner.expiry_partition_hash = Some(partition_hash);
        assert!(
            inner
                .expire_to_capacity()
                .expect("the expiry read succeeds")
                .is_none(),
            "the capacity reset must not run"
        );

        let assignment = storage_module.partition_assignment().expect("assigned");
        assert_eq!(assignment.partition_hash, partition_hash);
        assert_eq!(assignment.ledger_id, Some(DataLedger::Publish.into()));
        assert_eq!(assignment.slot_index, Some(1));
        assert_eq!(
            storage_module.get_chunk_type(&offset),
            Some(ChunkType::Data),
            "a ledger stored after the height check must keep its index"
        );
        let (_assignment, height) = storage_module
            .partition_assignment_with_height()
            .expect("packing params");
        assert_eq!(height, Some(20));
    }

    /// Expiry named hash H. The storage task then assigned a different hash,
    /// still stored below the notice. The reset must not replace that
    /// assignment or stamp the expiry height onto it.
    #[test]
    fn expire_to_capacity_leaves_a_hash_the_notice_did_not_name() {
        let (_tmp, config, storage_module, expired_hash) =
            open_expiry_module(0x7a, Some((DataLedger::Submit, 0)));
        let miner = config.node_config.miner_address();
        let other_hash = H256::repeat_byte(0x7b);
        storage_module
            .assign_partition(
                PartitionAssignment {
                    partition_hash: other_hash,
                    miner_address: miner,
                    ledger_id: Some(DataLedger::Publish.into()),
                    slot_index: Some(1),
                },
                15,
            )
            .expect("assign a different hash below the notice");
        let offset = PartitionChunkOffset::from(0);
        write_synced_data(&storage_module, 0x31);
        let (mut inner, _receivers) = expiry_service(&config, Arc::clone(&storage_module));

        inner.expiry_update_height = 40;
        inner.expiry_partition_hash = Some(expired_hash);
        assert!(
            inner
                .expire_to_capacity()
                .expect("the expiry read succeeds")
                .is_none(),
            "a different hash must not be reset"
        );

        let assignment = storage_module.partition_assignment().expect("assigned");
        assert_eq!(assignment.partition_hash, other_hash);
        assert_eq!(assignment.ledger_id, Some(DataLedger::Publish.into()));
        assert_eq!(assignment.slot_index, Some(1));
        assert_eq!(
            storage_module.get_chunk_type(&offset),
            Some(ChunkType::Data)
        );
        let (_assignment, height) = storage_module
            .partition_assignment_with_height()
            .expect("packing params");
        assert_eq!(height, Some(15));
    }

    /// A failed expiry stays armed. A later notice that does not name this
    /// hash drops that expiry. The retry must not store capacity, and it
    /// must not stamp the old expiry height onto the slot.
    #[test]
    fn notice_that_omits_the_hash_clears_a_failed_expiry() {
        let (_tmp, config, storage_module, partition_hash) =
            open_expiry_module(0x7c, Some((DataLedger::Submit, 0)));
        let submit = storage_module.partition_assignment().expect("assigned");
        storage_module
            .assign_partition(submit, 10)
            .expect("store the slot before the failed expiry");
        let offset = PartitionChunkOffset::from(0);
        write_synced_data(&storage_module, 0x41);
        let (mut inner, mut receivers) = expiry_service(&config, Arc::clone(&storage_module));

        storage_module.fail_next_reset_for_test();
        inner.handle_partitions_expiration(&H256List(vec![partition_hash]), 40);
        assert!(inner.expiry_reset_pending);
        assert!(storage_module.reset_incomplete());
        assert!(receivers.packing.try_recv().is_err());

        inner.handle_partitions_expiration(&H256List(vec![H256::repeat_byte(0x7d)]), 50);
        assert!(
            !inner.expiry_reset_pending,
            "a notice that omits this hash must drop the armed expiry"
        );

        inner.finish_expiry_reset();
        let assignment = storage_module.partition_assignment().expect("assigned");
        assert_eq!(assignment.partition_hash, partition_hash);
        assert_eq!(assignment.ledger_id, Some(DataLedger::Submit.into()));
        assert_eq!(assignment.slot_index, Some(0));
        assert_eq!(
            storage_module.get_chunk_type(&offset),
            Some(ChunkType::Data)
        );
        let (_assignment, height) = storage_module
            .partition_assignment_with_height()
            .expect("packing params");
        assert_eq!(height, Some(10));
        assert!(receivers.packing.try_recv().is_err());
    }

    /// A later notice that still names the hash finishes the failed expiry.
    #[test]
    fn later_notice_that_names_the_hash_still_resets() {
        let (_tmp, config, storage_module, partition_hash) =
            open_expiry_module(0x7e, Some((DataLedger::Submit, 0)));
        let submit = storage_module.partition_assignment().expect("assigned");
        storage_module
            .assign_partition(submit, 10)
            .expect("store the slot before the failed expiry");
        let (mut inner, mut receivers) = expiry_service(&config, Arc::clone(&storage_module));

        storage_module.fail_next_reset_for_test();
        inner.handle_partitions_expiration(&H256List(vec![partition_hash]), 40);
        assert!(inner.expiry_reset_pending);
        assert!(receivers.packing.try_recv().is_err());

        inner.handle_partitions_expiration(&H256List(vec![partition_hash]), 50);
        assert!(!inner.expiry_reset_pending);
        let assignment = storage_module.partition_assignment().expect("assigned");
        assert_eq!(assignment.partition_hash, partition_hash);
        assert_eq!(assignment.ledger_id, None);
        assert_eq!(assignment.slot_index, None);
        let (_assignment, height) = storage_module
            .partition_assignment_with_height()
            .expect("packing params");
        assert_eq!(height, Some(50));
        assert!(receivers.packing.try_recv().is_ok());
    }

    fn write_packing_params(path: &std::path::Path, params: &PackingParams) {
        params.write_to_disk(path).expect("rewrite packing params");
    }

    /// The slot this notice must keep is Publish at the notice height. A
    /// packing file that cannot be read must not clear that index. The expiry
    /// stays armed until the file can be read, then the ledger stays.
    #[test]
    fn unreadable_packing_params_does_not_reset_a_ledger_the_notice_keeps() {
        let (tmp_dir, config, storage_module, partition_hash) =
            open_expiry_module(0x81, Some((DataLedger::Publish, 1)));
        let miner = config.node_config.miner_address();
        let publish = storage_module.partition_assignment().expect("assigned");
        storage_module
            .assign_partition(publish, 20)
            .expect("store the ledger at the notice height");
        let offset = PartitionChunkOffset::from(0);
        write_synced_data(&storage_module, 0x51);
        let params_path = tmp_dir.path().join("hdd0").join(PACKING_PARAMS_FILE_NAME);
        std::fs::write(&params_path, b"not toml\n").expect("corrupt packing params");
        let (mut inner, mut receivers) = expiry_service(&config, Arc::clone(&storage_module));

        inner.handle_partitions_expiration(&H256List(vec![partition_hash]), 20);

        assert_eq!(
            storage_module.get_chunk_type(&offset),
            Some(ChunkType::Data),
            "an unreadable packing file must not clear the index"
        );
        let assignment = storage_module.partition_assignment().expect("assigned");
        assert_eq!(assignment.ledger_id, Some(DataLedger::Publish.into()));
        assert!(inner.expiry_reset_pending);
        assert!(!storage_module.reset_incomplete());
        assert!(receivers.packing.try_recv().is_err());

        inner.finish_expiry_reset();
        assert!(
            inner.expiry_reset_pending,
            "the expiry stays armed until the capacity assignment is stored or the slot is kept"
        );
        assert_eq!(
            storage_module.get_chunk_type(&offset),
            Some(ChunkType::Data)
        );

        write_packing_params(
            &params_path,
            &PackingParams {
                packing_address: miner,
                partition_hash: Some(partition_hash),
                ledger: Some(DataLedger::Publish.into()),
                slot: Some(1),
                last_updated_height: Some(20),
                entropy_packing_iterations: Some(config.consensus.entropy_packing_iterations),
                chunk_size: Some(32),
            },
        );
        inner.finish_expiry_reset();
        assert!(!inner.expiry_reset_pending);
        let assignment = storage_module.partition_assignment().expect("assigned");
        assert_eq!(assignment.ledger_id, Some(DataLedger::Publish.into()));
        assert_eq!(assignment.slot_index, Some(1));
        assert_eq!(
            storage_module.get_chunk_type(&offset),
            Some(ChunkType::Data)
        );
        assert!(receivers.packing.try_recv().is_err());
    }

    /// A real expiry whose packing file cannot be read must not reset yet.
    /// Once the file is readable again, the old slot is reset and packed.
    #[test]
    fn unreadable_packing_params_retries_an_expiry_that_must_reset() {
        let (tmp_dir, config, storage_module, partition_hash) =
            open_expiry_module(0x82, Some((DataLedger::Submit, 0)));
        let miner = config.node_config.miner_address();
        let submit = storage_module.partition_assignment().expect("assigned");
        storage_module
            .assign_partition(submit, 10)
            .expect("store the expiring slot");
        let offset = PartitionChunkOffset::from(0);
        write_synced_data(&storage_module, 0x61);
        let params_path = tmp_dir.path().join("hdd0").join(PACKING_PARAMS_FILE_NAME);
        std::fs::write(&params_path, b"not toml\n").expect("corrupt packing params");
        let (mut inner, mut receivers) = expiry_service(&config, Arc::clone(&storage_module));

        inner.handle_partitions_expiration(&H256List(vec![partition_hash]), 20);
        assert_eq!(
            storage_module.get_chunk_type(&offset),
            Some(ChunkType::Data)
        );
        assert!(inner.expiry_reset_pending);
        assert!(receivers.packing.try_recv().is_err());

        inner.finish_expiry_reset();
        assert!(inner.expiry_reset_pending);
        assert_eq!(
            storage_module.get_chunk_type(&offset),
            Some(ChunkType::Data)
        );

        write_packing_params(
            &params_path,
            &PackingParams {
                packing_address: miner,
                partition_hash: Some(partition_hash),
                ledger: Some(DataLedger::Submit.into()),
                slot: Some(0),
                last_updated_height: Some(10),
                entropy_packing_iterations: Some(config.consensus.entropy_packing_iterations),
                chunk_size: Some(32),
            },
        );
        inner.finish_expiry_reset();
        assert!(!inner.expiry_reset_pending);
        let assignment = storage_module.partition_assignment().expect("assigned");
        assert_eq!(assignment.ledger_id, None);
        assert_eq!(assignment.slot_index, None);
        assert_eq!(
            storage_module.get_chunk_type(&offset),
            Some(ChunkType::Uninitialized)
        );
        let (_assignment, height) = storage_module
            .partition_assignment_with_height()
            .expect("packing params");
        assert_eq!(height, Some(20));
        assert!(receivers.packing.try_recv().is_ok());
    }

    /// The reset failed, then the storage task stored Publish at the notice
    /// height and wrote a chunk. The retry must keep that slot and must not
    /// pack over it.
    #[test]
    fn finish_expiry_reset_keeps_a_ledger_installed_while_pending() {
        let (_tmp, config, storage_module, partition_hash) =
            open_expiry_module(0x83, Some((DataLedger::Submit, 0)));
        let miner = config.node_config.miner_address();
        let submit = storage_module.partition_assignment().expect("assigned");
        storage_module
            .assign_partition(submit, 10)
            .expect("store submit slot 0");
        let (mut inner, mut receivers) = expiry_service(&config, Arc::clone(&storage_module));

        storage_module.fail_next_reset_for_test();
        inner.handle_partitions_expiration(&H256List(vec![partition_hash]), 20);
        assert!(inner.expiry_reset_pending);
        assert!(storage_module.reset_incomplete());
        assert!(receivers.packing.try_recv().is_err());

        storage_module
            .assign_partition(
                PartitionAssignment {
                    partition_hash,
                    miner_address: miner,
                    ledger_id: Some(DataLedger::Publish.into()),
                    slot_index: Some(1),
                },
                20,
            )
            .expect("install publish while the reset is pending");
        let offset = PartitionChunkOffset::from(0);
        write_synced_data(&storage_module, 0x71);

        inner.finish_expiry_reset();
        assert!(!inner.expiry_reset_pending);
        let assignment = storage_module.partition_assignment().expect("assigned");
        assert_eq!(assignment.partition_hash, partition_hash);
        assert_eq!(assignment.ledger_id, Some(DataLedger::Publish.into()));
        assert_eq!(assignment.slot_index, Some(1));
        assert_eq!(
            storage_module.get_chunk_type(&offset),
            Some(ChunkType::Data)
        );
        let (_assignment, height) = storage_module
            .partition_assignment_with_height()
            .expect("packing params");
        assert_eq!(height, Some(20));
        assert!(receivers.packing.try_recv().is_err());
    }

    fn open_recall_miner(
        recall_range: u64,
    ) -> (
        irys_testing_utils::utils::tempfile::TempDir,
        Config,
        Arc<StorageModule>,
        H256,
        Arc<RwLock<VdfState>>,
    ) {
        let tmp_dir = TempDirBuilder::new().build();
        let node_config = NodeConfig {
            consensus: ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 10,
                num_chunks_in_recall_range: recall_range,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let partition_hash = H256::repeat_byte(0x77);
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(partition_chunk_offset_ie!(0, 10), "hdd0".into())],
        };
        let storage_module =
            Arc::new(StorageModule::new(&info, &config).expect("test storage module"));
        let vdf_state = Arc::new(RwLock::new(VdfState::new(
            8,
            0,
            Arc::new(AtomicBool::new(false)),
        )));
        (tmp_dir, config, storage_module, partition_hash, vdf_state)
    }

    fn recall_inner(
        config: &Config,
        storage_module: Arc<StorageModule>,
        vdf_state: Arc<RwLock<VdfState>>,
    ) -> PartitionMiningServiceInner {
        let (service_senders, _receivers) = ServiceSenders::new();
        PartitionMiningServiceInner::new(
            config,
            service_senders,
            storage_module,
            false,
            VdfStateReadonly::new(vdf_state),
            U256::zero(),
            None,
            None,
        )
    }

    /// `handle_reanchored` resets the recall rotation. The next query rebuilds
    /// from the corrected step buffer. The seed swap itself lives in `irys-vdf`.
    #[test]
    fn reanchored_event_rebuilds_recall_ranges_from_corrected_buffer() {
        // 10 / 2 is a 5-range rotation.
        let (_tmp, config, storage_module, partition_hash, vdf_state) = open_recall_miner(2);
        // Step buffer holding the "poisoned" steps 1..=3.
        for step in 1..=3_u64 {
            vdf_state
                .write()
                .unwrap()
                .store_step(Seed(H256::repeat_byte(0xA0 + step as u8)), step);
        }

        let mut inner = recall_inner(&config, storage_module, Arc::clone(&vdf_state));

        // Prime the rotation across the poisoned era (consecutive-step fast path).
        for step in 1..=3_u64 {
            inner.test_get_recall_range(step, H256::repeat_byte(step as u8), partition_hash);
        }
        assert_eq!(inner.ranges.last_step_num, 3);

        // The VDF thread re-anchors the buffer in place (steps 1..=3 rewritten onto
        // canonical seeds) and then broadcasts `Reanchored`.
        let corrected: Vec<H256> = (1..=3_u8).map(|n| H256::repeat_byte(0xB0 + n)).collect();
        vdf_state
            .write()
            .unwrap()
            .reanchor_seeds(corrected.iter().copied().map(Seed).collect());

        inner.handle_reanchored();
        assert_eq!(
            inner.ranges.last_step_num, 0,
            "handle_reanchored must reset the rotation (fresh_ranges) to force a rebuild"
        );

        // The next query must reconstruct from the corrected buffer: byte-for-byte the
        // recall range a fresh rotation fed the corrected steps [1, 3] would make.
        let seed4 = H256::repeat_byte(0x04);
        let got = inner.test_get_recall_range(4, seed4, partition_hash);

        let mut expected_ranges = Ranges::new(5).expect("5 recall ranges");
        expected_ranges
            .reconstruct(&H256List(corrected), &partition_hash)
            .expect("reconstruct from corrected steps");
        let expected = expected_ranges
            .get_recall_range(4, &seed4, &partition_hash)
            .expect("expected pick");

        assert_eq!(
            got,
            u64::try_from(expected).expect("range fits in u64"),
            "post-reanchor recall range must equal a rebuild from the corrected steps"
        );
        assert_eq!(inner.ranges.last_step_num, 4);
    }

    /// A step behind the rotation iterator must rebuild. The oracle only
    /// stepped consecutively up to that step.
    #[test]
    fn behind_step_rebuilds_rotation_instead_of_fast_pathing() {
        let (_tmp, config, storage_module, partition_hash, vdf_state) = open_recall_miner(2);
        for step in 1..=3_u64 {
            vdf_state
                .write()
                .unwrap()
                .store_step(Seed(H256::repeat_byte(0xA0 + step as u8)), step);
        }

        let mut inner = recall_inner(&config, Arc::clone(&storage_module), Arc::clone(&vdf_state));
        for step in 1..=3_u64 {
            inner.test_get_recall_range(step, H256::repeat_byte(step as u8), partition_hash);
        }
        assert_eq!(inner.ranges.last_step_num, 3);

        // Oracle: an identical service that only ever reached step 2
        // consecutively — the correct answer for (2, seed) by construction.
        let mut oracle = recall_inner(&config, storage_module, Arc::clone(&vdf_state));
        oracle.test_get_recall_range(1, H256::repeat_byte(0x01), partition_hash);
        let expected = oracle.test_get_recall_range(2, H256::repeat_byte(0x02), partition_hash);

        let got = inner.test_get_recall_range(2, H256::repeat_byte(0x02), partition_hash);
        assert_eq!(
            got, expected,
            "a behind-the-iterator step must rebuild the rotation, not answer from stale state"
        );
    }

    /// `read_chunks` skips `Uninitialized`. The solution offset is the map
    /// key. Offset 0 is a hole, so the first returned chunk is offset 1.
    /// One recall range covers the partition, so step 1 needs no VDF reads.
    /// Difficulty 0 makes that chunk win.
    #[test]
    fn solution_offset_keys_off_map_over_uninitialized_hole() {
        let (_tmp, config, storage_module, _partition_hash, vdf_state) = open_recall_miner(10);

        // Pack offsets 1..10 as Entropy, leaving offset 0 Uninitialized (the hole).
        let chunk = vec![0_u8; config.consensus.chunk_size as usize];
        for offset in 1..10_u32 {
            storage_module.write_chunk(
                PartitionChunkOffset::from(offset),
                chunk.clone(),
                ChunkType::Entropy,
            );
        }
        storage_module
            .force_sync_pending_chunks()
            .expect("sync entropy writes");

        // Sanity: read_chunks skips offset 0, so the first key is 1 at dense index 0.
        let chunks = storage_module
            .read_chunks(partition_chunk_offset_ie!(0, 10))
            .expect("read chunks");
        assert!(
            !chunks.contains_key(&PartitionChunkOffset::from(0)),
            "offset 0 must be an Uninitialized hole"
        );
        assert_eq!(
            *chunks.keys().next().expect("a returned chunk"),
            PartitionChunkOffset::from(1),
            "first returned chunk is offset 1"
        );

        let mut inner = recall_inner(&config, storage_module, vdf_state);

        let seed = H256::repeat_byte(0x11);
        // step = 1 hits get_recall_range's consecutive fast path (no VDF reads).
        let solution = inner
            .test_mine_partition_with_seed(seed, 1, &H256List::default())
            .expect("a solution at difficulty 0");

        assert_eq!(
            solution.chunk_offset, 1,
            "solution offset must be the true map key (1), not the dense index (0)"
        );
        // The PoA hash must be computed over that same true offset.
        assert_eq!(
            solution.solution_hash,
            irys_types::compute_solution_hash(&chunk, solution.chunk_offset, &seed),
            "solution hash must be over the true offset"
        );
        assert_eq!(
            inner.data_path_lookup_count(),
            0,
            "an entropy win reads no proof path"
        );
    }

    const DATA_CHUNKS: u64 = 4;

    /// One recall range covering the whole partition, so step 1 needs no VDF
    /// reconstruction. `indexed` writes a real transaction. Otherwise the
    /// chunks are `Data` bytes with no proof rows.
    fn open_data_miner(
        difficulty: U256,
        indexed: bool,
    ) -> (
        irys_testing_utils::utils::tempfile::TempDir,
        Arc<StorageModule>,
        PartitionMiningServiceInner,
        Option<irys_types::DataTransactionHeader>,
    ) {
        open_data_miner_with_tx_data(difficulty, indexed, None)
    }

    /// `tx_data` replaces the four full chunks when `indexed` is set. The
    /// partition stays [`DATA_CHUNKS`] long, so a short tail still has a slot.
    // MDBX-SUBMODULE-INDEX: this helper opens only MDBX. Retarget it at RocksDB, or delete the tests that call open_data_miner.
    fn open_data_miner_with_tx_data(
        difficulty: U256,
        indexed: bool,
        tx_data: Option<Vec<u8>>,
    ) -> (
        irys_testing_utils::utils::tempfile::TempDir,
        Arc<StorageModule>,
        PartitionMiningServiceInner,
        Option<irys_types::DataTransactionHeader>,
    ) {
        open_data_miner_for_index(
            difficulty,
            indexed,
            tx_data,
            SubmoduleIndexEngine::Mdbx,
            false,
        )
    }

    fn open_data_miner_for_index(
        difficulty: U256,
        indexed: bool,
        tx_data: Option<Vec<u8>>,
        index: SubmoduleIndexEngine,
        with_consensus_db: bool,
    ) -> (
        irys_testing_utils::utils::tempfile::TempDir,
        Arc<StorageModule>,
        PartitionMiningServiceInner,
        Option<irys_types::DataTransactionHeader>,
    ) {
        let tmp_dir = TempDirBuilder::new().build();
        let chunks = DATA_CHUNKS;
        let mut node_config = NodeConfig {
            consensus: ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: chunks,
                num_chunks_in_recall_range: chunks,
                entropy_packing_iterations: 1,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
                min_durable_interval_ms: 0,
                ..StorageSyncConfig::default()
            },
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        node_config.database.submodule_index = index;
        let config = Config::new_with_random_peer_id(node_config);
        let partition_hash = H256::repeat_byte(0x11);
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: Some(DataLedger::Submit.into()),
                slot_index: Some(0),
            }),
            submodules: vec![(partition_chunk_offset_ie!(0, chunks), "hdd0".into())],
        };
        let storage_module =
            Arc::new(StorageModule::new(&info, &config).expect("test storage module"));
        let chunk_size = config.consensus.chunk_size as usize;
        let mut indexed_header = None;
        if indexed {
            for offset in 0..chunks {
                let mut entropy = Vec::with_capacity(chunk_size);
                irys_packing::capacity_single::compute_entropy_chunk(
                    config.node_config.miner_address(),
                    offset,
                    partition_hash.0,
                    config.consensus.entropy_packing_iterations,
                    chunk_size,
                    &mut entropy,
                    config.consensus.chain_id,
                );
                assert!(storage_module.write_chunk(
                    PartitionChunkOffset::from(offset),
                    entropy,
                    ChunkType::Entropy,
                ));
            }
            storage_module
                .force_sync_pending_chunks()
                .expect("sync entropy");
            let signer = IrysSigner::random_signer(&config.consensus);
            let data = tx_data.unwrap_or_else(|| {
                (0..chunks as u8)
                    .flat_map(|byte| vec![byte; chunk_size])
                    .collect()
            });
            let tx_chunks = data.len().div_ceil(chunk_size) as u64;
            assert!(
                tx_chunks <= chunks,
                "transaction chunks must fit the partition"
            );
            let tx = signer
                .sign_transaction(
                    signer
                        .create_transaction(data, H256::zero())
                        .expect("transaction"),
                )
                .expect("signed transaction");
            let unpacked = tx.data_chunks().expect("data chunks");
            let (_, proofs) =
                DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&tx.header));
            storage_module
                .index_transaction_data(
                    &tx.header,
                    &proofs[0].proof,
                    LedgerChunkRange(ledger_chunk_offset_ii!(0, tx_chunks - 1)),
                    0,
                )
                .expect("index transaction");
            for chunk in &unpacked {
                storage_module
                    .write_data_chunk(chunk)
                    .expect("write data chunk");
            }
            indexed_header = Some(tx.header);
        } else {
            let bytes = vec![0_u8; chunk_size];
            for offset in 0..chunks {
                assert!(storage_module.write_chunk(
                    PartitionChunkOffset::from(offset),
                    bytes.clone(),
                    ChunkType::Data,
                ));
            }
        }
        storage_module
            .force_sync_pending_chunks()
            .expect("sync data chunks");
        let vdf_state = Arc::new(RwLock::new(VdfState::new(
            8,
            0,
            Arc::new(AtomicBool::new(false)),
        )));
        let (service_senders, _receivers) = ServiceSenders::new();
        let inner = PartitionMiningServiceInner::new(
            &config,
            service_senders,
            Arc::clone(&storage_module),
            false,
            VdfStateReadonly::new(vdf_state),
            difficulty,
            consensus_db(with_consensus_db, tmp_dir.path()),
            None,
        );
        (tmp_dir, storage_module, inner, indexed_header)
    }

    fn consensus_db(enabled: bool, tmp: &std::path::Path) -> Option<DatabaseProvider> {
        if !enabled {
            return None;
        }
        use irys_database::IrysDatabaseArgs as _;
        let db_dir = tmp.join("consensus");
        std::fs::create_dir_all(&db_dir).expect("consensus dir");
        let db = irys_database::open_or_create_db(
            &db_dir,
            irys_database::tables::IrysTables::ALL,
            reth_db::mdbx::DatabaseArguments::irys_testing().expect("testing args"),
        )
        .expect("consensus db");
        Some(DatabaseProvider(Arc::new(db)))
    }

    fn mine_once(inner: &mut PartitionMiningServiceInner) -> Option<SolutionContext> {
        inner.test_mine_partition_with_seed(H256::repeat_byte(0x42), 1, &H256List::default())
    }

    /// RocksDB v1 can store a data proof on an offset that no placement covers.
    /// That chunk must not start a derive or end the recall scan. The next
    /// entropy chunk is still a solution.
    #[test]
    fn unplaced_data_chunk_does_not_stop_the_recall_scan() {
        use irys_database::submodule::SubmoduleStore as _;

        let (_tmp, storage, mut inner, _header) = open_data_miner_for_index(
            U256::zero(),
            true,
            None,
            SubmoduleIndexEngine::Rocksdb,
            true,
        );
        let first = PartitionChunkOffset::from(0);
        let second = PartitionChunkOffset::from(1);
        let submodule = storage.get_submodule(first).expect("submodule");
        assert!(
            !submodule.db.stores_tx_paths(),
            "RocksDB v1 stores no tx path, so a data win derives"
        );
        assert!(
            submodule
                .db
                .view(|tx| tx.placement_at(first))
                .expect("placement read")
                .is_some(),
            "the indexed offset starts with a placement"
        );
        submodule
            .db
            .update(|tx| tx.add_tx_path_hash_to_offset_range(first, first, None))
            .expect("clear the first placement");
        assert!(
            submodule
                .db
                .view(|tx| tx.placement_at(first))
                .expect("placement read")
                .is_none()
        );
        assert!(
            submodule
                .db
                .view(|tx| tx.placement_at(second))
                .expect("placement read")
                .is_some(),
            "only the first offset loses its placement"
        );
        let (_, data_path) = storage
            .read_tx_data_paths(first, first)
            .expect("paths")
            .remove(&first)
            .expect("row");
        let data_path = data_path.expect("the data proof stays after the placement is removed");
        let recalled = storage
            .read_chunks(partition_chunk_offset_ie!(0, 1))
            .expect("recall");
        let (chunk_bytes, chunk_type) = recalled.get(&first).expect("offset 0");
        assert_eq!(*chunk_type, ChunkType::Data);
        assert!(
            inner.data_path_binds_chunk(first, chunk_bytes, &data_path),
            "the unplaced chunk is a real data win"
        );

        let chunk_size = storage.config.consensus.chunk_size as usize;
        for offset in 1..DATA_CHUNKS {
            assert!(storage.write_chunk(
                PartitionChunkOffset::from(offset as u32),
                vec![0xE5_u8; chunk_size],
                ChunkType::Entropy,
            ));
        }
        storage
            .force_sync_pending_chunks()
            .expect("sync entropy overwrite");

        let found = mine_once(&mut inner);
        if let Some(solution) = &found {
            let _ = solution.tx_path.resolve(std::time::Duration::from_secs(5));
        }
        let solution = found.expect("the entropy chunk after the unplaced data chunk wins");
        assert_eq!(solution.chunk_offset, 1);
        assert_eq!(solution.tx_path, TxPathSource::Stored(None));
    }

    #[test]
    fn data_recall_with_no_solution_reads_no_proof_path() {
        let (_tmp, _storage, mut inner, _header) = open_data_miner(U256::MAX, true);
        inner.clear_data_path_lookups();
        assert!(
            mine_once(&mut inner).is_none(),
            "only an all-0xff hash meets U256::MAX"
        );
        assert_eq!(inner.data_path_lookup_count(), 0);
    }

    #[test]
    fn winning_data_chunk_reads_its_own_paths() {
        let (_tmp, storage, mut inner, _header) = open_data_miner(U256::zero(), true);
        let offset = PartitionChunkOffset::from(0);
        let (expected_tx, expected_data) = storage
            .read_tx_data_path(LedgerChunkOffset::from(*offset))
            .expect("pre-read paths");
        let expected_tx = expected_tx.expect("tx path");
        let expected_data = expected_data.expect("data path");
        inner.clear_data_path_lookups();

        let solution = mine_once(&mut inner).expect("difficulty 0 finds the first chunk");
        assert_eq!(solution.chunk_offset, 0);
        assert_eq!(
            solution.tx_path.stored_bytes(),
            Some(expected_tx.as_slice())
        );
        assert_eq!(
            solution.data_path.as_deref(),
            Some(expected_data.as_slice())
        );
        assert_eq!(
            inner.data_path_lookup_count(),
            1,
            "one winning offset opens one path read"
        );
    }

    #[test]
    fn winning_data_chunk_with_no_stored_path_submits_nothing() {
        let (_tmp, _storage, mut inner, _header) = open_data_miner(U256::zero(), false);
        inner.clear_data_path_lookups();
        assert!(
            mine_once(&mut inner).is_none(),
            "a data win with empty paths is not submitted"
        );
        assert_eq!(
            inner.data_path_lookup_count(),
            DATA_CHUNKS,
            "each data chunk is looked up, then skipped"
        );
    }

    #[test]
    fn winning_data_chunk_whose_leaf_does_not_bind_submits_nothing() {
        let (_tmp, storage, mut inner, _header) = open_data_miner(U256::zero(), true);
        let end = PartitionChunkOffset::from(DATA_CHUNKS - 1);
        let mut before = Vec::new();
        for offset in 0..=*end {
            before.push(
                storage
                    .read_tx_data_path(LedgerChunkOffset::from(offset))
                    .expect("path before overwrite"),
            );
        }
        assert!(
            before
                .iter()
                .all(|(tx_path, data_path)| tx_path.is_some() && data_path.is_some())
        );
        let garbage = vec![0xA5_u8; 32];
        for offset in 0..DATA_CHUNKS {
            assert!(storage.write_chunk(
                PartitionChunkOffset::from(offset),
                garbage.clone(),
                ChunkType::Data,
            ));
        }
        storage
            .force_sync_pending_chunks()
            .expect("sync overwritten bytes");
        for (offset, expected) in before.iter().enumerate() {
            let after = storage
                .read_tx_data_path(LedgerChunkOffset::from(offset as u32))
                .expect("path after overwrite");
            assert_eq!(
                &after, expected,
                "overwriting chunk bytes must leave the stored paths in place"
            );
        }
        inner.clear_data_path_lookups();
        assert!(
            mine_once(&mut inner).is_none(),
            "a data win whose leaf does not bind the recall bytes is not submitted"
        );
        assert_eq!(inner.data_path_lookup_count(), DATA_CHUNKS);
    }

    #[test]
    fn short_final_chunk_binds_its_prefix_only() {
        let chunk_size = 32_usize;
        let tail = 10_usize;
        let mut data = Vec::new();
        for byte in 0..3_u8 {
            data.extend(std::iter::repeat_n(byte, chunk_size));
        }
        data.extend(std::iter::repeat_n(0x5A, tail));
        let (_tmp, storage, inner, _header) =
            open_data_miner_with_tx_data(U256::zero(), true, Some(data));
        let offset = PartitionChunkOffset::from(3);
        let read = storage
            .read_chunks(partition_chunk_offset_ie!(3, 4))
            .expect("read the short chunk");
        let (chunk_bytes, chunk_type) = read.get(&offset).expect("offset 3");
        assert_eq!(*chunk_type, ChunkType::Data);
        assert_eq!(chunk_bytes.len(), chunk_size);
        let (_, data_path) = storage
            .read_tx_data_path(LedgerChunkOffset::from(*offset))
            .expect("path read");
        let data_path = data_path.expect("data path");

        assert!(
            inner.data_path_binds_chunk(offset, chunk_bytes, &data_path),
            "the stored short chunk matches its leaf"
        );

        let mut prefix = chunk_bytes.clone();
        prefix[0] ^= 0x01;
        assert!(
            !inner.data_path_binds_chunk(offset, &prefix, &data_path),
            "a byte inside the hashed prefix must not bind"
        );

        let mut past_span = chunk_bytes.clone();
        past_span[tail] ^= 0x01;
        assert!(
            inner.data_path_binds_chunk(offset, &past_span, &data_path),
            "a byte past the short span is not part of the leaf"
        );
    }

    #[test]
    fn ledger_offset_includes_the_slot() {
        assert_eq!(ledger_chunk_offset(Some(3), 100, 7).expect("offset"), 307);
        assert!(ledger_chunk_offset(None, 100, 7).is_none());
        assert!(ledger_chunk_offset(Some(usize::MAX), u64::MAX, 1).is_none());
    }

    #[test]
    fn missing_tx_path_without_a_database_submits_nothing() {
        let (_tmp, storage, mut inner, _header) = open_data_miner(U256::zero(), true);
        clear_stored_tx_paths(&storage);
        let (tx_path, data_path) = storage
            .read_tx_data_paths(PartitionChunkOffset::from(0), PartitionChunkOffset::from(0))
            .expect("read")
            .remove(&PartitionChunkOffset::from(0))
            .expect("row");
        assert!(tx_path.is_none());
        assert!(data_path.is_some());
        inner.clear_data_path_lookups();
        assert!(
            mine_once(&mut inner).is_none(),
            "a data win whose tx path cannot be derived is not submitted"
        );
    }

    #[test]
    fn missing_stored_tx_path_derives_the_merkle_proof() {
        let (tmp, storage, _inner, header) = open_data_miner(U256::zero(), true);
        let header = header.expect("indexed header");
        let expected = storage
            .read_tx_data_path(LedgerChunkOffset::from(0))
            .expect("stored read")
            .0
            .expect("stored tx path");
        clear_stored_tx_paths(&storage);
        let db_dir = tmp.path().join("consensus");
        std::fs::create_dir_all(&db_dir).expect("consensus dir");
        let db = write_submit_chain(&db_dir, &header, storage.config.consensus.chunk_size);
        let pool =
            Arc::new(irys_database::tx_path_derive::tx_path_derive_pool(2).expect("tx path pool"));
        let vdf_state = Arc::new(RwLock::new(VdfState::new(
            8,
            0,
            Arc::new(AtomicBool::new(false)),
        )));
        let (service_senders, _receivers) = ServiceSenders::new();
        let mut inner = PartitionMiningServiceInner::new(
            &storage.config,
            service_senders,
            Arc::clone(&storage),
            false,
            VdfStateReadonly::new(vdf_state),
            U256::zero(),
            Some(db),
            Some(pool),
        );
        let solution = mine_once(&mut inner).expect("difficulty 0 finds the first chunk");
        let derived = solution
            .tx_path
            .resolve(std::time::Duration::from_secs(5))
            .expect("derive")
            .expect("derived path");
        assert_eq!(derived, expected);
    }

    fn clear_stored_tx_paths(storage: &StorageModule) {
        use irys_database::submodule::SubmoduleStore as _;
        let end = PartitionChunkOffset::from((DATA_CHUNKS - 1) as u32);
        let submodule = storage
            .get_submodule(PartitionChunkOffset::from(0))
            .expect("submodule");
        submodule
            .db
            .update(|tx| {
                tx.add_tx_path_hash_to_offset_range(PartitionChunkOffset::from(0), end, None)
            })
            .expect("clear tx paths");
    }

    fn write_submit_chain(
        dir: &std::path::Path,
        header: &irys_types::DataTransactionHeader,
        chunk_size: u64,
    ) -> DatabaseProvider {
        use irys_database::db::IrysDatabaseExt as _;
        use irys_database::tables::IrysTables;
        use irys_database::{
            IrysDatabaseArgs as _, insert_block_header, insert_block_index_items_for_block,
            insert_tx_header, open_or_create_db,
        };
        use irys_types::IrysBlockHeader;
        use reth_db::mdbx::DatabaseArguments;

        let chunks = header.data_size.div_ceil(chunk_size);
        let mut genesis = IrysBlockHeader::new_mock_header();
        genesis.height = 0;
        genesis.block_hash = H256::repeat_byte(1);
        set_submit(&mut genesis, &[], 0);
        let mut block = IrysBlockHeader::new_mock_header();
        block.height = 1;
        block.block_hash = H256::repeat_byte(2);
        set_submit(&mut block, std::slice::from_ref(header), chunks);
        let db = open_or_create_db(
            dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing().expect("testing args"),
        )
        .expect("open consensus db");
        db.update_eyre(|tx| {
            insert_tx_header(tx, header)?;
            for block in [&genesis, &block] {
                insert_block_header(tx, block)?;
                insert_block_index_items_for_block(tx, block)?;
            }
            Ok(())
        })
        .expect("write chain");
        DatabaseProvider(Arc::new(db))
    }

    fn set_submit(
        block: &mut irys_types::IrysBlockHeader,
        txs: &[irys_types::DataTransactionHeader],
        total: u64,
    ) {
        let (root, _) = DataTransactionLedger::merklize_tx_root(txs);
        let slot = block
            .data_ledgers
            .iter_mut()
            .find(|item| item.ledger_id == DataLedger::Submit.get_id())
            .expect("submit ledger");
        slot.tx_ids = H256List(txs.iter().map(|header| header.id).collect());
        slot.tx_root = root;
        slot.total_chunks = total;
    }

    /// `first_data_offset` calls `get_intervals` once at startup. That walk
    /// visits every fragment. Non-adjacent intervals do not merge, so a crash
    /// can leave one interval per chunk. A quadratic scan of 100k fragments
    /// does not finish inside this bound.
    #[test]
    fn get_intervals_over_100k_fragments_stays_within_ten_seconds() {
        const CHUNKS: u32 = 100_000;
        let setup_started = std::time::Instant::now();
        let tmp_dir = TempDirBuilder::new().prefix("fragmented_intervals").build();
        let node_config = NodeConfig {
            consensus: ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: u64::from(CHUNKS),
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: H256::repeat_byte(0x44),
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(
                partition_chunk_offset_ie!(0, CHUNKS),
                tmp_dir.path().join("hdd0"),
            )],
        };
        let storage = StorageModule::new(&info, &config).expect("storage module");
        {
            let mut intervals = storage.intervals().write().expect("intervals");
            for offset in 0..CHUNKS {
                let kind = if offset % 2 == 0 {
                    ChunkType::Uninitialized
                } else {
                    ChunkType::Entropy
                };
                StorageModule::cut_then_insert_interval_if_touching(
                    &mut intervals,
                    PartitionChunkOffset::from(offset),
                    kind,
                );
            }
        }
        let setup = setup_started.elapsed();
        let entropy_started = std::time::Instant::now();
        let entropy = storage.get_intervals(ChunkType::Entropy);
        let entropy_elapsed = entropy_started.elapsed();
        let uninit_started = std::time::Instant::now();
        let uninit = storage.get_intervals(ChunkType::Uninitialized);
        let uninit_elapsed = uninit_started.elapsed();
        let data_started = std::time::Instant::now();
        let data = storage.get_intervals(ChunkType::Data);
        let data_elapsed = data_started.elapsed();
        eprintln!(
            "fragmented intervals: chunks={CHUNKS} setup={setup:?} entropy={} in {entropy_elapsed:?} uninit={} in {uninit_elapsed:?} data={} in {data_elapsed:?}",
            entropy.len(),
            uninit.len(),
            data.len(),
        );
        assert_eq!(entropy.len(), usize::try_from(CHUNKS / 2).expect("half"));
        assert_eq!(uninit.len(), usize::try_from(CHUNKS / 2).expect("half"));
        assert!(data.is_empty());
        let budget = std::time::Duration::from_secs(10);
        assert!(
            entropy_elapsed < budget && uninit_elapsed < budget && data_elapsed < budget,
            "get_intervals over {CHUNKS} fragments exceeded {budget:?}: entropy={entropy_elapsed:?} uninit={uninit_elapsed:?} data={data_elapsed:?}"
        );
    }
}
