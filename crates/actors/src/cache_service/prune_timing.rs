//! Capacity-prune read counters, the manual timing fixture, and a check that
//! one pass walks each block once. Counters are absent from production builds.

use super::*;
use irys_database::{
    IrysDatabaseArgs as _, database,
    db_cache::{CachedChunk, CachedChunkIndexEntry, CachedChunkIndexMetadata},
    open_or_create_db,
    tables::{CachedChunks, CachedChunksIndex, IrysBlockHeaders, IrysTables, MigratedBlockHashes},
};
use irys_domain::{
    BlockIndex, BlockTree, ChunkType, CommitmentSnapshot, StorageModule, StorageModuleInfo,
    dummy_ema_snapshot,
};
use irys_testing_utils::{IrysBlockHeaderTestExt as _, utils::TempDirBuilder};
use irys_types::{
    Base64, BlockTransactions, ConsensusConfig, ConsensusOptions, DataTransactionHeaderV1,
    DataTransactionHeaderV1WithMetadata, DataTransactionLedger, DataTransactionMetadata,
    DbSyncMode, H256List, LedgerChunkRange, NodeConfig, PartitionChunkOffset, SealedBlock,
    SubmoduleIndexEngine, U256, UnpackedChunk, partition::PartitionAssignment,
    partition_chunk_offset_ii,
};
use nodit::interval::ii;
use reth_db::mdbx::DatabaseArguments;
use std::{cell::Cell, ops::RangeInclusive, sync::RwLock, time::Instant};

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
struct Reads {
    root_headers: u64,
    prefix_headers: u64,
    canonical_lookups: u64,
    canonical_database_lookups: u64,
    assignment_nanos: u128,
}

thread_local! {
    static READS: Cell<Option<Reads>> = const { Cell::new(None) };
    static SPAN_CACHE_DISABLED: Cell<bool> = const { Cell::new(false) };
}

pub(super) fn span_cache_disabled() -> bool {
    SPAN_CACHE_DISABLED.get()
}

pub(super) enum Read {
    RootHeader,
    PrefixHeader,
    CanonicalLookup,
    CanonicalDatabaseLookup,
}

pub(super) fn record(read: Read) {
    READS.with(|cell| {
        if let Some(mut counts) = cell.get() {
            match read {
                Read::RootHeader => counts.root_headers += 1,
                Read::PrefixHeader => counts.prefix_headers += 1,
                Read::CanonicalLookup => counts.canonical_lookups += 1,
                Read::CanonicalDatabaseLookup => counts.canonical_database_lookups += 1,
            }
            cell.set(Some(counts));
        }
    });
}

pub(super) struct AssignmentTimer(Option<Instant>);

impl AssignmentTimer {
    pub(super) fn start() -> Self {
        Self(READS.with(|cell| cell.get().is_some()).then(Instant::now))
    }
}

impl Drop for AssignmentTimer {
    fn drop(&mut self) {
        if let Some(start) = self.0 {
            READS.with(|cell| {
                if let Some(mut counts) = cell.get() {
                    counts.assignment_nanos += start.elapsed().as_nanos();
                    cell.set(Some(counts));
                }
            });
        }
    }
}

fn setting(name: &str, default: usize, range: RangeInclusive<usize>) -> eyre::Result<usize> {
    let value = match std::env::var(name) {
        Ok(value) => value.parse::<usize>()?,
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => return Err(error.into()),
    };
    eyre::ensure!(range.contains(&value), "{name} must be in {range:?}");
    Ok(value)
}

fn fixture_hash(index: usize) -> eyre::Result<H256> {
    Ok(irys_types::hash_sha256(&u64::try_from(index)?.to_le_bytes()).into())
}

fn submit_tx(id: H256, data_root: H256, data_size: u64) -> DataTransactionHeader {
    DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
        tx: DataTransactionHeaderV1 {
            id,
            data_root,
            data_size,
            ledger_id: DataLedger::Submit.into(),
            ..Default::default()
        },
        metadata: DataTransactionMetadata::new(),
    })
}

fn timing_task(
    db: DatabaseProvider,
    config: Config,
    tree: BlockTree,
    storage_modules: Vec<Arc<StorageModule>>,
) -> InnerCacheTask {
    InnerCacheTask {
        block_index_guard: BlockIndexReadGuard::new(BlockIndex::new_for_testing(db.clone())),
        db,
        block_tree_guard: BlockTreeReadGuard::new(Arc::new(RwLock::new(tree))),
        config,
        gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
        ingress_proof_generation_state: IngressProofGenerationState::new(),
        cache_sender: tokio::sync::mpsc::unbounded_channel().0,
        storage_modules_guard: StorageModulesReadGuard::new(Arc::new(RwLock::new(storage_modules))),
        prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
    }
}

/// Resolving every tx of one block reads each tx header once and looks up
/// the block and its parent once, whatever order the txs are resolved in.
#[rstest::rstest]
#[case::ledger_order([0, 1, 2])]
#[case::reverse_order([2, 1, 0])]
fn span_cache_walks_each_block_once(#[case] order: [usize; 3]) -> eyre::Result<()> {
    let temp = TempDirBuilder::new().prefix("span_cache_walk").build();
    let config = Config::new_with_random_peer_id(NodeConfig::testing());
    let chunk_size = config.consensus.chunk_size;
    let db = DatabaseProvider(Arc::new(open_or_create_db(
        temp.path(),
        IrysTables::ALL,
        DatabaseArguments::irys_testing()?,
    )?));
    let mut genesis = IrysBlockHeader::new_mock_header();
    genesis.height = 0;
    genesis.data_ledgers[DataLedger::Submit].total_chunks = 4;
    genesis.test_sign();
    let headers: Vec<_> = (1..=3_u64)
        .map(|chunks| submit_tx(H256::random(), H256::random(), chunks * chunk_size))
        .collect();
    let mut block = IrysBlockHeader::new_mock_header();
    block.height = 1;
    block.previous_block_hash = genesis.block_hash;
    let submit = &mut block.data_ledgers[DataLedger::Submit];
    submit.total_chunks = 10;
    submit.tx_ids = H256List(headers.iter().map(|header| header.id).collect());
    block.test_sign();
    db.update_eyre(|tx| {
        for header in &headers {
            irys_database::insert_tx_header(tx, header)?;
        }
        tx.put::<IrysBlockHeaders>(block.block_hash, block.clone().into())?;
        tx.put::<MigratedBlockHashes>(1, block.block_hash)?;
        Ok(())
    })?;
    let tree = BlockTree::new(&genesis, config.consensus.clone());
    let task = timing_task(db, config, tree, Vec::new());

    let mut span_cache = PruneSpanCache::default();
    let mut starts = [0_u64; 3];
    READS.set(Some(Reads::default()));
    for index in order {
        let span = task
            .tx_span_in_block(headers[index].id, DataLedger::Submit, 1, &mut span_cache)?
            .ok_or_else(|| eyre::eyre!("tx {index} has no span"))?;
        starts[index] = *span.start;
    }
    let reads = READS
        .take()
        .ok_or_else(|| eyre::eyre!("read counters absent"))?;

    assert_eq!(
        starts,
        [4, 5, 7],
        "spans follow ledger order from the parent's total"
    );
    assert_eq!(reads.prefix_headers, 3, "each tx header is read once");
    assert_eq!(
        reads.canonical_lookups, 2,
        "the block and its parent are looked up once"
    );
    Ok(())
}

/// Run alone with `--ignored --nocapture`; setup and cache refills are untimed.
/// Headers and bodies are synthetic: this measures pruning, not validation or ingress.
#[test]
#[ignore = "manual capacity-prune timing; writes several GiB at default settings"]
fn slow_timed_capacity_prune() -> eyre::Result<()> {
    let roots = setting(
        "IRYS_PRUNE_TIMING_ROOTS",
        MAX_EVICTIONS_PER_RUN,
        1..=MAX_EVICTIONS_PER_RUN,
    )?;
    let per_block = setting("IRYS_PRUNE_TIMING_TXS_PER_BLOCK", 100, 1..=1_000)?;
    let copies = setting("IRYS_PRUNE_TIMING_COPIES", 1, 1..=2)?;
    let body_bytes = setting("IRYS_PRUNE_TIMING_BODY_BYTES", 256 * 1024, 1..=256 * 1024)?;
    let samples = setting("IRYS_PRUNE_TIMING_SAMPLES", 3, 1..=10)?;
    // Fixture blocks held in the in-memory tree; 0 keeps only genesis there.
    let tree_blocks = setting("IRYS_PRUNE_TIMING_TREE_BLOCKS", 0, 0..=1_000)?;
    // 0 walks every span afresh: the per-tx cost before the span cache.
    let span_cache = setting("IRYS_PRUNE_TIMING_SPAN_CACHE", 1, 0..=1)? == 1;
    SPAN_CACHE_DISABLED.set(!span_cache);
    let transactions = roots * copies;
    let block_count = transactions.div_ceil(per_block);
    eyre::ensure!(
        tree_blocks <= block_count,
        "IRYS_PRUNE_TIMING_TREE_BLOCKS exceeds the {block_count} fixture blocks"
    );
    let root_count = u64::try_from(roots)?;
    let chunk_size = u64::try_from(body_bytes)?;
    let temp = TempDirBuilder::new()
        .prefix("capacity_prune_timing")
        .build();
    let testing = ConsensusConfig::testing();
    let mut node_config = NodeConfig {
        base_directory: temp.path().join("storage"),
        consensus: ConsensusOptions::Custom(ConsensusConfig {
            chunk_size,
            num_chunks_in_partition: u64::try_from(transactions)?,
            num_chunks_in_recall_range: 1,
            num_partitions_per_slot: 1,
            entropy_packing_iterations: 1,
            block_tree_depth: u64::try_from(tree_blocks)?.max(testing.block_tree_depth),
            ..testing
        }),
        ..NodeConfig::testing()
    };
    node_config.database.submodule_index = SubmoduleIndexEngine::Rocksdb;
    node_config.cache.max_cache_size_bytes = root_count * chunk_size / 2;
    let config = Config::new_with_random_peer_id(node_config);
    let local_address = config.irys_signer().address();
    let db = DatabaseProvider(Arc::new(open_or_create_db(
        temp.path().join("cache"),
        IrysTables::ALL,
        DatabaseArguments::irys_default(DbSyncMode::Durable)?
            .with_geometry_max_size(Some(16 * 1024 * 1024 * 1024)),
    )?));
    let storage = Arc::new(StorageModule::new(
        &StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                ledger_id: Some(DataLedger::Submit.into()),
                slot_index: Some(0),
                miner_address: local_address,
                partition_hash: H256::repeat_byte(7),
            }),
            submodules: vec![(
                partition_chunk_offset_ii!(0, u32::try_from(transactions - 1)?),
                "chunks".into(),
            )],
        },
        &config,
    )?);
    let mut genesis = IrysBlockHeader::new_mock_header();
    genesis.height = 0;
    genesis.previous_block_hash = H256::zero();
    // Below every fixture block, so the tree's canonical chain follows them.
    genesis.cumulative_diff = U256::zero();
    genesis.data_ledgers[DataLedger::Submit].total_chunks = 0;
    genesis.test_sign();
    db.update_eyre(|tx| {
        tx.put::<IrysBlockHeaders>(genesis.block_hash, genesis.clone().into())?;
        tx.put::<MigratedBlockHashes>(0, genesis.block_hash)?;
        Ok(())
    })?;

    let setup_start = Instant::now();
    let mut previous = genesis.block_hash;
    let mut tree_headers = Vec::with_capacity(tree_blocks);
    for first in (0..transactions).step_by(per_block) {
        let end = (first + per_block).min(transactions);
        let height = u64::try_from(first / per_block + 1)?;
        let headers = (first..end)
            .map(|index| {
                Ok(submit_tx(
                    fixture_hash(transactions + index)?,
                    fixture_hash(index % roots)?,
                    chunk_size,
                ))
            })
            .collect::<eyre::Result<Vec<_>>>()?;
        let (tx_root, paths) = DataTransactionLedger::merklize_tx_root(&headers);
        let mut block = IrysBlockHeader::new_mock_header();
        block.height = height;
        block.previous_block_hash = previous;
        block.cumulative_diff = U256::from(height + 1);
        let ledger = &mut block.data_ledgers[DataLedger::Submit];
        ledger.total_chunks = u64::try_from(end)?;
        ledger.tx_ids = H256List(headers.iter().map(|header| header.id).collect());
        ledger.tx_root = tx_root;
        block.test_sign();
        db.update_eyre(|tx| {
            for header in &headers {
                database::cache_data_root(tx, header, None)?;
                irys_database::insert_tx_header(tx, header)?;
                irys_database::set_data_tx_included_height(tx, &header.id, height)
                    .map_err(|error| eyre::eyre!("set inclusion: {error:?}"))?;
            }
            tx.put::<IrysBlockHeaders>(block.block_hash, block.clone().into())?;
            tx.put::<MigratedBlockHashes>(height, block.block_hash)?;
            Ok(())
        })?;
        for (index, (header, path)) in headers.iter().zip(paths).enumerate() {
            let offset = u64::try_from(first + index)?;
            storage.index_transaction_data(
                header,
                &path.proof,
                LedgerChunkRange(nodit::interval::ii(offset.into(), offset.into())),
                height,
            )?;
            eyre::ensure!(
                storage.write_chunk(
                    PartitionChunkOffset::from(u32::try_from(offset)?),
                    vec![42; body_bytes],
                    ChunkType::Data,
                ),
                "fixture body write rejected"
            );
        }
        storage.force_sync_pending_chunks()?;
        previous = block.block_hash;
        if height <= u64::try_from(tree_blocks)? {
            tree_headers.push(block);
        }
    }
    for index in 0..transactions {
        eyre::ensure!(
            storage.is_data_chunk_persisted_at(PartitionChunkOffset::from(u32::try_from(index)?)),
            "fixture placement {index} is not persisted"
        );
    }
    let migration_height = u64::try_from(block_count)?;
    // A node keeps `block_tree_depth` canonical blocks in memory. Every
    // canonical lookup clones that chain under the tree lock.
    let mut tree = BlockTree::new(&genesis, config.consensus.clone());
    for block in tree_headers {
        let sealed = Arc::new(SealedBlock::new_unchecked(
            Arc::new(block),
            BlockTransactions::default(),
        ));
        tree.add_common(
            sealed.header().block_hash,
            &sealed,
            Arc::new(CommitmentSnapshot::default()),
            Arc::new(EpochSnapshot::default()),
            dummy_ema_snapshot(),
            ChainState::Onchain,
        )?;
    }
    eyre::ensure!(
        tree.get_canonical_chain().0.len() == tree_blocks + 1,
        "the canonical chain must hold genesis and every tree block"
    );
    let task = timing_task(db.clone(), config, tree, vec![Arc::clone(&storage)]);
    eprintln!(
        "TIMED_PRUNE_SETUP seconds={:.3}",
        setup_start.elapsed().as_secs_f64()
    );
    let body = vec![42; body_bytes];
    let ingress_hash = irys_types::generate_ingress_data_hash(&body, local_address);
    // Every fixture tx is resolved. Cached, each block is walked once to its
    // end; uncached, each tx walks its block's prefix up to itself.
    let (expected_prefix_reads, expected_canonical_lookups) = if span_cache {
        (u64::try_from(transactions)?, 2 * migration_height)
    } else {
        let prefix = (0..transactions).step_by(per_block).try_fold(
            0_u64,
            |total, start| -> eyre::Result<u64> {
                let count = u64::try_from((transactions - start).min(per_block))?;
                Ok(total + count * (count + 1) / 2)
            },
        )?;
        (prefix, 2 * u64::try_from(transactions)?)
    };
    for sample in 0..samples {
        for first in (0..roots).step_by(256) {
            db.update_eyre(|tx| {
                for index in first..(first + 256).min(roots) {
                    let root = fixture_hash(index)?;
                    let chunk = UnpackedChunk {
                        data_root: root,
                        data_size: chunk_size,
                        data_path: Base64(root.as_bytes().to_vec()),
                        bytes: Base64(body.clone()),
                        tx_offset: TxChunkOffset::from(0),
                    };
                    let path_hash = chunk.chunk_path_hash();
                    tx.put::<CachedChunks>(path_hash, CachedChunk::from(&chunk))?;
                    tx.put::<CachedChunksIndex>(
                        root,
                        CachedChunkIndexEntry {
                            index: chunk.tx_offset,
                            meta: CachedChunkIndexMetadata {
                                chunk_path_hash: path_hash,
                                updated_at: UnixTimestamp::from_secs(0),
                            },
                        },
                    )?;
                    irys_database::store_ingress_data_hash(
                        tx,
                        root,
                        local_address,
                        chunk.tx_offset,
                        ingress_hash,
                    )?;
                }
                Ok(())
            })?;
        }
        let before = db.view_eyre(|tx| Ok(tx.entries::<CachedChunks>()?))?;
        assert_eq!(before, roots);
        READS.set(Some(Reads::default()));
        let start = Instant::now();
        let result = task.prune_cache(migration_height);
        let elapsed = start.elapsed();
        let reads = READS
            .take()
            .ok_or_else(|| eyre::eyre!("timing counters absent"))?;
        result?;
        let remaining = db.view_eyre(|tx| Ok(tx.entries::<CachedChunks>()?))?;
        assert_eq!(
            remaining, 0,
            "the measured pass must delete every eligible body"
        );
        assert_eq!(reads.root_headers, u64::try_from(transactions)?);
        assert_eq!(reads.prefix_headers, expected_prefix_reads);
        assert_eq!(reads.canonical_lookups, expected_canonical_lookups);
        println!(
            "TIMED_PRUNE {}",
            serde_json::json!({
                "roots": roots, "transactions_per_block": per_block, "copies": copies,
                "body_bytes": body_bytes, "tree_blocks": tree_blocks,
                "span_cache": span_cache, "sample": sample,
                "deleted": before - remaining,
                "elapsed_ms": elapsed.as_secs_f64() * 1000.0,
                "reads": reads,
                "debug_assertions": cfg!(debug_assertions), "cache_state": "warm_after_fixture_or_refill",
                "mdbx_sync_mode": "Durable", "storage_index": "Rocksdb"
            })
        );
    }
    Ok(())
}
