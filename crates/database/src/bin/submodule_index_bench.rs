//! Write one synthetic submodule index on MDBX and on RocksDB.
//!
//! Prints wall time and directory bytes. The path must contain an
//! `index-bench` component and must be empty. `--profile filter-reuse` is
//! the exception: it opens an existing `rocks/` directory and refuses the
//! path when `mdbx/` is also present. This binary does not open a live
//! node database.
//!
//! ```text
//! cargo run -p irys-database --features rocksdb --bin submodule-index-bench -- \
//!   --dir /tmp/index-bench --chunks 4096
//! ```
//!
//! `--worst-case` stores a max-size data proof on every chunk. A full
//! partition is `--chunks 75534400` and needs `IRYS_INDEX_BENCH_LARGE=1`.
//! `--small-txs N` fills the same span with N-chunk transactions, each with
//! its own data root. `--mid-txs` uses transactions of 257 to 4096 chunks.
//! `--mid` writes 1_000_000 chunks of 4096-byte paths, past an HDD cache.
//! It needs the same variable. `--profile filled|mid|worst` selects the
//! older workloads. `--rocks <preset>` changes one RocksDB setting.
//! `--engine rocks` skips MDBX. `--schema v2|v3` selects the RocksDB layout
//! of an empty directory. The default is v2. Production open of an empty
//! directory is schema v3. MDBX stays schema v2.
//!
//! `--data-fraction F` writes transactions and data rows for the first
//! `F × --chunks` offsets. `--with-crc` stores each of those rows with the
//! schema 3.2 checksum trailer. `--entropy-fill` writes a checksum-only row
//! for every other offset. Both need `--schema v3` on RocksDB. `entropy_range`
//! and `recall_rows` time cold reads of those rows. They are skipped when the
//! data region covers the whole partition.
//!
//! After the writes, each engine drops its file cache with `posix_fadvise`
//! and times random lookups. This does not call `drop_caches`.

use std::fs::{self, File};
use std::io::Error;
use std::os::unix::io::AsRawFd as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use irys_database::IrysDatabaseArgs as _;
use irys_database::submodule::tables::{
    CHUNK_META_BY_OFFSET_CF, ChunkPathHashes, DataRootInfo, OffsetMeta, PackedOffsetRow,
    PendingBodyMigration, TxLeafBinding,
};
use irys_database::submodule::{
    LegacySstSpan, RocksTuning, SubmoduleIndex, SubmoduleStore as _, encode_offset_meta,
};
use irys_types::{DbSyncMode, H256, PartitionChunkOffset, RelativeChunkOffset, TERABYTE};
use reth_db::mdbx::DatabaseArguments;

/// Mainnet partition size (`CHUNKS_PER_PARTITION_20TB`).
const PARTITION_CHUNKS: u64 = 75_534_400;
/// Mainnet max data transaction, in chunks (5 TiB at 256 KiB).
const MAX_DATA_TX_CHUNKS: u64 = 20_971_520;
/// Mainnet max data transactions in one block. Sizes the tx-path proof.
const MAX_DATA_TXS_PER_BLOCK: u64 = 100;
const CHUNK_SIZE: u64 = 256 * 1024;
/// Same widths as the merkle proof encoder: 32-byte ids, 32-byte notes.
const HASH_SIZE: usize = 32;
const NOTE_SIZE: usize = 32;
const BRANCH_SIZE: usize = HASH_SIZE * 2 + NOTE_SIZE;
const LEAF_SIZE: usize = HASH_SIZE + NOTE_SIZE;
/// Raw proof bytes above this need `IRYS_INDEX_BENCH_LARGE=1`.
const LARGE_RAW_BYTES: u64 = 8 << 30;
/// Filled run past a typical HDD cache. 4096-byte paths, about 4 GiB raw.
const MID_CHUNKS: u32 = 1_000_000;
/// Transactions in one grouped commit. `--mid` uses this for MDBX.
/// `--group-commit` uses it for both engines.
const MID_MDBX_TXS_PER_COMMIT: u32 = 8;
/// Data-path rows in one grouped commit. `--mid` uses this for MDBX.
/// `--group-commit` uses it for both engines. Rocks without the flag stays
/// at `--batch`.
const MID_MDBX_PATH_BATCH: u32 = 2048;
/// First samples reported apart from the rest, in arrival order.
const HEAD_SAMPLES: usize = 32;
/// Filled runs at or below this stay inside an 8 GiB map. `--mid` is above it.
const FILLED_MAP_CEILING: u64 = 8 << 30;

/// How many times pairing reduces `n` leaves to one root.
const fn pairing_layers(mut n: u64) -> usize {
    let mut layers = 0;
    while n > 1 {
        n = n.div_ceil(2);
        layers += 1;
    }
    layers
}

const fn proof_bytes(layers: usize) -> usize {
    layers * BRANCH_SIZE + LEAF_SIZE
}

/// Deepest data proof of a max-size transaction: 25 branches + one leaf.
const DATA_PROOF_LAYERS: usize = pairing_layers(MAX_DATA_TX_CHUNKS);
const DATA_PROOF_BYTES: usize = proof_bytes(DATA_PROOF_LAYERS);
/// Deepest tx proof of a full block: 7 branches + one leaf.
const TX_PROOF_LAYERS: usize = pairing_layers(MAX_DATA_TXS_PER_BLOCK);
const TX_PROOF_BYTES: usize = proof_bytes(TX_PROOF_LAYERS);
/// `--mid-txs` draws a size from this inclusive range.
const MID_TX_MIN: u32 = 257;
const MID_TX_MAX: u32 = 4096;
const MID_TX_SEED: u64 = 0x5EED_71E5;
/// Seed for the `--shuffle-paths` run order.
const SHUFFLE_SEED: u64 = 0x5EED_5A1F;

/// Workload shape. `mid` and `worst` are also the old flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Workload {
    Filled,
    Mid,
    Worst,
    /// N Rocks databases left open, so cache RSS can add up.
    MemDbs,
    /// One `add_tx_path_hash_to_offset_range` covering every offset.
    IndexSpan,
    /// Offset rows only, so the bloom can outgrow the block cache.
    OffsetRows,
    /// Point reads of a schema-v1 `ChunkPathHashesByOffset` SST.
    FilterReuse,
    /// `N` chunks per transaction. Each transaction has its own data root.
    SmallTxs,
    /// Transactions of [`MID_TX_MIN`] to [`MID_TX_MAX`] chunks, one root each.
    MidTxs,
    /// SST bytes of the offset row with and without the checksum trailer.
    CrcRows,
}

impl Workload {
    fn name(self) -> &'static str {
        match self {
            Self::Filled => "filled",
            Self::Mid => "mid",
            Self::Worst => "worst",
            Self::MemDbs => "mem-dbs",
            Self::IndexSpan => "index-span",
            Self::OffsetRows => "offset-rows",
            Self::FilterReuse => "filter-reuse",
            Self::SmallTxs => "small-txs",
            Self::MidTxs => "mid-txs",
            Self::CrcRows => "crc-rows",
        }
    }

    fn is_filter_reuse(self) -> bool {
        matches!(self, Self::FilterReuse)
    }

    fn is_memory(self) -> bool {
        matches!(self, Self::MemDbs | Self::IndexSpan | Self::OffsetRows)
    }

    fn is_mid(self) -> bool {
        matches!(self, Self::Mid)
    }

    fn is_worst(self) -> bool {
        matches!(self, Self::Worst)
    }

    /// Proof bytes come from one data tree per transaction.
    fn is_tree(self) -> bool {
        matches!(self, Self::Worst | Self::SmallTxs | Self::MidTxs)
    }

    /// Many data roots. The per-transaction rows are the bulk of the cost.
    fn is_many_tx(self) -> bool {
        matches!(self, Self::SmallTxs | Self::MidTxs)
    }
}

/// One transaction's stored span. `leaves` is the full data tree.
struct TxPlace {
    start: u32,
    stored: u32,
    leaves: u64,
}

/// RocksDB layout written into an empty directory. MDBX stays schema v2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IndexSchema {
    V2,
    V3,
}

impl IndexSchema {
    fn name(self) -> &'static str {
        match self {
            Self::V2 => "v2",
            Self::V3 => "v3",
        }
    }

    fn is_v3(self) -> bool {
        matches!(self, Self::V3)
    }
}

/// Which engines to write. A preset sweep uses `Rocks` so MDBX is not repeated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Engines {
    Both,
    Mdbx,
    Rocks,
}

impl Engines {
    fn name(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Mdbx => "mdbx",
            Self::Rocks => "rocks",
        }
    }

    fn includes_mdbx(self) -> bool {
        matches!(self, Self::Both | Self::Mdbx)
    }

    fn includes_rocks(self) -> bool {
        matches!(self, Self::Both | Self::Rocks)
    }
}

struct Args {
    /// `--worst-case` writes one unrelated random proof per chunk (the layout of
    /// runs before the shared-node generator). Off by default: real proofs of one
    /// transaction share most tree nodes, which is what inline storage compresses.
    uniform_proofs: bool,
    dir: PathBuf,
    chunks: u32,
    batch: u32,
    /// Offsets covered by one transaction commit.
    tx_chunks: u32,
    path_bytes: usize,
    workload: Workload,
    engines: Engines,
    /// Random samples of each read shape. Zero skips the read phase.
    reads: u32,
    /// Inclusive offset window for one range lookup.
    range_len: u32,
    /// RocksDB open. MDBX ignores this. Production open stays on the baseline.
    rocks: RocksTuning,
    /// Databases kept open together. `mem-dbs` only.
    dbs: u32,
    /// Both engines use the `--mid` MDBX commit sizes.
    group_commit: bool,
    /// Compact the v1 path-hash family before the timed reads.
    rewrite_filters: bool,
    /// Chunks in each `--small-txs` transaction. Zero for other workloads.
    small_tx_chunks: u32,
    /// `(start, stored)` for `--mid-txs`. Empty otherwise.
    mid_spans: Vec<(u32, u32)>,
    /// Data paths arrive as runs of this many chunks in a seeded random
    /// order, like scattered data sync. `None` writes them in offset order.
    shuffle_run: Option<u32>,
    /// Overrides the transaction rows per commit. Production commits one
    /// block's transactions together.
    txs_per_commit: Option<u32>,
    /// RocksDB layout. MDBX ignores this and stays schema v2.
    schema: IndexSchema,
    /// Printed `--data-fraction`. `1` when the flag is absent.
    data_fraction: f64,
    /// First offsets that receive transactions, placements, and data rows.
    data_chunks: u32,
    /// Data rows use the schema 3.2 checksum trailer.
    with_crc: bool,
    /// Offsets outside the data region get a checksum-only row.
    entropy_fill: bool,
}

/// Every open gets its own block cache, sized like the production shared
/// cache. The process-wide shared cache survives a reopen, so a cold shape
/// would hit blocks an earlier shape loaded (serve samples overlap the random
/// shape's offsets). Presets that already chose a private cache keep it.
fn open_bench_rocks(path: &Path, args: &Args) -> eyre::Result<SubmoduleIndex> {
    let tuning = if args.rocks.share_resources {
        args.rocks
            .with_block_cache(irys_types::DEFAULT_ROCKSDB_BLOCK_CACHE_BYTES as usize)
    } else {
        args.rocks
    };
    SubmoduleIndex::open_rocks_bench(path, tuning, args.schema.is_v3())
}

fn main() -> eyre::Result<()> {
    let args = parse_args()?;
    refuse_large_payload(&args)?;
    prepare_dir(&args.dir, args.workload.is_filter_reuse())?;
    println!(
        "chunks={} tx_chunks={} batch={} path_bytes={} reads={} range_len={} profile={} engines={} schema={} dbs={} group_commit={} shuffle_run={} data_fraction={} data_chunks={} with_crc={} entropy_fill={} allocator=system jemalloc=off",
        args.chunks,
        args.tx_chunks,
        args.batch,
        args.path_bytes,
        args.reads,
        args.range_len,
        args.workload.name(),
        args.engines.name(),
        args.schema.name(),
        args.dbs,
        u8::from(args.group_commit),
        args.shuffle_run.unwrap_or(0),
        args.data_fraction,
        args.data_chunks,
        u8::from(args.with_crc),
        u8::from(args.entropy_fill),
    );
    if args.workload.is_filter_reuse() {
        println!(
            "mode=filter-reuse schema=v1 rewrite_filters={}",
            u8::from(args.rewrite_filters)
        );
        return run_filter_reuse(&args);
    }
    if args.workload == Workload::CrcRows {
        println!(
            "mode=crc-rows sample_chunks={} partition_chunks={PARTITION_CHUNKS} schema=3.2",
            args.chunks
        );
        return run_crc_rows(&args);
    }
    if args.workload.is_worst() {
        let raw = u64::from(args.chunks) * DATA_PROOF_BYTES as u64;
        let full = PARTITION_CHUNKS * DATA_PROOF_BYTES as u64;
        println!(
            "mode=worst-case data_proof_bytes={DATA_PROOF_BYTES} data_proof_branches={DATA_PROOF_LAYERS} tx_proof_bytes={TX_PROOF_BYTES} tx_group_chunks={MAX_DATA_TX_CHUNKS} raw_data_proof_bytes={raw} full_partition_chunks={PARTITION_CHUNKS} full_partition_raw_data_proof_bytes={full}"
        );
    } else if args.workload.is_mid() {
        let raw = u64::from(args.chunks) * args.path_bytes as u64;
        let mdbx = commits_for(true, args.batch, true, args.group_commit);
        let rocks = commits_for(true, args.batch, false, args.group_commit);
        println!(
            "mode=mid raw_data_path_bytes={raw} mdbx_txs_per_commit={} mdbx_path_batch={} rocks_txs_per_commit={} rocks_path_batch={}",
            mdbx.txs_per_commit, mdbx.path_batch, rocks.txs_per_commit, rocks.path_batch
        );
    } else if args.workload.is_memory() {
        println!("mode={} {}", args.workload.name(), memory_predict(&args));
        return run_memory(&args);
    } else if args.workload == Workload::SmallTxs {
        println!(
            "mode=small-txs tx_chunks={} txs={} tx_proof_bytes={TX_PROOF_BYTES}",
            args.small_tx_chunks,
            tx_count(&args),
        );
    } else if args.workload == Workload::MidTxs {
        println!(
            "mode=mid-txs tx_chunks={MID_TX_MIN}..={MID_TX_MAX} txs={} tx_proof_bytes={TX_PROOF_BYTES}",
            tx_count(&args),
        );
    } else {
        println!("mode=filled");
    }

    if args.engines.includes_mdbx() {
        let mdbx_dir = args.dir.join("mdbx");
        fs::create_dir_all(&mdbx_dir)?;
        let map_bytes = engine_map_bytes(&args);
        let mut mdbx_commits =
            commits_for(args.workload.is_mid(), args.batch, true, args.group_commit);
        if let Some(n) = args.txs_per_commit {
            mdbx_commits.txs_per_commit = n;
        }
        println!(
            "engine=mdbx schema=v2 sync=durable geometry_max_bytes={map_bytes} txs_per_commit={} path_batch={}",
            mdbx_commits.txs_per_commit, mdbx_commits.path_batch
        );
        measure(&mdbx_dir, "mdbx", &args, mdbx_commits, |path| {
            SubmoduleIndex::open_mdbx(
                path,
                DatabaseArguments::irys_default(DbSyncMode::Durable)?
                    .with_geometry_max_size(Some(map_bytes)),
            )
        })?;
    }

    if args.engines.includes_rocks() {
        let rocks_dir = args.dir.join("rocks");
        fs::create_dir_all(&rocks_dir)?;
        let mut rocks_commits =
            commits_for(args.workload.is_mid(), args.batch, false, args.group_commit);
        if let Some(n) = args.txs_per_commit {
            rocks_commits.txs_per_commit = n;
        }
        let tuning = args.rocks;
        println!(
            "engine=rocks schema={} sync=wal_fsync {} max_open_files=-1 file_opening_threads=16 txs_per_commit={} path_batch={}",
            args.schema.name(),
            tuning.describe(),
            rocks_commits.txs_per_commit,
            rocks_commits.path_batch
        );
        measure(&rocks_dir, "rocks", &args, rocks_commits, |path| {
            open_bench_rocks(path, &args)
        })?;
    }
    Ok(())
}

/// One durable `update`. `--mid` groups MDBX only. `--group-commit` applies
/// that grouping to both engines. Without it, Rocks keeps one transaction
/// and the `--batch` data-path size.
struct CommitGrouping {
    txs_per_commit: u32,
    path_batch: u32,
}

/// 10-bit bloom, one byte per 8 bits. One family of offset keys.
fn bloom_bytes(keys: u64) -> u64 {
    keys.saturating_mul(10) / 8
}

fn memory_predict(args: &Args) -> String {
    let shared = args.rocks.share_resources;
    // A shared open uses the process cache, not the private preset field.
    // The bench does not install a custom size, so that cache is the default.
    // Inline data-path blocks use this cache. There is no separate blob
    // cache. These profiles store offset rows, so the figure is the bloom
    // against that one cache.
    let cache = if shared {
        irys_types::DEFAULT_ROCKSDB_BLOCK_CACHE_BYTES
    } else {
        args.rocks.block_cache_bytes as u64
    };
    let shared_resources = if shared { "on" } else { "off" };
    let keys = u64::from(args.chunks);
    let bloom = bloom_bytes(keys);
    match args.workload {
        Workload::MemDbs => {
            let serving = if shared {
                cache
            } else {
                cache.saturating_mul(u64::from(args.dbs))
            };
            format!(
                "rows_per_db={keys} block_cache_bytes={cache} serving_cache_bytes={serving} bloom_bytes_per_db={bloom} shared_resources={shared_resources}"
            )
        }
        Workload::IndexSpan => {
            format!(
                "one_update=1 rows={keys} bloom_bytes={bloom} block_cache_bytes={cache} shared_resources={shared_resources}"
            )
        }
        Workload::OffsetRows => {
            let fits = if bloom <= cache { "yes" } else { "no" };
            format!(
                "rows={keys} batch={} bloom_bytes={bloom} block_cache_bytes={cache} bloom_fits={fits} shared_resources={shared_resources}",
                args.batch
            )
        }
        Workload::Filled
        | Workload::Mid
        | Workload::Worst
        | Workload::FilterReuse
        | Workload::SmallTxs
        | Workload::MidTxs
        | Workload::CrcRows => String::new(),
    }
}

fn run_memory(args: &Args) -> eyre::Result<()> {
    match args.workload {
        Workload::MemDbs => run_mem_dbs(args),
        Workload::IndexSpan => run_index_span(args),
        Workload::OffsetRows => run_offset_rows(args),
        Workload::Filled
        | Workload::Mid
        | Workload::Worst
        | Workload::FilterReuse
        | Workload::SmallTxs
        | Workload::MidTxs
        | Workload::CrcRows => {
            eyre::bail!("not a memory profile")
        }
    }
}

/// Open several Rocks databases and leave them open.
///
/// Each one gets `chunks` offset rows, a flush, then a full scan. The scan
/// fills that database's block cache. RSS after each database is the number
/// to multiply toward a full host.
fn run_mem_dbs(args: &Args) -> eyre::Result<()> {
    let watch = MemWatch::start();
    note_mem("process", "start", &watch);
    let mut previous = current_mem();
    let mut stores = Vec::with_capacity(usize::try_from(args.dbs)?);
    for i in 0..args.dbs {
        let dir = args.dir.join(format!("rocks-{i}"));
        fs::create_dir_all(&dir)?;
        let store = open_bench_rocks(&dir, args)?;
        let write_ms = timed(|| write_offset_rows(&store, args.chunks, args.batch))?;
        let settle_ms = timed(|| store.settle_files())?;
        let scan_ms = timed(|| read_every_offset(&store, args.chunks))?;
        let now = current_mem();
        watch.observe(now);
        println!(
            "  db={i} write_ms={write_ms} settle_ms={settle_ms} scan_ms={scan_ms} rss_kb={} anon_kb={} delta_rss_kb={} delta_anon_kb={}",
            now.rss_kb,
            now.anon_kb,
            now.rss_kb as i64 - previous.rss_kb as i64,
            now.anon_kb as i64 - previous.anon_kb as i64,
        );
        previous = now;
        stores.push(store);
    }
    note_mem("process", "all_open", &watch);
    for i in (0..stores.len()).rev() {
        stores.pop();
        note_mem("process", &format!("after_drop_{i}"), &watch);
    }
    print_mem("process", "peak", watch.stop());
    Ok(())
}

/// One production-shaped index update: every offset in the span, one commit.
fn run_index_span(args: &Args) -> eyre::Result<()> {
    let watch = MemWatch::start();
    let dir = args.dir.join("rocks");
    fs::create_dir_all(&dir)?;
    note_mem("rocks", "before_open", &watch);
    let store = open_bench_rocks(&dir, args)?;
    note_mem("rocks", "after_open", &watch);
    let end = PartitionChunkOffset::from(args.chunks - 1);
    let update_ms = timed(|| {
        store.update(|tx| {
            tx.add_tx_path_hash_to_offset_range(
                PartitionChunkOffset::from(0),
                end,
                Some(hash_at(1)),
            )
        })
    })?;
    note_mem("rocks", "after_update", &watch);
    println!("  update_ms={update_ms}");
    let settle_ms = timed(|| store.settle_files())?;
    note_mem("rocks", "after_settle", &watch);
    println!("  settle_ms={settle_ms}");
    drop(store);
    note_mem("rocks", "after_drop", &watch);
    let (logical, allocated) = dir_usage(&dir)?;
    println!("  logical_bytes={logical} allocated_bytes={allocated}");
    print_mem("rocks", "peak", watch.stop());
    Ok(())
}

/// Offset rows and no data paths. The cold read is one family.
fn run_offset_rows(args: &Args) -> eyre::Result<()> {
    let watch = MemWatch::start();
    let dir = args.dir.join("rocks");
    fs::create_dir_all(&dir)?;
    note_mem("rocks", "before_open", &watch);
    let store = open_bench_rocks(&dir, args)?;
    note_mem("rocks", "after_open", &watch);
    let write_ms = timed(|| write_offset_rows(&store, args.chunks, args.batch))?;
    note_mem("rocks", "after_write", &watch);
    println!("  write_ms={write_ms}");
    let settle_ms = timed(|| store.settle_files())?;
    note_mem("rocks", "after_settle", &watch);
    println!("  settle_ms={settle_ms}");
    drop(store);
    let (logical, allocated) = dir_usage(&dir)?;
    println!("  logical_bytes={logical} allocated_bytes={allocated}");
    if args.reads > 0 {
        let disk = disk_id(&dir)?;
        let open = |path: &Path| open_bench_rocks(path, args);
        time_reopen(
            &dir,
            disk.as_ref(),
            "offset_random",
            args.reads,
            &open,
            |store, i| {
                let at = sample_offset(i, args.chunks);
                let row = store
                    .view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(at)))?;
                eyre::ensure!(row.is_some(), "missing offset {at}");
                Ok(())
            },
        )?;
    } else {
        println!("  reads=0");
    }
    note_mem("rocks", "after_reads", &watch);
    print_mem("rocks", "peak", watch.stop());
    Ok(())
}

fn write_offset_rows(store: &SubmoduleIndex, chunks: u32, batch: u32) -> eyre::Result<()> {
    let mut offset = 0_u32;
    while offset < chunks {
        let end = offset.saturating_add(batch).min(chunks);
        store.update(|tx| {
            for raw in offset..end {
                tx.set_path_hashes_by_offset(
                    PartitionChunkOffset::from(raw),
                    ChunkPathHashes {
                        data_path_hash: Some(hash_at(u64::from(raw).saturating_add(0x2000_0000))),
                        tx_path_hash: Some(hash_at(1)),
                    },
                )?;
            }
            Ok(())
        })?;
        offset = end;
    }
    Ok(())
}

fn print_legacy_ssts(store: &SubmoduleIndex, when: &str) -> eyre::Result<()> {
    for file in store.legacy_path_hash_ssts()? {
        println!(
            "  sst when={when} name={} level={} bytes={} entries={} start={} end={}",
            file.name,
            file.level,
            file.bytes,
            file.entries,
            fmt_offset(file.start),
            fmt_offset(file.end),
        );
    }
    Ok(())
}

/// Files whose key range overlaps the sampled offsets `0..chunks`.
fn print_sst_cover(store: &SubmoduleIndex, chunks: u32) -> eyre::Result<()> {
    let end = chunks.saturating_sub(1);
    let mut names = Vec::new();
    for file in store.legacy_path_hash_ssts()? {
        if sst_covers(&file, 0, end) {
            names.push(format!("{}@{}", file.name, file.level));
        }
    }
    let files = if names.is_empty() {
        "none".to_string()
    } else {
        names.join(",")
    };
    println!("  sst_cover shape=offset_get span_start=0 span_end={end} files={files}");
    Ok(())
}

fn sst_covers(file: &LegacySstSpan, start: u32, end: u32) -> bool {
    match (file.start, file.end) {
        (Some(file_start), Some(file_end)) => file_start <= end && start <= file_end,
        _ => true,
    }
}

fn fmt_offset(offset: Option<u32>) -> String {
    offset
        .map(|value| value.to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// Point Gets on a copied schema-v1 path-hash SST. Does not write proofs.
fn run_filter_reuse(args: &Args) -> eyre::Result<()> {
    let dir = args.dir.join("rocks");
    println!(
        "engine=rocks sync=wal_fsync {} max_open_files=-1 file_opening_threads=16",
        args.rocks.describe()
    );
    let started = Instant::now();
    let store = SubmoduleIndex::open_rocks_v1_probe(&dir, args.rocks)?;
    println!("  open_ms={}", started.elapsed().as_millis());
    print_legacy_ssts(&store, "after_open")?;
    if args.rewrite_filters {
        let rewrite_ms = timed(|| store.compact_legacy_path_hashes())?;
        println!("  rewrite_ms={rewrite_ms}");
        print_legacy_ssts(&store, "after_rewrite")?;
    }
    print_sst_cover(&store, args.chunks)?;
    let files = evict_cache(&dir)?;
    let disk = disk_id(&dir)?;
    time_shape(
        disk.as_ref(),
        "offset_get",
        args.reads,
        &store,
        files,
        true,
        &mut |store, i| {
            let at = sample_offset(i, args.chunks);
            let present = store.legacy_path_hash_present(PartitionChunkOffset::from(at))?;
            eyre::ensure!(present, "missing offset {at}");
            Ok(())
        },
    )?;
    Ok(())
}

fn read_every_offset(store: &SubmoduleIndex, chunks: u32) -> eyre::Result<()> {
    store.view(|tx| {
        for raw in 0..chunks {
            let row = tx
                .get_path_hashes_by_offset(PartitionChunkOffset::from(raw))?
                .ok_or_else(|| eyre::eyre!("missing offset {raw}"))?;
            eyre::ensure!(row.tx_path_hash.is_some(), "empty tx hash at {raw}");
        }
        Ok(())
    })
}

fn commits_for(mid: bool, batch: u32, mdbx: bool, group_commit: bool) -> CommitGrouping {
    if group_commit || (mid && mdbx) {
        CommitGrouping {
            txs_per_commit: MID_MDBX_TXS_PER_COMMIT,
            path_batch: batch.max(MID_MDBX_PATH_BATCH),
        }
    } else {
        CommitGrouping {
            txs_per_commit: 1,
            path_batch: batch,
        }
    }
}

fn measure(
    dir: &Path,
    engine: &str,
    args: &Args,
    commits: CommitGrouping,
    open: impl Fn(&Path) -> eyre::Result<SubmoduleIndex>,
) -> eyre::Result<()> {
    // Peak covers this engine only. `before_open` is whatever the previous
    // engine left resident. `file_kb` is mapped file pages. `anon_kb` is heap.
    let watch = MemWatch::start();
    note_mem(engine, "before_open", &watch);
    let store = open(dir)?;
    note_mem(engine, "after_open", &watch);
    let index_ms = timed(|| write_tx_index(&store, args, commits.txs_per_commit))?;
    note_mem(engine, "after_index", &watch);
    let data_path_ms = timed(|| write_data_paths(&store, args, commits.path_batch))?;
    note_mem(engine, "after_data_path", &watch);
    let entropy_ms = if args.entropy_fill {
        let elapsed = timed(|| write_entropy_rows(&store, args, commits.path_batch))?;
        note_mem(engine, "after_entropy", &watch);
        elapsed
    } else {
        0
    };
    let settle_ms = timed(|| store.settle_files())?;
    note_mem(engine, "after_settle", &watch);
    if engine == "rocks" {
        print_column_files(&store, args.chunks)?;
        if args.entropy_fill {
            print_entropy_fill_size(&entropy_fill_size(&store, args)?);
        }
    }
    drop(store);
    note_mem(engine, "after_write_drop", &watch);

    let (logical, allocated) = dir_usage(dir)?;
    let open_started = Instant::now();
    let store = open(dir)?;
    let open_ms = open_started.elapsed().as_millis();
    println!(
        "  index_ms={index_ms} data_path_ms={data_path_ms} entropy_ms={entropy_ms} settle_ms={settle_ms} open_ms={open_ms}"
    );
    println!("  logical_bytes={logical} allocated_bytes={allocated}");
    drop(store);
    read_phase(dir, engine, args, &open)?;
    note_mem(engine, "after_reads", &watch);
    print_mem(engine, "peak", watch.stop());
    Ok(())
}

/// Resident and virtual sizes from `/proc/self/status`, in KiB.
#[derive(Clone, Copy, Default)]
struct ProcStatus {
    rss_kb: u64,
    anon_kb: u64,
    file_kb: u64,
    size_kb: u64,
}

fn parse_proc_status(text: &str) -> ProcStatus {
    let mut status = ProcStatus::default();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(key) = fields.next() else {
            continue;
        };
        let Ok(value) = fields.next().unwrap_or("0").parse::<u64>() else {
            continue;
        };
        match key {
            "VmRSS:" => status.rss_kb = value,
            "RssAnon:" => status.anon_kb = value,
            "RssFile:" => status.file_kb = value,
            "VmSize:" => status.size_kb = value,
            _ => {}
        }
    }
    status
}

fn current_mem() -> ProcStatus {
    fs::read_to_string("/proc/self/status")
        .map(|text| parse_proc_status(&text))
        .unwrap_or_default()
}

fn print_mem(engine: &str, phase: &str, status: ProcStatus) {
    println!(
        "  mem engine={engine} phase={phase} rss_kb={} anon_kb={} file_kb={} size_kb={}",
        status.rss_kb, status.anon_kb, status.file_kb, status.size_kb
    );
}

fn note_mem(engine: &str, phase: &str, watch: &MemWatch) {
    let status = current_mem();
    watch.observe(status);
    print_mem(engine, phase, status);
}

/// Samples RSS while one engine is open. The process high-water mark is not
/// usable here: it never goes down, so the second engine would hide the first.
struct MemWatch {
    stop: Arc<AtomicBool>,
    peak: Arc<Mutex<ProcStatus>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MemWatch {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(Mutex::new(ProcStatus::default()));
        let stop_flag = Arc::clone(&stop);
        let peak_slot = Arc::clone(&peak);
        let thread = thread::spawn(move || {
            loop {
                let sample = current_mem();
                let mut best = peak_slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if sample.rss_kb >= best.rss_kb {
                    *best = sample;
                }
                drop(best);
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                thread::sleep(Duration::from_millis(200));
            }
        });
        Self {
            stop,
            peak,
            thread: Some(thread),
        }
    }

    fn observe(&self, sample: ProcStatus) {
        let mut best = self
            .peak
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if sample.rss_kb >= best.rss_kb {
            *best = sample;
        }
    }

    fn stop(mut self) -> ProcStatus {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let peak = self
            .peak
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *peak
    }
}

impl Drop for MemWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct IndexTx {
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
    tx_hash: H256,
    data_root: H256,
    tx_path: Vec<u8>,
    start_offset: RelativeChunkOffset,
    data_size: u64,
}

fn write_tx_index(store: &SubmoduleIndex, args: &Args, txs_per_commit: u32) -> eyre::Result<()> {
    let mut offset = 0_u32;
    while offset < args.data_chunks {
        let mut group = Vec::new();
        for _ in 0..txs_per_commit {
            if offset >= args.data_chunks {
                break;
            }
            let place = place_tx(args, offset);
            eyre::ensure!(
                place.start == offset,
                "transaction cursor {offset} is inside the span at {}",
                place.start
            );
            let count = place.stored;
            let start = PartitionChunkOffset::from(place.start);
            let end = PartitionChunkOffset::from(place.start + count - 1);
            group.push(IndexTx {
                start,
                end,
                tx_hash: hash_at(u64::from(place.start)),
                data_root: hash_at(u64::from(place.start) + 0x1000_0000),
                tx_path: tx_path_bytes(args, place.start),
                start_offset: RelativeChunkOffset(i32::try_from(place.start)?),
                data_size: place.leaves.saturating_mul(CHUNK_SIZE),
            });
            offset += count;
        }
        store.update(|tx| {
            for piece in group {
                tx.add_full_tx_path(piece.tx_hash, piece.tx_path)?;
                tx.add_tx_leaf_binding(
                    piece.tx_hash,
                    &TxLeafBinding {
                        data_root: piece.data_root,
                        prefix_hash: H256::zero(),
                    },
                )?;
                tx.add_tx_path_hash_to_offset_range(piece.start, piece.end, Some(piece.tx_hash))?;
                tx.add_data_root_info(
                    piece.data_root,
                    &DataRootInfo {
                        start_offset: piece.start_offset,
                        data_size: piece.data_size,
                    },
                )?;
                tx.add_pending_body_migration(
                    piece.start,
                    &PendingBodyMigration {
                        data_root: piece.data_root,
                        data_size: piece.data_size,
                        start_offset: piece.start_offset,
                        block_height: 1,
                        attempts: 0,
                    },
                )?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

fn write_data_paths(store: &SubmoduleIndex, args: &Args, path_batch: u32) -> eyre::Result<()> {
    if let Some(run) = args.shuffle_run {
        return write_shuffled_data_paths(store, args, path_batch, run);
    }
    let mut offset = 0_u32;
    while offset < args.data_chunks {
        let count = path_batch.min(args.data_chunks - offset);
        let mut updates = Vec::with_capacity(count as usize);
        for step in 0..count {
            let at = offset + step;
            updates.push((
                PartitionChunkOffset::from(at),
                hash_at(u64::from(at) + 0x2000_0000),
                data_path_bytes(args, at),
            ));
        }
        write_data_batch(store, args, updates)?;
        offset += count;
    }
    Ok(())
}

/// Each commit takes `path_batch` chunks from runs of `run` offsets visited
/// in a seeded Fisher-Yates order, so most commits touch keys spread over
/// the whole partition.
fn write_shuffled_data_paths(
    store: &SubmoduleIndex,
    args: &Args,
    path_batch: u32,
    run: u32,
) -> eyre::Result<()> {
    let runs = shuffled_runs(args.data_chunks, run);
    let mut updates = Vec::with_capacity(path_batch as usize);
    for start in runs {
        let end = start.saturating_add(run).min(args.data_chunks);
        for at in start..end {
            updates.push((
                PartitionChunkOffset::from(at),
                hash_at(u64::from(at) + 0x2000_0000),
                data_path_bytes(args, at),
            ));
            if updates.len() >= path_batch as usize {
                let batch = std::mem::take(&mut updates);
                write_data_batch(store, args, batch)?;
            }
        }
    }
    if !updates.is_empty() {
        write_data_batch(store, args, updates)?;
    }
    Ok(())
}

/// One data-path commit. `--with-crc` uses the production offset record.
fn write_data_batch(
    store: &SubmoduleIndex,
    args: &Args,
    updates: Vec<(PartitionChunkOffset, H256, Vec<u8>)>,
) -> eyre::Result<()> {
    if args.with_crc {
        let rows = updates
            .into_iter()
            .map(|(offset, _hash, proof)| PackedOffsetRow {
                offset,
                crc: Some(bench_chunk_crc(offset.0)),
                proof: Some(proof),
            })
            .collect();
        let missed = store.update(|tx| tx.write_packed_offset_rows(rows))?;
        eyre::ensure!(
            missed.is_empty(),
            "data row has no placement at {}",
            missed.first().map(|offset| offset.0).unwrap_or(0)
        );
        return Ok(());
    }
    store.update(|tx| tx.write_data_path_updates(updates))?;
    Ok(())
}

/// Checksum stored in a bench offset row.
///
/// The bench has no packed chunk, so this is not the CRC32C of chunk bytes.
/// The record is still [`encode_offset_meta`]: proof bytes, then the 4-byte
/// checksum, the flags byte, and the format byte. Offset 0 stores checksum 0.
fn bench_chunk_crc(offset: u32) -> u32 {
    offset.wrapping_mul(0x0100_0001)
}

/// Checksum-only rows for offsets the data region does not cover.
///
/// Rows go out in offset order. One `update` holds `batch` rows, the same
/// commit size as the data-path writes. `update` is the commit. The bench
/// does not add a sync between batches.
fn write_entropy_rows(store: &SubmoduleIndex, args: &Args, batch: u32) -> eyre::Result<()> {
    let mut offset = args.data_chunks;
    while offset < args.chunks {
        let count = batch.min(args.chunks - offset);
        let mut rows = Vec::with_capacity(count as usize);
        for step in 0..count {
            let at = offset + step;
            rows.push(PackedOffsetRow {
                offset: PartitionChunkOffset::from(at),
                crc: Some(bench_chunk_crc(at)),
                proof: None,
            });
        }
        let missed = store.update(|tx| tx.write_packed_offset_rows(rows))?;
        eyre::ensure!(missed.is_empty(), "entropy row was not stored");
        offset += count;
    }
    Ok(())
}

/// Start offsets of `run`-sized runs covering `0..chunks`, shuffled.
fn shuffled_runs(chunks: u32, run: u32) -> Vec<u32> {
    let step = usize::try_from(run.max(1)).unwrap_or(usize::MAX);
    let mut runs: Vec<u32> = (0..chunks).step_by(step).collect();
    let mut state = SHUFFLE_SEED;
    for i in (1..runs.len()).rev() {
        state = state.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        let j = usize::try_from((state >> 1) % (i as u64 + 1)).unwrap_or(0);
        runs.swap(i, j);
    }
    runs
}

/// Cold lookups. Each shape reopens the engine and drops the OS file cache.
/// A shared Rocks block cache would turn the sequential scan into hits on the
/// random sample. `drop_caches` is not used: another bench may share the host.
///
/// `data_path_random` then runs again on the same open engine. That pass
/// still drops the page cache. The Rocks block cache stays warm, and table
/// files stay open.
fn read_phase(
    dir: &Path,
    engine: &str,
    args: &Args,
    open: &impl Fn(&Path) -> eyre::Result<SubmoduleIndex>,
) -> eyre::Result<()> {
    if args.reads == 0 {
        println!("  reads=0");
        return Ok(());
    }
    let disk = disk_id(dir)?;
    let disk_name = disk.as_ref().map(|id| id.name.as_str()).unwrap_or("none");
    println!("  cold=fadvise disk={disk_name}");
    // Scattered chunk reads (recall, single-chunk serve). Data offsets only.
    let data = args.data_chunks;
    let mut random =
        |store: &SubmoduleIndex, i: u32| read_one_data_path(store, args, sample_offset(i, data));
    let (store, files) = open_for_shape(dir, open)?;
    time_shape(
        disk.as_ref(),
        "data_path_random",
        args.reads,
        &store,
        files,
        false,
        &mut random,
    )?;
    // Same engine. Pages are dropped again. Tables stay open.
    let files = evict_cache(dir)?;
    if engine == "rocks" {
        println!("  note shape=data_path_random_open kept_open=1 page_cache=cold block_cache=warm");
    } else {
        println!("  note shape=data_path_random_open kept_open=1 page_cache=cold");
    }
    time_shape(
        disk.as_ref(),
        "data_path_random_open",
        args.reads,
        &store,
        files,
        true,
        &mut random,
    )?;
    drop(store);
    // Consecutive chunk reads (a span serve). Same call, offsets 0, 1, 2, ...
    time_reopen(
        dir,
        disk.as_ref(),
        "data_path_seq",
        args.reads,
        open,
        |store, i| read_one_data_path(store, args, seq_offset(i, data)),
    )?;
    time_reopen(dir, disk.as_ref(), "serve", args.reads, open, |store, i| {
        let at = sample_offset(i.wrapping_add(1), data);
        let expected = hash_at(u64::from(at) + 0x2000_0000);
        store.view(|tx| {
            let hashes = tx
                .get_path_hashes_by_offset(PartitionChunkOffset::from(at))?
                .ok_or_else(|| eyre::eyre!("missing offset row {at}"))?;
            let hash = hashes
                .data_path_hash
                .ok_or_else(|| eyre::eyre!("missing data path hash at {at}"))?;
            let path = tx
                .get_data_path_by_offset(PartitionChunkOffset::from(at))?
                .ok_or_else(|| eyre::eyre!("missing data path body at {at}"))?;
            let want_len = expected_path_len(args, at);
            eyre::ensure!(
                path.len() == want_len,
                "serve path len at {at}: {} != {want_len}",
                path.len()
            );
            // Schema v2 stores the hash the writer passed. Schema v3 has no
            // path-hash row, so the reported hash is sha256 of the bytes.
            let expected = if store.stores_tx_paths() {
                expected
            } else {
                H256(irys_types::hash_sha256(&path))
            };
            eyre::ensure!(hash == expected, "data path hash mismatch at {at}");
            Ok(())
        })
    })?;
    let window = args.range_len.min(data);
    time_reopen(dir, disk.as_ref(), "range", args.reads, open, |store, i| {
        let start = sample_offset(i.wrapping_add(2), data - window + 1);
        let end = start + window - 1;
        let rows = store.view(|tx| {
            tx.path_hashes_in_inclusive_range(
                PartitionChunkOffset::from(start),
                PartitionChunkOffset::from(end),
            )
        })?;
        eyre::ensure!(
            rows.len() == usize::try_from(window)?,
            "range {}..={} returned {}",
            start,
            end,
            rows.len()
        );
        Ok(())
    })?;
    // What `paths_in_tx` / `metas_in_tx` do for a span: the range read,
    // then every proof in it, in one read transaction. Same offsets as range.
    time_reopen(dir, disk.as_ref(), "span", args.reads, open, |store, i| {
        let start = sample_offset(i.wrapping_add(2), data - window + 1);
        let end = start + window - 1;
        let proofs = store.view(|tx| {
            let rows = tx.path_hashes_in_inclusive_range(
                PartitionChunkOffset::from(start),
                PartitionChunkOffset::from(end),
            )?;
            let mut proofs = 0_usize;
            for (offset, hashes) in rows {
                if hashes.data_path_hash.is_some() && tx.get_data_path_by_offset(offset)?.is_some()
                {
                    proofs += 1;
                }
            }
            Ok(proofs)
        })?;
        eyre::ensure!(
            proofs == usize::try_from(window)?,
            "span {start}..={end} read {proofs} proofs"
        );
        Ok(())
    })?;
    // Cold range reads of offset rows. Skipped when every offset is data.
    if let Some(reason) = entropy_read_skip(engine, args) {
        println!("  read shape=entropy_range skipped: {reason}");
        println!("  read shape=recall_rows skipped: {reason}");
    } else {
        time_reopen(
            dir,
            disk.as_ref(),
            "entropy_range",
            args.reads,
            open,
            |store, i| {
                read_entropy_range(store, args, i)?;
                Ok(())
            },
        )?;
        time_reopen(
            dir,
            disk.as_ref(),
            "recall_rows",
            args.reads,
            open,
            |store, i| {
                read_recall_rows(store, args, i)?;
                Ok(())
            },
        )?;
    }
    // Before rmw_path rewrites proofs. The leaf hash comes from the bytes
    // this run stored, not from a data-path read that would warm that family.
    if engine == "rocks" && args.schema.is_v3() {
        time_reopen(
            dir,
            disk.as_ref(),
            "content_lookup",
            args.reads,
            open,
            |store, i| {
                let at = sample_offset(i, data);
                let proof = data_path_bytes(args, at);
                let Some(hash) = content_hash_of_proof(&proof) else {
                    eyre::bail!("content_lookup needs a proof of at least 64 bytes at offset {at}");
                };
                let offsets = store.view(|tx| tx.offsets_for_content_hash(hash))?;
                eyre::ensure!(
                    offsets.iter().any(|offset| offset.0 == at),
                    "content lookup missed offset {at}"
                );
                Ok(())
            },
        )?;
        time_reopen(
            dir,
            disk.as_ref(),
            "content_miss",
            args.reads,
            open,
            |store, i| {
                let mut tagged = b"content-miss".to_vec();
                tagged.extend(i.to_be_bytes());
                let hash = H256(irys_types::hash_sha256(&tagged));
                let offsets = store.view(|tx| tx.offsets_for_content_hash(hash))?;
                eyre::ensure!(offsets.is_empty(), "absent content hash returned a row");
                Ok(())
            },
        )?;
    }
    time_reopen(
        dir,
        disk.as_ref(),
        "rmw_path",
        args.reads,
        open,
        |store, i| {
            let at = sample_offset(i.wrapping_add(3), data);
            // One changed byte. The same bytes let MDBX skip the flush while
            // RocksDB still syncs the WAL.
            let path = rewritten_data_path(args, at);
            store.update(|tx| {
                tx.write_data_path_updates(vec![(
                    PartitionChunkOffset::from(at),
                    hash_at(u64::from(at) + 0x2000_0000),
                    path,
                )])?;
                Ok(())
            })
        },
    )?;
    if args.schema.is_v3() {
        // Same reason as `rmw_root_sample`. Do not time empty samples.
        println!("  read shape=rmw_root skipped: {V3_RMW_ROOT_SKIP}");
    } else {
        time_reopen(
            dir,
            disk.as_ref(),
            "rmw_root",
            args.reads,
            open,
            |store, i| match rmw_root_sample(store, args, i)? {
                RmwRoot::Wrote => Ok(()),
                RmwRoot::Skipped(reason) => {
                    eyre::bail!("rmw_root skipped during a timed shape: {reason}")
                }
            },
        )?;
    }
    let store = open(dir)?;
    let kept = store.view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(0)))?;
    let kept = kept.ok_or_else(|| eyre::eyre!("offset 0 missing after rmw"))?;
    // Schema v2 keeps the stored tx-path hash. Schema v3 reports the
    // placement data root. Offset 0 is the start of the first transaction.
    let expected_tx = if store.stores_tx_paths() {
        hash_at(0)
    } else {
        hash_at(0x1000_0000)
    };
    eyre::ensure!(
        kept.tx_path_hash == Some(expected_tx),
        "rmw_path dropped the tx path hash"
    );
    Ok(())
}

/// Stored proof length at `at`. Tree shapes vary the depth per transaction
/// (and per leaf in an odd tree), so ask the generator.
fn expected_path_len(args: &Args, at: u32) -> usize {
    if args.workload.is_tree() {
        data_path_bytes(args, at).len()
    } else {
        args.path_bytes
    }
}

enum RmwRoot {
    Wrote,
    Skipped(&'static str),
}

/// Why `rmw_root` does not write on schema v3.
///
/// Production fills the data-root row once, in the same commit as the
/// interval (`add_data_root_info` while the placement is still unfilled).
/// After that commit the placement is filled. A later append has no unfilled
/// placement, and `set_data_root_infos` will not add or replace the row while
/// the interval exists. There is no production read-modify-write of this row.
const V3_RMW_ROOT_SKIP: &str = "schema v3 fills each placement's data-root row once, in the same commit as its interval. A later info append has no unfilled placement, and that row cannot be replaced while the interval exists. Production has no read-modify-write of this row.";

/// Append one data-root info that is not already stored, so the commit is real.
///
/// Schema v2 keeps a list per data root. This sample pushes a new entry.
/// Schema v3 returns [`RmwRoot::Skipped`] and does not write. See
/// [`V3_RMW_ROOT_SKIP`].
fn rmw_root_sample(store: &SubmoduleIndex, args: &Args, i: u32) -> eyre::Result<RmwRoot> {
    if !store.stores_tx_paths() {
        return Ok(RmwRoot::Skipped(V3_RMW_ROOT_SKIP));
    }
    let at = sample_offset(i.wrapping_add(4), args.data_chunks);
    let tx_start = at / args.tx_chunks * args.tx_chunks;
    let root = hash_at(u64::from(tx_start) + 0x1000_0000);
    // Distinct from the indexed row and from every other sample, so
    // the read-before-write always appends and both engines commit.
    let info = DataRootInfo {
        start_offset: RelativeChunkOffset(-1),
        data_size: indexed_data_size(args, 1).saturating_add(u64::from(i) + 1),
    };
    store.update(|tx| tx.add_data_root_info(root, &info))?;
    Ok(RmwRoot::Wrote)
}

fn read_one_data_path(store: &SubmoduleIndex, args: &Args, at: u32) -> eyre::Result<()> {
    let path = store.view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(at)))?;
    let path = path.ok_or_else(|| eyre::eyre!("missing data path at {at}"))?;
    let expected = expected_path_len(args, at);
    eyre::ensure!(
        path.len() == expected,
        "data path len at {at}: {} != {expected}",
        path.len()
    );
    Ok(())
}

/// Why `entropy_range` and `recall_rows` do not run.
fn entropy_read_skip(engine: &str, args: &Args) -> Option<&'static str> {
    if args.data_chunks == args.chunks {
        Some("no entropy region")
    } else if engine != "rocks" || !args.schema.is_v3() {
        Some("schema v3 rocks is required")
    } else {
        None
    }
}

/// Cold `--range-len` read of consecutive checksum-only rows.
///
/// The window sits in the entropy region. The call is `offset_metas_in_range`,
/// the same range read production uses when it checks recall checksums.
/// Returns the row count.
fn read_entropy_range(store: &SubmoduleIndex, args: &Args, i: u32) -> eyre::Result<usize> {
    let entropy = args.chunks - args.data_chunks;
    eyre::ensure!(entropy > 0, "no entropy region");
    let window = args.range_len.min(entropy);
    let start = args.data_chunks + sample_offset(i, entropy - window + 1);
    let end = start + window - 1;
    let rows = store.view(|tx| {
        tx.offset_metas_in_range(
            PartitionChunkOffset::from(start),
            PartitionChunkOffset::from(end),
        )
    })?;
    if args.entropy_fill {
        eyre::ensure!(
            rows.len() == usize::try_from(window)?,
            "entropy_range {start}..={end} returned {} rows",
            rows.len()
        );
        for (offset, meta) in &rows {
            eyre::ensure!(
                meta.proof.is_none() && meta.crc.is_some(),
                "entropy row {} is not checksum-only",
                offset.0
            );
        }
    } else {
        eyre::ensure!(
            rows.is_empty(),
            "entropy_range {start}..={end} returned {} rows without --entropy-fill",
            rows.len()
        );
    }
    Ok(rows.len())
}

/// Cold `--range-len` read at a random offset over the whole partition.
///
/// Data and entropy rows are both visible. The iterator is
/// `offset_metas_in_range`. Returns the row count.
fn read_recall_rows(store: &SubmoduleIndex, args: &Args, i: u32) -> eyre::Result<usize> {
    let window = args.range_len.min(args.chunks);
    let start = sample_offset(i, args.chunks - window + 1);
    let end = start + window - 1;
    let rows = store.view(|tx| {
        tx.offset_metas_in_range(
            PartitionChunkOffset::from(start),
            PartitionChunkOffset::from(end),
        )
    })?;
    let mut expected = 0_usize;
    for offset in start..=end {
        if offset < args.data_chunks || args.entropy_fill {
            expected += 1;
        }
    }
    eyre::ensure!(
        rows.len() == expected,
        "recall_rows {start}..={end} returned {} rows, want {expected}",
        rows.len()
    );
    Ok(rows.len())
}

/// Sequential offsets, wrapping after the last chunk.
fn seq_offset(i: u32, chunks: u32) -> u32 {
    i % chunks
}

fn open_for_shape(
    dir: &Path,
    open: &impl Fn(&Path) -> eyre::Result<SubmoduleIndex>,
) -> eyre::Result<(SubmoduleIndex, u32)> {
    // Drop pages left by the previous shape, then open a new cache.
    evict_cache(dir)?;
    let store = open(dir)?;
    // Open reads metadata. Drop those pages. Rocks keeps filters it copied.
    let files = evict_cache(dir)?;
    Ok((store, files))
}

fn time_reopen(
    dir: &Path,
    disk: Option<&DiskId>,
    shape: &str,
    reads: u32,
    open: &impl Fn(&Path) -> eyre::Result<SubmoduleIndex>,
    mut one: impl FnMut(&SubmoduleIndex, u32) -> eyre::Result<()>,
) -> eyre::Result<()> {
    let (store, files) = open_for_shape(dir, open)?;
    time_shape(disk, shape, reads, &store, files, false, &mut one)?;
    drop(store);
    Ok(())
}

fn time_shape(
    disk: Option<&DiskId>,
    shape: &str,
    reads: u32,
    store: &SubmoduleIndex,
    files: u32,
    kept_open: bool,
    one: &mut impl FnMut(&SubmoduleIndex, u32) -> eyre::Result<()>,
) -> eyre::Result<()> {
    let opens_before = print_rocks_bg(store, shape, false, None)?;
    let before = disk_counters(disk);
    let mut samples = Vec::with_capacity(usize::try_from(reads)?);
    for i in 0..reads {
        let started = Instant::now();
        one(store, i)?;
        samples.push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
    }
    let after = disk_counters(disk);
    print_rocks_bg(store, shape, true, opens_before)?;
    let (head, tail) = head_tail(&samples);
    let all = ranks(&samples);
    let kept = u8::from(kept_open);
    let line = format!(
        "  read shape={shape} n={} p50_us={} p99_us={} max_us={} head_n={} head_p50_us={} head_p99_us={} head_max_us={} tail_n={} tail_p50_us={} tail_p99_us={} tail_max_us={} evict_files={files} kept_open={kept}",
        all.n,
        all.p50,
        all.p99,
        all.max,
        head.n,
        head.p50,
        head.p99,
        head.max,
        tail.n,
        tail.p50,
        tail.p99,
        tail.max,
    );
    match (before, after) {
        (Some(before), Some(after)) => {
            let disk_reads = after.reads.saturating_sub(before.reads);
            let disk_read_bytes = after
                .read_sectors
                .saturating_sub(before.read_sectors)
                .saturating_mul(512);
            let disk_writes = after.writes.saturating_sub(before.writes);
            let disk_write_bytes = after
                .write_sectors
                .saturating_sub(before.write_sectors)
                .saturating_mul(512);
            let n = all.n.max(1);
            println!(
                "{line} disk_reads={disk_reads} disk_read_bytes={disk_read_bytes} disk_writes={disk_writes} disk_write_bytes={disk_write_bytes} per_lookup_read_bytes={}",
                disk_read_bytes / n
            );
        }
        _ => println!("{line} disk=none"),
    }
    Ok(())
}

fn print_rocks_bg(
    store: &SubmoduleIndex,
    shape: &str,
    after: bool,
    opens_before: Option<u64>,
) -> eyre::Result<Option<u64>> {
    let Some(bg) = store.rocks_background()? else {
        return Ok(None);
    };
    let when = if after { "after" } else { "before" };
    match (after, opens_before, bg.no_file_opens) {
        (true, Some(before), Some(now)) => println!(
            "  rocks_bg shape={shape} when={when} compactions={} flushes={} compaction_pending={} flush_pending={} no_file_opens={now} no_file_opens_delta={}",
            bg.compactions,
            bg.flushes,
            bg.compaction_pending,
            bg.flush_pending,
            now.saturating_sub(before),
        ),
        (_, _, Some(now)) => println!(
            "  rocks_bg shape={shape} when={when} compactions={} flushes={} compaction_pending={} flush_pending={} no_file_opens={now}",
            bg.compactions, bg.flushes, bg.compaction_pending, bg.flush_pending,
        ),
        _ => println!(
            "  rocks_bg shape={shape} when={when} compactions={} flushes={} compaction_pending={} flush_pending={}",
            bg.compactions, bg.flushes, bg.compaction_pending, bg.flush_pending,
        ),
    }
    Ok(bg.no_file_opens)
}

struct Rank {
    n: u64,
    p50: u64,
    p99: u64,
    max: u64,
}

/// Percentiles of `samples` in the order given. Sorting is a copy.
fn ranks(samples: &[u64]) -> Rank {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    Rank {
        n: sorted.len() as u64,
        p50: percentile(&sorted, 50),
        p99: percentile(&sorted, 99),
        max: sorted.last().copied().unwrap_or(0),
    }
}

/// First [`HEAD_SAMPLES`] stay in arrival order. The rest are the tail.
fn head_tail(samples: &[u64]) -> (Rank, Rank) {
    let split = HEAD_SAMPLES.min(samples.len());
    (ranks(&samples[..split]), ranks(&samples[split..]))
}

/// `i` maps onto `0..span`. The sequence is fixed so two runs hit the same keys.
fn sample_offset(i: u32, span: u32) -> u32 {
    debug_assert!(span > 0);
    let mixed = u64::from(i)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(0x6A09_E667);
    u32::try_from(mixed % u64::from(span)).unwrap_or(0)
}

/// Nearest-rank percentile. `sorted` is ascending. `pct` is 0..=100.
fn percentile(sorted: &[u64], pct: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = pct.saturating_mul(sorted.len() as u64).div_ceil(100);
    let idx = usize::try_from(rank.saturating_sub(1)).unwrap_or(0);
    sorted[idx.min(sorted.len() - 1)]
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn posix_fadvise(fd: i32, offset: i64, len: i64, advice: i32) -> i32;
}

#[cfg(target_os = "linux")]
const POSIX_FADV_DONTNEED: i32 = 4;

/// Drop clean page-cache pages for every regular file under `dir`.
fn evict_cache(dir: &Path) -> eyre::Result<u32> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = dir;
        eyre::bail!("cold reads need linux posix_fadvise");
    }
    #[cfg(target_os = "linux")]
    {
        let mut files = 0_u32;
        let mut stack = vec![dir.to_path_buf()];
        while let Some(path) = stack.pop() {
            // Compaction can unlink an SST between readdir and this stat.
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err.into()),
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                let entries = match fs::read_dir(&path) {
                    Ok(entries) => entries,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(err.into()),
                };
                for entry in entries {
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(err) => return Err(err.into()),
                    };
                    stack.push(entry.path());
                }
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let file = match File::open(&path) {
                Ok(file) => file,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err.into()),
            };
            let rc = unsafe { posix_fadvise(file.as_raw_fd(), 0, 0, POSIX_FADV_DONTNEED) };
            if rc != 0 {
                eyre::bail!(
                    "fadvise {} failed: {}",
                    path.display(),
                    Error::from_raw_os_error(rc)
                );
            }
            files += 1;
        }
        Ok(files)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiskId {
    major: u32,
    minor: u32,
    name: String,
}

#[derive(Clone, Copy)]
struct DiskSnap {
    reads: u64,
    read_sectors: u64,
    writes: u64,
    write_sectors: u64,
}

fn disk_id(dir: &Path) -> eyre::Result<Option<DiskId>> {
    let canon = dir.canonicalize()?;
    let text = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    Ok(mount_disk(&text, &canon))
}

fn disk_counters(disk: Option<&DiskId>) -> Option<DiskSnap> {
    let disk = disk?;
    let text = fs::read_to_string("/proc/diskstats").ok()?;
    parse_diskstats(&text, disk.major, disk.minor)
}

fn mount_disk(mountinfo: &str, path: &Path) -> Option<DiskId> {
    let mut best: Option<(usize, u32, u32)> = None;
    for line in mountinfo.lines() {
        let Some((mount, major, minor)) = parse_mount_line(line) else {
            continue;
        };
        let mount_path = Path::new(&mount);
        if path != mount_path && !path.starts_with(mount_path) {
            continue;
        }
        let len = mount.len();
        if best
            .as_ref()
            .is_some_and(|(best_len, _, _)| *best_len >= len)
        {
            continue;
        }
        best = Some((len, major, minor));
    }
    let (_, major, minor) = best?;
    let stats = fs::read_to_string("/proc/diskstats").ok()?;
    let name = disk_name(&stats, major, minor)?;
    Some(DiskId { major, minor, name })
}

fn parse_mount_line(line: &str) -> Option<(String, u32, u32)> {
    let mut fields = line.split_whitespace();
    let _id = fields.next()?;
    let _parent = fields.next()?;
    let dev = fields.next()?;
    let _root = fields.next()?;
    let mount = unescape_mount(fields.next()?);
    let (major, minor) = dev.split_once(':')?;
    Some((mount, major.parse().ok()?, minor.parse().ok()?))
}

fn unescape_mount(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let oct = &text[i + 1..i + 4];
            if let Ok(value) = u8::from_str_radix(oct, 8) {
                out.push(char::from(value));
                i += 4;
                continue;
            }
        }
        out.push(char::from(bytes[i]));
        i += 1;
    }
    out
}

fn parse_diskstats(text: &str, major: u32, minor: u32) -> Option<DiskSnap> {
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let line_major: u32 = fields.next()?.parse().ok()?;
        let line_minor: u32 = fields.next()?.parse().ok()?;
        if line_major != major || line_minor != minor {
            continue;
        }
        let _name = fields.next()?;
        let reads = fields.next()?.parse().ok()?;
        let _merged = fields.next()?;
        let read_sectors = fields.next()?.parse().ok()?;
        let _read_ms = fields.next()?;
        let writes = fields.next()?.parse().ok()?;
        let _write_merged = fields.next()?;
        let write_sectors = fields.next()?.parse().ok()?;
        return Some(DiskSnap {
            reads,
            read_sectors,
            writes,
            write_sectors,
        });
    }
    None
}

fn disk_name(text: &str, major: u32, minor: u32) -> Option<String> {
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let line_major: u32 = fields.next()?.parse().ok()?;
        let line_minor: u32 = fields.next()?.parse().ok()?;
        let name = fields.next()?;
        if line_major == major && line_minor == minor {
            return Some(name.to_string());
        }
    }
    None
}

#[derive(Clone, Copy)]
enum CrcShape {
    /// One 4096-chunk transaction. Neighbouring leaves share the upper proof.
    MidShared,
    /// One max-size transaction. The sample is a contiguous leaf prefix.
    WorstShared,
    /// A distinct max-depth proof on every offset.
    WorstUniform,
}

#[derive(Clone, Copy)]
enum CrcLayout {
    Raw,
    SuffixNoCrc,
    SuffixCrc,
    PrefixCrc,
}

impl CrcLayout {
    fn name(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::SuffixNoCrc => "suffix_no_crc",
            Self::SuffixCrc => "suffix_crc",
            Self::PrefixCrc => "prefix_crc",
        }
    }
}

fn run_crc_rows(args: &Args) -> eyre::Result<()> {
    let shared = args.chunks;
    let uniform = shared.min(131_072);
    let shapes = [
        ("mid_shared", shared, CrcShape::MidShared),
        ("worst_shared", shared, CrcShape::WorstShared),
        ("worst_uniform", uniform, CrcShape::WorstUniform),
    ];
    let layouts = [
        CrcLayout::Raw,
        CrcLayout::SuffixNoCrc,
        CrcLayout::SuffixCrc,
        CrcLayout::PrefixCrc,
    ];
    for (shape, chunks, kind) in shapes {
        for layout in layouts {
            measure_crc_layout(args, shape, chunks, kind, layout)?;
        }
    }
    measure_crc_only(args, shared)?;
    println!(
        "crc_rows note=sample partition_chunks={PARTITION_CHUNKS} extrapolated_sst is sample_sst / sample_chunks * partition_chunks"
    );
    Ok(())
}

fn measure_crc_layout(
    args: &Args,
    shape: &str,
    chunks: u32,
    kind: CrcShape,
    layout: CrcLayout,
) -> eyre::Result<()> {
    let path = args.dir.join(format!("crc-{shape}-{}", layout.name()));
    fs::create_dir_all(&path)?;
    let store = open_bench_rocks(&path, args)?;
    let mut total_value = 0_u64;
    let mut batch = Vec::with_capacity(4_096);
    for index in 0..chunks {
        let proof = crc_shape_proof(kind, index);
        let bytes = crc_layout_bytes(layout, &proof, index)?;
        total_value = total_value.saturating_add(bytes.len() as u64);
        batch.push((PartitionChunkOffset::from(index), bytes));
        if batch.len() == 4_096 {
            store.write_raw_data_paths(std::mem::take(&mut batch))?;
        }
    }
    if !batch.is_empty() {
        store.write_raw_data_paths(batch)?;
    }
    store.compact_data_paths()?;
    let sst = data_path_sst_bytes(&store)?;
    drop(store);
    print_crc_size(shape, layout.name(), chunks, total_value, sst);
    fs::remove_dir_all(&path)?;
    Ok(())
}

fn measure_crc_only(args: &Args, chunks: u32) -> eyre::Result<()> {
    let path = args.dir.join("crc-only");
    fs::create_dir_all(&path)?;
    let store = open_bench_rocks(&path, args)?;
    let mut total_value = 0_u64;
    let mut batch = Vec::with_capacity(4_096);
    for index in 0..chunks {
        let bytes = encode_offset_meta(&OffsetMeta {
            crc: Some(index.wrapping_mul(0x0100_0001)),
            proof: None,
        })?;
        total_value = total_value.saturating_add(bytes.len() as u64);
        batch.push((PartitionChunkOffset::from(index), bytes));
        if batch.len() == 4_096 {
            store.write_raw_data_paths(std::mem::take(&mut batch))?;
        }
    }
    if !batch.is_empty() {
        store.write_raw_data_paths(batch)?;
    }
    store.compact_data_paths()?;
    let sst = data_path_sst_bytes(&store)?;
    drop(store);
    print_crc_size("crc_only", "suffix", chunks, total_value, sst);
    fs::remove_dir_all(&path)?;
    Ok(())
}

struct EntropyFillSize {
    data_rows: u64,
    entropy_rows: u64,
    sst_bytes: u64,
}

/// `ChunkMetaByOffset` after settle. `sst_bytes` is the whole family.
fn entropy_fill_size(store: &SubmoduleIndex, args: &Args) -> eyre::Result<EntropyFillSize> {
    Ok(EntropyFillSize {
        data_rows: u64::from(args.data_chunks),
        entropy_rows: u64::from(args.chunks - args.data_chunks),
        sst_bytes: data_path_sst_bytes(store)?,
    })
}

fn print_entropy_fill_size(size: &EntropyFillSize) {
    let per_data = if size.data_rows == 0 {
        0.0
    } else {
        size.sst_bytes as f64 / size.data_rows as f64
    };
    let per_entropy = if size.entropy_rows == 0 {
        0.0
    } else {
        size.sst_bytes as f64 / size.entropy_rows as f64
    };
    println!(
        "  entropy_fill data_rows={} entropy_rows={} cf={CHUNK_META_BY_OFFSET_CF} sst_bytes={} sst_bytes_per_data_chunk={per_data:.2} sst_bytes_per_entropy_row={per_entropy:.2}",
        size.data_rows, size.entropy_rows, size.sst_bytes
    );
}

fn print_crc_size(shape: &str, layout: &str, chunks: u32, total_value: u64, sst: u64) {
    let chunks_f = f64::from(chunks);
    let value_per = total_value as f64 / chunks_f;
    let sst_per = sst as f64 / chunks_f;
    let extrapolated = sst_per * PARTITION_CHUNKS as f64;
    println!(
        "crc_rows shape={shape} layout={layout} chunks={chunks} value_bytes={total_value} value_bytes_per_chunk={value_per:.2} sst_bytes={sst} sst_bytes_per_chunk={sst_per:.2} extrapolated_partition_sst={extrapolated:.0}"
    );
}

fn data_path_sst_bytes(store: &SubmoduleIndex) -> eyre::Result<u64> {
    let files = store.column_file_usage()?;
    Ok(files
        .into_iter()
        .find(|cf| cf.name == CHUNK_META_BY_OFFSET_CF)
        .map(|cf| cf.sst_bytes)
        .unwrap_or(0))
}

fn crc_shape_proof(kind: CrcShape, index: u32) -> Vec<u8> {
    match kind {
        CrcShape::MidShared => {
            let leaf = u64::from(index % 4_096);
            shared_data_proof(&[0x11_u8; 32], 4_096, leaf, CHUNK_SIZE)
        }
        CrcShape::WorstShared => shared_data_proof(
            &[0x22_u8; 32],
            MAX_DATA_TX_CHUNKS,
            u64::from(index),
            CHUNK_SIZE,
        ),
        CrcShape::WorstUniform => {
            let end = u64::from(index)
                .saturating_add(1)
                .saturating_mul(CHUNK_SIZE);
            shaped_proof(DATA_PROOF_LAYERS, end, u64::from(index))
        }
    }
}

fn crc_layout_bytes(layout: CrcLayout, proof: &[u8], index: u32) -> eyre::Result<Vec<u8>> {
    let crc = index.wrapping_mul(0x0100_0001);
    match layout {
        CrcLayout::Raw => Ok(proof.to_vec()),
        CrcLayout::SuffixNoCrc => encode_offset_meta(&OffsetMeta {
            crc: None,
            proof: Some(proof.to_vec()),
        }),
        CrcLayout::SuffixCrc => encode_offset_meta(&OffsetMeta {
            crc: Some(crc),
            proof: Some(proof.to_vec()),
        }),
        CrcLayout::PrefixCrc => Ok(encode_prefix_record(proof, Some(crc))),
    }
}

/// Bench-only prefix. Production records put the trailer after the proof.
fn encode_prefix_record(proof: &[u8], crc: Option<u32>) -> Vec<u8> {
    let mut flags = 0_u8;
    if !proof.is_empty() {
        flags |= 0x01;
    }
    if crc.is_some() {
        flags |= 0x02;
    }
    let mut out = Vec::with_capacity(proof.len() + 6);
    out.push(1);
    out.push(flags);
    if let Some(crc) = crc {
        out.extend_from_slice(&crc.to_le_bytes());
    }
    out.extend_from_slice(proof);
    out
}

fn timed(f: impl FnOnce() -> eyre::Result<()>) -> eyre::Result<u128> {
    let started = Instant::now();
    f()?;
    Ok(started.elapsed().as_millis())
}

fn print_column_files(store: &SubmoduleIndex, chunks: u32) -> eyre::Result<()> {
    let mut files = store.column_file_usage()?;
    files.sort_by(|left, right| left.name.cmp(&right.name));
    let mut sst_files = 0_u64;
    let mut blob_files = 0_u64;
    for cf in &files {
        sst_files = sst_files.saturating_add(cf.sst_files);
        blob_files = blob_files.saturating_add(cf.blob_files);
        println!(
            "  cf name={} sst_files={} sst_bytes={} blob_files={} blob_bytes={}",
            cf.name, cf.sst_files, cf.sst_bytes, cf.blob_files, cf.blob_bytes
        );
        if cf.name == "ChunkOffsetsByContentHash" && chunks > 0 {
            let per_chunk = cf.sst_bytes as f64 / f64::from(chunks);
            println!(
                "  cf_per_chunk name={} sst_bytes={} bytes_per_chunk={per_chunk:.2}",
                cf.name, cf.sst_bytes
            );
        }
    }
    println!("  files sst={sst_files} blob={blob_files}");
    Ok(())
}

/// Leaf `data_hash` of a stored proof. `None` when the bytes are shorter
/// than a leaf. This does not read the index.
fn content_hash_of_proof(proof: &[u8]) -> Option<H256> {
    const LEAF_LEN: usize = 64;
    if proof.len() < LEAF_LEN {
        return None;
    }
    let tail = proof[proof.len() - LEAF_LEN..].to_vec();
    let leaf = irys_types::get_leaf_proof(&irys_types::Base64(tail)).ok()?;
    irys_types::ProofDeserialize::hash(&leaf).map(H256)
}

fn mdbx_map_bytes(chunks: u32, path_bytes: usize, worst_case: bool) -> usize {
    let estimate = u64::from(chunks)
        .saturating_mul(path_bytes as u64)
        .saturating_mul(4)
        .saturating_add(512 * 1024 * 1024);
    // The cap is a virtual map. Growth stays at the 10 MiB step. A small
    // filled run stays inside 8 GiB. `--mid` and `--worst-case` may use the
    // estimate, up to the production submodule cap of 2 TiB.
    let ceiling = if worst_case || estimate > FILLED_MAP_CEILING {
        2 * TERABYTE as u64
    } else {
        FILLED_MAP_CEILING
    };
    let cap = estimate.clamp(1 << 30, ceiling);
    usize::try_from(cap).unwrap_or(usize::MAX)
}

/// Map size for one engine. The pinned estimate stays in [`mdbx_map_bytes`].
/// Many-root shapes add the tx-path and binding bytes that estimate omits.
fn engine_map_bytes(args: &Args) -> usize {
    let base = mdbx_map_bytes(args.chunks, args.path_bytes, args.workload.is_worst());
    if !args.workload.is_many_tx() {
        return base;
    }
    let extra = tx_count(args).saturating_mul((TX_PROOF_BYTES + 256) as u64);
    base.saturating_add(usize::try_from(extra).unwrap_or(usize::MAX))
}

fn tx_count(args: &Args) -> u64 {
    let chunks = args.data_chunks;
    match args.workload {
        Workload::SmallTxs => u64::from(chunks).div_ceil(u64::from(args.small_tx_chunks.max(1))),
        Workload::MidTxs => args.mid_spans.len() as u64,
        Workload::Worst => u64::from(chunks).div_ceil(MAX_DATA_TX_CHUNKS),
        _ => u64::from(chunks).div_ceil(u64::from(args.tx_chunks.max(1))),
    }
}

/// Transaction that owns `offset`. Tree workloads use the full leaf count.
fn place_tx(args: &Args, offset: u32) -> TxPlace {
    match args.workload {
        Workload::Worst => {
            let span = u32::try_from(MAX_DATA_TX_CHUNKS).unwrap_or(u32::MAX);
            let start = offset / span * span;
            let stored = span.min(args.data_chunks - start);
            TxPlace {
                start,
                stored,
                leaves: MAX_DATA_TX_CHUNKS,
            }
        }
        Workload::SmallTxs => {
            let span = args.small_tx_chunks.max(1);
            let start = offset / span * span;
            let stored = span.min(args.data_chunks - start);
            TxPlace {
                start,
                stored,
                leaves: u64::from(stored),
            }
        }
        Workload::MidTxs => {
            let idx = args
                .mid_spans
                .partition_point(|(start, _)| *start <= offset)
                .saturating_sub(1);
            let (start, stored) = args.mid_spans[idx];
            TxPlace {
                start,
                stored,
                leaves: u64::from(stored),
            }
        }
        _ => {
            let span = args.tx_chunks.max(1);
            let start = offset / span * span;
            let stored = span.min(args.data_chunks - start);
            TxPlace {
                start,
                stored,
                leaves: u64::from(stored),
            }
        }
    }
}

/// Seeded sizes in [`MID_TX_MIN`]..=[`MID_TX_MAX`]. A remainder under the
/// minimum is one short tail transaction.
fn mid_spans(chunks: u32) -> Vec<(u32, u32)> {
    let mut state = MID_TX_SEED;
    let mut spans = Vec::new();
    let mut at = 0_u32;
    let width = u64::from(MID_TX_MAX - MID_TX_MIN + 1);
    while at < chunks {
        state = state.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        let size = MID_TX_MIN + u32::try_from(state % width).unwrap_or(0);
        let remain = chunks - at;
        let stored = if remain < MID_TX_MIN {
            remain
        } else {
            size.min(remain)
        };
        spans.push((at, stored));
        at += stored;
    }
    spans
}

fn indexed_data_size(args: &Args, count: u32) -> u64 {
    // A short tail is still the start of a max transaction, so the row records
    // the full transaction size. The filled mode records only the chunks written.
    let chunks = if args.workload.is_worst() {
        MAX_DATA_TX_CHUNKS
    } else {
        u64::from(count)
    };
    chunks * CHUNK_SIZE
}

/// Stored path with the last byte inverted. Same length, different value.
fn rewritten_data_path(args: &Args, offset: u32) -> Vec<u8> {
    let mut path = data_path_bytes(args, offset);
    let last = path.len() - 1;
    path[last] ^= 0xff;
    path
}

fn data_path_bytes(args: &Args, offset: u32) -> Vec<u8> {
    if args.workload.is_many_tx() {
        // Node bytes depend on `(data_root, level, index)`, so two chunks under
        // one branch store the same 96 bytes. `--worst-case` stays a uniform
        // max proof so its size stays comparable with earlier runs.
        let place = place_tx(args, offset);
        let root = hash_at(u64::from(place.start).wrapping_add(0x1000_0000));
        let leaf = u64::from(offset.saturating_sub(place.start));
        return shared_data_proof(&root.0, place.leaves, leaf, CHUNK_SIZE);
    }
    if !args.workload.is_worst() {
        return fill(args.path_bytes, u64::from(offset));
    }
    if !args.uniform_proofs {
        // One max transaction per group, nodes shared by `(data_root, level,
        // index)` like a real tree. A short --chunks prefix is still a leaf of
        // the full transaction, so the proof keeps the max depth.
        let place = place_tx(args, offset);
        let root = hash_at(u64::from(place.start).wrapping_add(0x1000_0000));
        let leaf = u64::from(offset.saturating_sub(place.start));
        return shared_data_proof(&root.0, place.leaves, leaf, CHUNK_SIZE);
    }
    // Local index inside a max transaction. Depth stays at the max proof even
    // when this run writes only a prefix of the transaction.
    let local = u64::from(offset) % MAX_DATA_TX_CHUNKS;
    let leaf_end = (local + 1) * CHUNK_SIZE;
    shaped_proof(
        DATA_PROOF_LAYERS,
        leaf_end,
        u64::from(offset).wrapping_add(0x2000_0000),
    )
}

fn tx_path_bytes(args: &Args, tx_start: u32) -> Vec<u8> {
    if !args.workload.is_tree() {
        return fill(args.path_bytes, u64::from(tx_start));
    }
    let leaf_end = if args.workload.is_worst() {
        let ordinal = u64::from(tx_start) / MAX_DATA_TX_CHUNKS;
        ordinal
            .saturating_add(1)
            .saturating_mul(MAX_DATA_TX_CHUNKS)
            .saturating_mul(CHUNK_SIZE)
    } else {
        u64::from(tx_start)
            .saturating_add(1)
            .saturating_mul(CHUNK_SIZE)
    };
    shaped_proof(
        TX_PROOF_LAYERS,
        leaf_end,
        u64::from(tx_start).wrapping_add(0x4000_0000),
    )
}

/// Proof bytes in root-to-leaf order.
///
/// Each branch is two 32-byte ids plus a 32-byte note. The leaf is one id
/// plus a note. A note is 24 zero bytes and a big-endian byte offset, which
/// is the layout `to_note_vec` writes. Ids are not hashes. They only have to
/// be incompressible so compression sees the same mix as a real proof.
fn shaped_proof(layers: usize, leaf_end: u64, seed: u64) -> Vec<u8> {
    let mut out = vec![0_u8; proof_bytes(layers)];
    let mut state = mix_seed(seed);
    for level in 0..layers {
        let at = level * BRANCH_SIZE;
        fill_span(&mut out[at..at + HASH_SIZE], &mut state);
        fill_span(&mut out[at + HASH_SIZE..at + HASH_SIZE * 2], &mut state);
        write_note(
            &mut out[at + HASH_SIZE * 2..at + BRANCH_SIZE],
            branch_pivot(leaf_end, level, layers),
        );
    }
    let leaf = layers * BRANCH_SIZE;
    fill_span(&mut out[leaf..leaf + HASH_SIZE], &mut state);
    write_note(&mut out[leaf + HASH_SIZE..leaf + LEAF_SIZE], leaf_end);
    out
}

/// Left-child boundary at this level. Root notes are wide. The parent of the
/// leaf is one chunk. High bytes stay zero for a 5 TiB transaction.
fn branch_pivot(leaf_end: u64, level: usize, layers: usize) -> u64 {
    let shift = u32::try_from(layers - 1 - level).unwrap_or(40).min(40);
    let span = CHUNK_SIZE.saturating_mul(1_u64 << shift);
    let aligned = leaf_end.saturating_sub(1) / span * span;
    aligned.max(CHUNK_SIZE).min(leaf_end.max(CHUNK_SIZE))
}

fn write_note(out: &mut [u8], value: u64) {
    debug_assert_eq!(out.len(), NOTE_SIZE);
    out[..NOTE_SIZE - 8].fill(0);
    out[NOTE_SIZE - 8..].copy_from_slice(&value.to_be_bytes());
}

fn fill_span(out: &mut [u8], state: &mut u64) {
    for byte in out {
        *state = state.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        *byte = (*state >> 33) as u8;
    }
}

fn refuse_large_payload(args: &Args) -> eyre::Result<()> {
    let raw = gated_raw_bytes(args);
    let tree_over = args.workload.is_tree() && raw > LARGE_RAW_BYTES;
    let memory_over = match args.workload {
        // 8 databases of 1_000_000 rows is the default and stays under this.
        Workload::MemDbs => u64::from(args.chunks).saturating_mul(u64::from(args.dbs)) > 8_000_000,
        // 1_000_000 offsets is one step. A max transaction is past it.
        Workload::IndexSpan => args.chunks > 1_000_000,
        // A bloom that still fits in the 64 MiB cache is under this.
        Workload::OffsetRows => args.chunks > 2_000_000,
        Workload::Filled
        | Workload::Mid
        | Workload::Worst
        | Workload::FilterReuse
        | Workload::SmallTxs
        | Workload::MidTxs
        | Workload::CrcRows => false,
    };
    if !args.workload.is_mid() && !tree_over && !memory_over {
        return Ok(());
    }
    let allowed = std::env::var("IRYS_INDEX_BENCH_LARGE").ok().as_deref() == Some("1");
    if allowed {
        return Ok(());
    }
    if args.workload.is_mid() {
        eyre::bail!(
            "--mid writes {raw} raw data-path bytes per engine (about 9 GiB on disk, two engines). Set IRYS_INDEX_BENCH_LARGE=1 to run it."
        );
    }
    if memory_over {
        eyre::bail!(
            "--profile {} is past the small memory step. Set IRYS_INDEX_BENCH_LARGE=1 to run it.",
            args.workload.name()
        );
    }
    if !args.workload.is_worst() {
        eyre::bail!(
            "{} raw upper bound is {raw} bytes ({} transactions). Set IRYS_INDEX_BENCH_LARGE=1 to write them. A full partition is --chunks {PARTITION_CHUNKS}.",
            args.workload.name(),
            tx_count(args),
        );
    }
    eyre::bail!(
        "worst-case raw data proofs are {raw} bytes. Set IRYS_INDEX_BENCH_LARGE=1 to write them. A full partition is --chunks {PARTITION_CHUNKS} ({full} raw bytes per engine).",
        full = PARTITION_CHUNKS * DATA_PROOF_BYTES as u64
    );
}

/// Upper bound used by the large-run gate. Worst-case stays
/// `chunks * DATA_PROOF_BYTES`. Many-root shapes also count tx proofs.
fn gated_raw_bytes(args: &Args) -> u64 {
    let per = if args.workload.is_tree() {
        DATA_PROOF_BYTES as u64
    } else {
        args.path_bytes as u64
    };
    let proofs = u64::from(args.chunks).saturating_mul(per);
    if args.workload.is_many_tx() {
        proofs.saturating_add(tx_count(args).saturating_mul(TX_PROOF_BYTES as u64))
    } else {
        proofs
    }
}

fn dir_usage(path: &Path) -> eyre::Result<(u64, u64)> {
    let mut logical = 0_u64;
    let mut allocated = 0_u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                stack.push(entry.path());
                continue;
            }
            logical = logical.saturating_add(meta.len());
            allocated = allocated.saturating_add(file_blocks(&meta));
        }
    }
    Ok((logical, allocated))
}

fn file_blocks(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    meta.blocks().saturating_mul(512)
}

fn hash_at(n: u64) -> H256 {
    let mut raw = [0_u8; 32];
    raw[..8].copy_from_slice(&n.to_be_bytes());
    raw[8..16].copy_from_slice(&n.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_be_bytes());
    H256(raw)
}

/// SplitMix64 finaliser. `seed | 1` maps seeds 2k and 2k+1 to one stream,
/// which made adjacent 1-chunk transactions share tx-path bytes and let LZ4
/// store each pair once.
fn mix_seed(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)) | 1
}

fn fill(len: usize, seed: u64) -> Vec<u8> {
    let mut out = vec![0_u8; len];
    let mut state = mix_seed(seed);
    for byte in &mut out {
        state = state.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        *byte = (state >> 33) as u8;
    }
    out
}

/// One node in a synthetic data proof. Level 0 is the leaf.
struct ProofNode {
    level: u32,
    index: u64,
}

fn layer_widths(mut n: u64) -> Vec<u64> {
    let mut widths = Vec::new();
    if n == 0 {
        return widths;
    }
    widths.push(n);
    while n > 1 {
        n = n / 2 + n % 2;
        widths.push(n);
    }
    widths
}

/// Proof nodes in stored order: root branch first, leaf last.
///
/// A promoted odd node is not a proof record. The walk skips it, which is
/// the same rule a merkle build uses when it lifts an unpaired child.
fn proof_nodes(widths: &[u64], leaf: u64) -> Vec<ProofNode> {
    let mut branches = Vec::new();
    let mut index = leaf;
    for layer in 0..widths.len().saturating_sub(1) {
        let parent = index / 2;
        let real = parent * 2 + 1 < widths[layer];
        if real {
            branches.push(ProofNode {
                level: (layer as u32) + 1,
                index: parent,
            });
        }
        index = parent;
    }
    branches.reverse();
    branches.push(ProofNode {
        level: 0,
        index: leaf,
    });
    branches
}

fn rightmost_leaf(widths: &[u64], level: u32, index: u64) -> u64 {
    let mut idx = index;
    for layer in (1..=level).rev() {
        let below = widths[(layer as usize) - 1];
        if idx * 2 + 1 < below {
            idx = idx * 2 + 1;
        } else {
            idx *= 2;
        }
    }
    idx
}

/// Data proof whose node bytes depend only on `(data_root, level, index)`.
///
/// Ids are not hashes. They are incompressible and stable, so two chunks
/// under one branch store the same 96 bytes. This is the full-path layout.
/// It does not store tiles.
fn shared_data_proof(
    data_root: &[u8; 32],
    num_leaves: u64,
    leaf: u64,
    chunk_bytes: u64,
) -> Vec<u8> {
    if num_leaves == 0 || leaf >= num_leaves {
        return Vec::new();
    }
    let widths = layer_widths(num_leaves);
    let nodes = proof_nodes(&widths, leaf);
    let mut out = Vec::new();
    for node in nodes {
        if node.level == 0 {
            let end = leaf
                .saturating_add(1)
                .saturating_mul(chunk_bytes)
                .min(num_leaves.saturating_mul(chunk_bytes));
            fill_id(&mut out, data_root, 0, leaf, 3, 32);
            push_note(&mut out, end);
        } else {
            let pivot_leaf = rightmost_leaf(&widths, node.level - 1, node.index * 2);
            let pivot = pivot_leaf
                .saturating_add(1)
                .saturating_mul(chunk_bytes)
                .min(num_leaves.saturating_mul(chunk_bytes));
            fill_id(&mut out, data_root, node.level, node.index, 1, 32);
            fill_id(&mut out, data_root, node.level, node.index, 2, 32);
            push_note(&mut out, pivot);
        }
    }
    out
}

fn fill_id(out: &mut Vec<u8>, root: &[u8; 32], level: u32, index: u64, tag: u8, len: usize) {
    let mut state = 0x6A09_E667_F3BC_C908_u64;
    for chunk in root.chunks(8) {
        let mut wide = [0_u8; 8];
        wide[..chunk.len()].copy_from_slice(chunk);
        state = state
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(u64::from_be_bytes(wide));
    }
    state ^= u64::from(level).wrapping_mul(0x9E37_79B9);
    state ^= index.wrapping_mul(0xBF58_4E1D);
    state ^= u64::from(tag).wrapping_mul(0x94D0_49BB);
    if state == 0 {
        state = 1;
    }
    for _ in 0..len {
        state = state.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        out.push((state >> 33) as u8);
    }
}

fn push_note(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&[0_u8; 24]);
    out.extend_from_slice(&value.to_be_bytes());
}

fn prepare_dir(path: &Path, reuse: bool) -> eyre::Result<()> {
    let marked = path
        .components()
        .any(|component| matches!(component, Component::Normal(name) if name == "index-bench"));
    eyre::ensure!(
        marked,
        "refusing {}: path needs an index-bench component",
        path.display()
    );
    if reuse {
        eyre::ensure!(
            !path.join("mdbx").exists(),
            "refusing {}: directory also has mdbx/. Copy rocks/ to a new index-bench directory first",
            path.display()
        );
        eyre::ensure!(
            path.join("rocks").join("CURRENT").is_file(),
            "refusing {}: rocks/CURRENT is missing",
            path.display()
        );
        return Ok(());
    }
    if path.exists() {
        eyre::ensure!(
            path.read_dir()?.next().is_none(),
            "refusing {}: directory is not empty",
            path.display()
        );
    } else {
        fs::create_dir_all(path)?;
    }
    Ok(())
}

fn parse_args() -> eyre::Result<Args> {
    parse_arg_list(std::env::args().skip(1))
}

fn parse_arg_list(args: impl IntoIterator<Item = String>) -> eyre::Result<Args> {
    let mut dir = None;
    let mut chunks = 4096_u32;
    let mut batch = 512_u32;
    let mut tx_chunks: Option<u32> = None;
    let mut path_bytes: Option<usize> = None;
    let mut profile: Option<Workload> = None;
    let mut worst_case = false;
    let mut mid = false;
    let mut chunks_set = false;
    let mut reads = 1024_u32;
    let mut range_len = 32_u32;
    let mut engines = Engines::Both;
    let mut engines_set = false;
    let mut dbs = 1_u32;
    let mut dbs_set = false;
    let mut rocks: Option<RocksTuning> = None;
    let mut rocks_set = false;
    let mut block_cache: Option<usize> = None;
    let mut group_commit = false;
    let mut rewrite_filters = false;
    let mut small_txs: Option<u32> = None;
    let mut uniform_proofs = false;
    let mut mid_txs = false;
    let mut shuffle_run: Option<u32> = None;
    let mut txs_per_commit: Option<u32> = None;
    let mut schema = IndexSchema::V2;
    let mut schema_set = false;
    let mut data_fraction: Option<(u64, u64, f64)> = None;
    let mut with_crc = false;
    let mut entropy_fill = false;
    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "--list-rocks" => {
                print_rocks();
                std::process::exit(0);
            }
            "--dir" => dir = Some(PathBuf::from(required(&mut it, &flag)?)),
            "--chunks" => {
                chunks = required(&mut it, &flag)?.parse()?;
                chunks_set = true;
            }
            "--batch" => batch = required(&mut it, &flag)?.parse()?,
            "--tx-chunks" => tx_chunks = Some(required(&mut it, &flag)?.parse()?),
            "--path-bytes" => path_bytes = Some(required(&mut it, &flag)?.parse()?),
            "--reads" => reads = required(&mut it, &flag)?.parse()?,
            "--range-len" => range_len = required(&mut it, &flag)?.parse()?,
            "--rocks-block-cache" => block_cache = Some(required(&mut it, &flag)?.parse()?),
            "--profile" => {
                profile = Some(parse_profile(&required(&mut it, &flag)?)?);
            }
            "--engine" => {
                engines = parse_engines(&required(&mut it, &flag)?)?;
                engines_set = true;
            }
            "--dbs" => {
                dbs = required(&mut it, &flag)?.parse()?;
                dbs_set = true;
            }
            "--rocks" => {
                let name = required(&mut it, &flag)?;
                rocks = Some(RocksTuning::preset(&name)?);
                rocks_set = true;
            }
            "--worst-case" => worst_case = true,
            "--mid" => mid = true,
            "--small-txs" => small_txs = Some(required(&mut it, &flag)?.parse()?),
            "--uniform-proofs" => uniform_proofs = true,
            "--mid-txs" => mid_txs = true,
            "--shuffle-paths" => shuffle_run = Some(required(&mut it, &flag)?.parse()?),
            "--txs-per-commit" => txs_per_commit = Some(required(&mut it, &flag)?.parse()?),
            "--schema" => {
                schema = parse_schema(&required(&mut it, &flag)?)?;
                schema_set = true;
            }
            "--data-fraction" => {
                let text = required(&mut it, &flag)?;
                data_fraction = Some(parse_data_fraction(&text)?);
            }
            "--with-crc" => with_crc = true,
            "--entropy-fill" => entropy_fill = true,
            "--group-commit" => group_commit = true,
            "--rewrite-filters" => rewrite_filters = true,
            other => eyre::bail!("unknown argument {other}"),
        }
    }
    let Some(dir) = dir else {
        print_help();
        eyre::bail!("--dir is required");
    };
    let workload = resolve_workload(profile, mid, worst_case, small_txs.is_some(), mid_txs)?;
    if group_commit && (workload.is_memory() || workload.is_filter_reuse()) {
        eyre::bail!(
            "--group-commit does not apply to --profile {}",
            workload.name()
        );
    }
    if rewrite_filters && !workload.is_filter_reuse() {
        eyre::bail!("--rewrite-filters applies only to --profile filter-reuse");
    }
    if workload == Workload::CrcRows {
        if engines_set && engines != Engines::Rocks {
            eyre::bail!("--profile crc-rows runs rocks only");
        }
        engines = Engines::Rocks;
        schema = IndexSchema::V3;
        if !chunks_set {
            chunks = 262_144;
        }
    }
    if workload.is_filter_reuse() {
        if engines_set && engines != Engines::Rocks {
            eyre::bail!("--profile filter-reuse runs rocks only");
        }
        engines = Engines::Rocks;
        if !chunks_set {
            chunks = u32::try_from(MAX_DATA_TX_CHUNKS)?;
        }
    }
    if workload.is_memory() {
        if engines_set && engines != Engines::Rocks {
            eyre::bail!("--profile {} runs rocks only", workload.name());
        }
        engines = Engines::Rocks;
        if workload == Workload::MemDbs && !chunks_set {
            chunks = 1_000_000;
        }
        if workload == Workload::MemDbs && !dbs_set {
            dbs = 8;
        }
        if workload != Workload::MemDbs && !chunks_set {
            eyre::bail!("--chunks is required for --profile {}", workload.name());
        }
    }
    let mid = workload.is_mid();
    let worst_case = workload.is_worst();
    if workload.is_tree() && path_bytes.is_some() {
        eyre::bail!(
            "--path-bytes does not apply to --profile {}",
            workload.name()
        );
    }
    if workload.is_tree() && tx_chunks.is_some() {
        eyre::bail!(
            "--tx-chunks does not apply to --profile {}",
            workload.name()
        );
    }
    if let Some(n) = small_txs {
        eyre::ensure!(n > 0, "--small-txs must be > 0");
    }
    if let Some(n) = txs_per_commit {
        eyre::ensure!(n > 0, "--txs-per-commit must be > 0");
    }
    if let Some(run) = shuffle_run {
        eyre::ensure!(run > 0, "--shuffle-paths must be > 0");
        if workload.is_memory() || workload.is_filter_reuse() {
            eyre::bail!(
                "--shuffle-paths does not apply to --profile {}",
                workload.name()
            );
        }
    }
    if mid && path_bytes.is_some() {
        eyre::bail!("--path-bytes does not apply to --mid");
    }
    if mid && chunks_set {
        eyre::bail!("--chunks does not apply to --mid");
    }
    if rocks_set && !engines.includes_rocks() {
        eyre::bail!("--rocks applies only when the rocks engine runs");
    }
    if schema_set && workload.is_filter_reuse() {
        eyre::bail!("--schema does not apply to --profile filter-reuse");
    }
    if schema_set && !engines.includes_rocks() {
        eyre::bail!("--schema applies only when the rocks engine runs");
    }
    if mid {
        chunks = MID_CHUNKS;
    }
    let (fraction_numer, fraction_denom, data_fraction) = data_fraction.unwrap_or((1, 1, 1.0));
    let data_chunks = data_prefix(chunks, fraction_numer, fraction_denom)?;
    if (fraction_numer != fraction_denom || with_crc || entropy_fill)
        && (workload.is_memory() || workload.is_filter_reuse() || workload == Workload::CrcRows)
    {
        eyre::bail!(
            "--data-fraction, --with-crc, and --entropy-fill do not apply to --profile {}",
            workload.name()
        );
    }
    if (with_crc || entropy_fill) && !schema.is_v3() {
        eyre::bail!("--with-crc and --entropy-fill need --schema v3");
    }
    if (with_crc || entropy_fill) && engines.includes_mdbx() {
        eyre::bail!("--with-crc and --entropy-fill run on rocks only");
    }
    let small_tx_chunks = small_txs.unwrap_or(0);
    let tree_leaves = if worst_case {
        MAX_DATA_TX_CHUNKS
    } else if workload == Workload::SmallTxs {
        u64::from(small_tx_chunks.min(chunks).max(1))
    } else if workload == Workload::MidTxs {
        u64::from(MID_TX_MAX)
    } else {
        0
    };
    let path_bytes = if tree_leaves > 0 {
        if worst_case {
            DATA_PROOF_BYTES
        } else {
            shared_data_proof(&[0_u8; 32], tree_leaves, 0, CHUNK_SIZE).len()
        }
    } else {
        path_bytes.unwrap_or(4096)
    };
    let tx_chunks = if worst_case {
        u32::try_from(MAX_DATA_TX_CHUNKS)?
    } else if workload == Workload::SmallTxs {
        small_tx_chunks
    } else if workload == Workload::MidTxs {
        MID_TX_MAX
    } else {
        tx_chunks.unwrap_or(32)
    };
    let mid_spans = if workload == Workload::MidTxs {
        mid_spans(data_chunks)
    } else {
        Vec::new()
    };
    let mut rocks = rocks.unwrap_or_else(RocksTuning::baseline);
    if let Some(bytes) = block_cache {
        eyre::ensure!(bytes > 0, "--rocks-block-cache must be > 0");
        rocks = rocks.with_block_cache(bytes);
    }
    eyre::ensure!(dbs > 0, "--dbs must be > 0");
    eyre::ensure!(chunks > 0, "--chunks must be > 0");
    eyre::ensure!(batch > 0, "--batch must be > 0");
    eyre::ensure!(tx_chunks > 0, "--tx-chunks must be > 0");
    eyre::ensure!(path_bytes > 0, "--path-bytes must be > 0");
    eyre::ensure!(range_len > 0, "--range-len must be > 0");
    Ok(Args {
        dir,
        chunks,
        batch,
        tx_chunks,
        path_bytes,
        workload,
        engines,
        reads,
        range_len,
        rocks,
        dbs,
        group_commit,
        rewrite_filters,
        small_tx_chunks,
        mid_spans,
        shuffle_run,
        txs_per_commit,
        schema,
        uniform_proofs,
        data_fraction,
        data_chunks,
        with_crc,
        entropy_fill,
    })
}

/// `(numerator, denominator, printed value)`. The ratio is `> 0` and `<= 1`.
fn parse_data_fraction(text: &str) -> eyre::Result<(u64, u64, f64)> {
    let text = text.trim();
    let (whole, frac) = match text.split_once('.') {
        Some((whole, frac)) => (whole, frac),
        None => (text, ""),
    };
    eyre::ensure!(
        (!whole.is_empty() || !frac.is_empty())
            && whole.chars().all(|c| c.is_ascii_digit())
            && frac.chars().all(|c| c.is_ascii_digit()),
        "--data-fraction {text} is not a number"
    );
    eyre::ensure!(
        frac.len() <= 9,
        "--data-fraction {text} has too many decimal places"
    );
    let denom = 10_u64.pow(u32::try_from(frac.len()).unwrap_or(0));
    let digits = if frac.is_empty() {
        whole.to_string()
    } else if whole.is_empty() {
        frac.to_string()
    } else {
        format!("{whole}{frac}")
    };
    let numerator: u64 = digits
        .parse()
        .map_err(|_| eyre::eyre!("--data-fraction {text} is too large"))?;
    eyre::ensure!(
        numerator > 0 && numerator <= denom,
        "--data-fraction must be > 0 and <= 1"
    );
    Ok((numerator, denom, numerator as f64 / denom as f64))
}

/// First `numerator/denom` of `chunks`, rounded down. At least one offset.
fn data_prefix(chunks: u32, numerator: u64, denom: u64) -> eyre::Result<u32> {
    eyre::ensure!(denom > 0 && numerator > 0 && numerator <= denom);
    let chunks_u = u64::from(chunks);
    let n = chunks_u / denom * numerator + (chunks_u % denom) * numerator / denom;
    eyre::ensure!(
        n > 0,
        "--data-fraction covers no offsets of --chunks {chunks}"
    );
    Ok(u32::try_from(n).unwrap_or(chunks))
}

fn parse_schema(value: &str) -> eyre::Result<IndexSchema> {
    match value {
        "v2" => Ok(IndexSchema::V2),
        "v3" => Ok(IndexSchema::V3),
        other => eyre::bail!("unknown schema {other}; use v2 or v3"),
    }
}

fn parse_profile(value: &str) -> eyre::Result<Workload> {
    match value {
        "filled" => Ok(Workload::Filled),
        "mid" => Ok(Workload::Mid),
        "worst" => Ok(Workload::Worst),
        "mem-dbs" => Ok(Workload::MemDbs),
        "index-span" => Ok(Workload::IndexSpan),
        "offset-rows" => Ok(Workload::OffsetRows),
        "filter-reuse" => Ok(Workload::FilterReuse),
        "crc-rows" => Ok(Workload::CrcRows),
        other => eyre::bail!(
            "unknown profile {other}; use filled, mid, worst, mem-dbs, index-span, offset-rows, filter-reuse, or crc-rows"
        ),
    }
}

fn parse_engines(value: &str) -> eyre::Result<Engines> {
    match value {
        "both" => Ok(Engines::Both),
        "mdbx" => Ok(Engines::Mdbx),
        "rocks" => Ok(Engines::Rocks),
        other => eyre::bail!("unknown engine {other}; use both, mdbx, or rocks"),
    }
}

fn resolve_workload(
    profile: Option<Workload>,
    mid: bool,
    worst: bool,
    small_txs: bool,
    mid_txs: bool,
) -> eyre::Result<Workload> {
    let picked = [mid, worst, small_txs, mid_txs]
        .into_iter()
        .filter(|flag| *flag)
        .count();
    if picked > 1 {
        eyre::bail!("--profile, --mid, --worst-case, --small-txs, and --mid-txs disagree");
    }
    match (profile, mid, worst, small_txs, mid_txs) {
        (None, false, false, false, false) => Ok(Workload::Filled),
        (None, true, false, false, false) => Ok(Workload::Mid),
        (None, false, true, false, false) => Ok(Workload::Worst),
        (None, false, false, true, false) => Ok(Workload::SmallTxs),
        (None, false, false, false, true) => Ok(Workload::MidTxs),
        (Some(profile), false, false, false, false) => Ok(profile),
        (Some(Workload::Mid), true, false, false, false) => Ok(Workload::Mid),
        (Some(Workload::Worst), false, true, false, false) => Ok(Workload::Worst),
        (Some(Workload::SmallTxs), false, false, true, false) => Ok(Workload::SmallTxs),
        (Some(Workload::MidTxs), false, false, false, true) => Ok(Workload::MidTxs),
        _ => eyre::bail!("--profile, --mid, --worst-case, --small-txs, and --mid-txs disagree"),
    }
}

fn required(it: &mut impl Iterator<Item = String>, flag: &str) -> eyre::Result<String> {
    it.next().ok_or_else(|| eyre::eyre!("{flag} needs a value"))
}

fn print_rocks() {
    for preset in RocksTuning::presets() {
        eprintln!("{}", preset.describe());
    }
}

fn print_help() {
    let presets = RocksTuning::preset_names();
    eprintln!(
        "submodule-index-bench --dir <path/index-bench> [--chunks N] [--batch N] [--tx-chunks N] [--path-bytes N] [--reads N] [--range-len N] [--rocks-block-cache N] [--profile filled|mid|worst] [--engine both|mdbx|rocks] [--schema v2|v3] [--rocks <preset>] [--mid] [--group-commit] [--worst-case] [--small-txs N] [--mid-txs] [--shuffle-paths RUN] [--txs-per-commit N] [--data-fraction F] [--with-crc] [--entropy-fill] [--list-rocks]\n\
Writes the same synthetic submodule index on durable MDBX and on RocksDB, then times cold lookups.\n\
The directory must be empty and must contain an index-bench path component.\n\
--profile selects the workload. filled is the default. mid is --mid. worst is --worst-case and still takes --chunks.\n\
mem-dbs, index-span, and offset-rows measure RocksDB RSS. They run rocks only.\n\
mem-dbs keeps --dbs databases open (default 8) after writing --chunks offset rows in each (default 1000000) and scanning them.\n\
index-span writes --chunks offsets in one update. A max transaction is --chunks 20971520.\n\
offset-rows writes --chunks offset rows and no data paths, then does cold point reads. A full partition is --chunks 75534400.\n\
--engine both is the default. A preset sweep uses --engine rocks so MDBX is not repeated.\n\
--schema v2|v3 selects the RocksDB layout of an empty directory. The default is v2, so a size line can be compared with an older run. \
Production open of an empty directory is schema v3: one placement row and an inline data proof, and no stored tx path. \
MDBX stays schema v2. --profile filter-reuse opens a schema-v1 directory and does not take --schema.\n\
--rocks selects one RocksDB preset. The default is baseline, which is what production open uses. \
Baseline stores data-path and tx-path values in SST blocks. The blobs preset writes values at or above the blob threshold into blob files. \
Baseline uses partitioned filters and a 10-bit bloom only on families whose point lookups miss. all-blooms keeps that shape and puts the bloom on every family. full-filters is one full filter per SST, one cache shard, and a 128 MiB cache. \
Presets: {presets}. Each preset other than full-filters, fdatasync-wal-rotate, and fdatasync-wal-rotate-compact changes one setting. \
--rocks-block-cache overrides that preset's cache size and does not change the shard count. A new block size or a switch between baseline and full-filters needs an empty directory. An existing SST keeps its filter until --rewrite-filters.\n\
--profile filter-reuse opens an existing schema-v1 rocks/ directory. It refuses a directory that also contains mdbx/. \
--profile crc-rows writes schema 3.2 offset records on RocksDB and prints SST bytes per chunk for raw proofs, the production suffix trailer, a prefix trailer, and checksum-only rows. The default sample is 262144 chunks. The uniform shape is capped at 131072. The line extrapolated_partition_sst scales that sample to 75534400 offsets. \
--rewrite-filters forces a bottommost compaction of ChunkPathHashesByOffset so the existing SST is rewritten with this open's filter. A plain compact can trivial-move the file and keep the old filter. The run prints one sst line per live path-hash file and sst_cover for the sampled offsets. \
Default --chunks is 20971520, the first max transaction in the stuck worst-case run.\n\
--list-rocks prints the presets and exits.\n\
--shuffle-paths RUN writes the data paths as runs of RUN consecutive chunks in a seeded random order, like scattered data sync. Each commit still holds the data-path batch size. RUN=1 is fully random. Transaction rows stay in offset order.\n\
--txs-per-commit N overrides the transaction rows per durable commit. Production commits one block's transactions together, so --small-txs 1 should use the block's transaction count (100 at the expected cap).\n\
--small-txs N stores one data root every N chunks. Sibling chunks in one transaction share branch bytes. --mid-txs draws each transaction from 257 to 4096 chunks. Neither flag takes --path-bytes or --tx-chunks.\n\
--data-fraction F (default 1, and 0 < F <= 1) writes transactions, placements, and data rows for the first F × --chunks offsets. The transaction shape is the shape of that prefix. The header prints data_fraction and data_chunks.\n\
--with-crc stores every data row as the schema 3.2 record: the proof, then the checksum trailer from encode_offset_meta. It needs --schema v3 and --engine rocks.\n\
--entropy-fill writes a checksum-only row (crc set, no proof) for every offset outside that data region. Rows are written in offset order, one commit per data-path batch. It needs --schema v3 and --engine rocks. After settle the run prints data_rows, entropy_rows, and ChunkMetaByOffset SST bytes per data chunk and per entropy row.\n\
--reads N (default 1024) is the sample count for each shape: data_path_random, data_path_random_open, data_path_seq, serve, range, span (range plus every proof in it, as paths_in_tx does), entropy_range, recall_rows, rmw_path, rmw_root. Schema v3 RocksDB also times content_lookup and content_miss. The data shapes sample the data region only.\n\
entropy_range is a cold offset_metas_in_range of --range-len consecutive entropy offsets, sampled over the entropy region. recall_rows is the same call for --range-len offsets at a uniform start over the whole partition, so the window can mix data and entropy rows. Both use the cold-cache reopen the other shapes use. Both are skipped, with a printed reason, when there is no entropy region.\n\
data_path_random is scattered get_data_path_by_offset. data_path_random_open repeats it with the engine left open. data_path_seq is the same call on offsets 0, 1, 2, ...\n\
content_lookup is a cold prefix seek of a leaf hash that was stored. content_miss seeks a hash that was not stored. Both run only for schema v3 RocksDB, before rmw_path rewrites proofs.\n\
Each read line also prints head_p50_us, head_p99_us, and head_max_us for the first 32 samples, and the same for the tail.\n\
RocksDB prints rocks_bg before and after each shape: running compactions, running flushes, pending flags, and no_file_opens.\n\
The RocksDB engine line shows max_open_files=-1 file_opening_threads=16. Production open uses the same table cache. The bench open also skips the table-property scan.\n\
rmw_path rewrites the stored path with one byte changed. rmw_root appends a distinct placement on every sample. Both commit. Schema v3 skips rmw_root: a placement's data-root row is filled once, with the interval, and production does not read-modify-write that row.\n\
Each shape calls posix_fadvise(DONTNEED) on the engine files. It does not drop the host page cache. data_path_random_open keeps the Rocks block cache.\n\
Each engine prints mem lines. rss_kb is resident. anon_kb is heap. file_kb is mapped file pages. size_kb is virtual.\n\
--group-commit makes both engines commit {MID_MDBX_TXS_PER_COMMIT} transactions and {MID_MDBX_PATH_BATCH} data-path rows (or --batch, when that is larger). Without it, only --mid MDBX uses those sizes. RocksDB stays at one transaction and --batch rows.\n\
--mid writes {MID_CHUNKS} chunks of 4096-byte paths (about 4 GiB raw per engine). \
It needs IRYS_INDEX_BENCH_LARGE=1. Do not combine it with --chunks, --path-bytes, or --worst-case.\n\
--worst-case stores a {DATA_PROOF_BYTES}-byte max data proof on every chunk \
(tree nodes shared between chunks of one transaction, like a real proof; --uniform-proofs gives every chunk an unrelated random proof instead, the layout of runs before 2026-10-09) \
(a max transaction is {MAX_DATA_TX_CHUNKS} chunks). \
A full partition is --chunks {PARTITION_CHUNKS} and requires IRYS_INDEX_BENCH_LARGE=1."
    );
}

#[cfg(test)]
mod tests {
    use irys_database::submodule::BLOCK_CACHE_BYTES;

    use super::{
        Args, DATA_PROOF_BYTES, DATA_PROOF_LAYERS, Engines, FILLED_MAP_CEILING, HASH_SIZE,
        HEAD_SAMPLES, IndexSchema, LEAF_SIZE, MAX_DATA_TX_CHUNKS, MID_CHUNKS, MID_MDBX_PATH_BATCH,
        MID_MDBX_TXS_PER_COMMIT, NOTE_SIZE, PARTITION_CHUNKS, SubmoduleIndex, TX_PROOF_BYTES,
        TX_PROOF_LAYERS, Workload, bloom_bytes, commits_for, data_path_bytes, hash_at, head_tail,
        mdbx_map_bytes, memory_predict, mid_spans, pairing_layers, parse_arg_list, parse_diskstats,
        parse_mount_line, parse_proc_status, percentile, place_tx, proof_bytes, ranks,
        rewritten_data_path, sample_offset, seq_offset, shaped_proof, shuffled_runs, tx_count,
        tx_path_bytes, write_data_paths, write_entropy_rows, write_tx_index,
    };

    #[test]
    fn adjacent_seeds_fill_distinct_bytes() {
        assert_ne!(super::fill(64, 2), super::fill(64, 3));
        assert_ne!(super::fill(64, 0), super::fill(64, 1));
    }

    #[test]
    fn shuffled_runs_cover_every_offset_once() {
        for (chunks, run) in [(1_u32, 1_u32), (10, 3), (1000, 1), (1000, 64), (4097, 512)] {
            let runs = shuffled_runs(chunks, run);
            let mut seen = vec![false; chunks as usize];
            for start in runs {
                let end = start.saturating_add(run).min(chunks);
                for at in start..end {
                    assert!(!seen[at as usize], "offset {at} visited twice");
                    seen[at as usize] = true;
                }
            }
            assert!(seen.iter().all(|hit| *hit));
        }
        let runs = shuffled_runs(1000, 1);
        assert_ne!(runs, (0..1000).collect::<Vec<_>>());
        assert_eq!(runs, shuffled_runs(1000, 1));
    }

    #[test]
    fn max_proofs_match_the_merkle_layout() {
        assert_eq!(pairing_layers(1), 0);
        assert_eq!(proof_bytes(0), LEAF_SIZE);
        assert_eq!(pairing_layers(2), 1);
        assert_eq!(proof_bytes(1), 160);
        assert_eq!(DATA_PROOF_LAYERS, 25);
        assert_eq!(DATA_PROOF_BYTES, 2464);
        assert_eq!(TX_PROOF_LAYERS, 7);
        assert_eq!(TX_PROOF_BYTES, 736);
        assert!((DATA_PROOF_BYTES - LEAF_SIZE).is_multiple_of(HASH_SIZE * 2 + NOTE_SIZE));
    }

    #[test]
    fn shaped_proof_keeps_zero_notes_and_changes_per_chunk() {
        let first = shaped_proof(DATA_PROOF_LAYERS, 262_144, 1);
        let second = shaped_proof(DATA_PROOF_LAYERS, 524_288, 2);
        assert_eq!(first.len(), DATA_PROOF_BYTES);
        assert_ne!(first, second);
        assert!(first[..HASH_SIZE].iter().any(|byte| *byte != 0));
        let leaf = &first[first.len() - LEAF_SIZE..];
        assert!(
            leaf[HASH_SIZE..HASH_SIZE + NOTE_SIZE - 8]
                .iter()
                .all(|byte| *byte == 0)
        );
        assert_eq!(&leaf[leaf.len() - 8..], &262_144_u64.to_be_bytes());
        let branch_note = &first[HASH_SIZE * 2..HASH_SIZE * 2 + NOTE_SIZE];
        assert!(branch_note[..NOTE_SIZE - 8].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn partition_is_three_max_transactions_and_a_prefix() {
        let max = u32::try_from(MAX_DATA_TX_CHUNKS).unwrap();
        let partition = u32::try_from(PARTITION_CHUNKS).unwrap();
        assert_eq!(partition / max, 3);
        assert_eq!(partition % max, 12_619_840);
        assert_eq!(3 * max + 12_619_840, partition);
    }

    #[test]
    fn proc_status_splits_file_pages_from_anonymous() {
        let text = "\
Name:\tbench
VmSize:\t  17000000 kB
VmRSS:\t   8600000 kB
RssAnon:\t    120000 kB
RssFile:\t   8480000 kB
VmHWM:\t   8600000 kB
";
        let status = parse_proc_status(text);
        assert_eq!(status.size_kb, 17_000_000);
        assert_eq!(status.rss_kb, 8_600_000);
        assert_eq!(status.anon_kb, 120_000);
        assert_eq!(status.file_kb, 8_480_000);
        assert_eq!(parse_proc_status("VmRSS:\t 10 kB\n").anon_kb, 0);
    }

    #[test]
    fn mid_groups_mdbx_commits_and_leaves_rocks_alone() {
        let mdbx = commits_for(true, 512, true, false);
        assert_eq!(mdbx.txs_per_commit, MID_MDBX_TXS_PER_COMMIT);
        assert_eq!(mdbx.path_batch, MID_MDBX_PATH_BATCH);
        let rocks = commits_for(true, 512, false, false);
        assert_eq!(rocks.txs_per_commit, 1);
        assert_eq!(rocks.path_batch, 512);
        let raised = commits_for(true, 4096, true, false);
        assert_eq!(raised.path_batch, 4096);
        let filled = commits_for(false, 512, true, false);
        assert_eq!(filled.txs_per_commit, 1);
        assert_eq!(filled.path_batch, 512);
    }

    #[test]
    fn group_commit_uses_the_same_sizes_on_both_engines() {
        let rocks = commits_for(true, 512, false, true);
        let mdbx = commits_for(true, 512, true, true);
        assert_eq!(rocks.txs_per_commit, MID_MDBX_TXS_PER_COMMIT);
        assert_eq!(rocks.path_batch, MID_MDBX_PATH_BATCH);
        assert_eq!(mdbx.txs_per_commit, rocks.txs_per_commit);
        assert_eq!(mdbx.path_batch, rocks.path_batch);
        let filled = commits_for(false, 512, false, true);
        assert_eq!(filled.txs_per_commit, MID_MDBX_TXS_PER_COMMIT);
        assert_eq!(filled.path_batch, MID_MDBX_PATH_BATCH);
        assert_eq!(commits_for(false, 4096, true, true).path_batch, 4096);
    }

    #[test]
    fn comparison_map_stays_small_and_mid_uses_the_estimate() {
        assert_eq!(mdbx_map_bytes(16_384, 4096, false), 1 << 30);
        let estimate = u64::from(MID_CHUNKS) * 4096 * 4 + 512 * 1024 * 1024;
        assert!(estimate > FILLED_MAP_CEILING);
        assert_eq!(
            mdbx_map_bytes(MID_CHUNKS, 4096, false),
            usize::try_from(estimate).unwrap()
        );
        let worst = PARTITION_CHUNKS * DATA_PROOF_BYTES as u64 * 4 + 512 * 1024 * 1024;
        let chunks = u32::try_from(PARTITION_CHUNKS).unwrap();
        assert_eq!(
            mdbx_map_bytes(chunks, DATA_PROOF_BYTES, true),
            usize::try_from(worst).unwrap()
        );
    }

    #[test]
    fn rewrite_changes_only_the_last_byte() {
        let args = Args {
            dir: std::path::PathBuf::from("index-bench"),
            chunks: 8,
            batch: 8,
            tx_chunks: 4,
            path_bytes: 4096,
            workload: Workload::Filled,
            engines: Engines::Both,
            reads: 1,
            range_len: 4,
            rocks: irys_database::submodule::RocksTuning::baseline().with_block_cache(1024),
            dbs: 1,
            group_commit: false,
            rewrite_filters: false,
            small_tx_chunks: 0,
            mid_spans: Vec::new(),
            shuffle_run: None,
            txs_per_commit: None,
            schema: IndexSchema::V2,
            uniform_proofs: false,
            data_fraction: 1.0,
            data_chunks: 8,
            with_crc: false,
            entropy_fill: false,
        };
        let stored = data_path_bytes(&args, 3);
        let rewritten = rewritten_data_path(&args, 3);
        assert_eq!(rewritten.len(), stored.len());
        assert_eq!(
            &rewritten[..rewritten.len() - 1],
            &stored[..stored.len() - 1]
        );
        assert_ne!(rewritten.last(), stored.last());
    }

    fn parse(args: &[&str]) -> eyre::Result<Args> {
        parse_arg_list(args.iter().map(|arg| (*arg).to_string()))
    }

    #[test]
    fn profile_selects_one_rocks_preset() {
        let args = parse(&[
            "--dir",
            "/tmp/x/index-bench",
            "--profile",
            "mid",
            "--engine",
            "rocks",
            "--rocks",
            "block-16k",
        ])
        .unwrap();
        assert_eq!(args.chunks, MID_CHUNKS);
        assert_eq!(args.path_bytes, 4096);
        assert_eq!(args.workload, Workload::Mid);
        assert_eq!(args.engines, Engines::Rocks);
        assert_eq!(args.rocks.name, "block-16k");
        assert_eq!(args.rocks.block_bytes, 16 * 1024);
        assert_eq!(args.rocks.block_cache_bytes, BLOCK_CACHE_BYTES);
        assert!(args.rocks.share_resources);

        let cache = parse(&[
            "--dir",
            "index-bench",
            "--rocks",
            "cache-1g",
            "--rocks-block-cache",
            "4096",
        ])
        .unwrap();
        assert_eq!(cache.rocks.name, "cache-1g");
        assert_eq!(cache.rocks.block_cache_bytes, 4096);
        assert!(!cache.rocks.share_resources);
        assert_eq!(cache.engines, Engines::Both);

        let defaults = parse(&["--dir", "index-bench"]).unwrap();
        assert_eq!(defaults.workload, Workload::Filled);
        assert_eq!(defaults.engines, Engines::Both);
        assert_eq!(defaults.rocks.name, "baseline");
        assert_eq!(defaults.rocks.block_cache_bytes, BLOCK_CACHE_BYTES);
        assert!(defaults.rocks.share_resources);
        assert_eq!(defaults.chunks, 4096);
        assert_eq!(defaults.schema, IndexSchema::V2);
        assert!(!defaults.group_commit);

        let grouped = parse(&["--dir", "index-bench", "--mid", "--group-commit"]).unwrap();
        assert!(grouped.group_commit);
        assert_eq!(grouped.workload, Workload::Mid);
        assert!(
            parse(&[
                "--dir",
                "index-bench",
                "--profile",
                "mem-dbs",
                "--group-commit"
            ])
            .is_err()
        );

        let mid = parse(&["--dir", "index-bench", "--mid"]).unwrap();
        assert_eq!(mid.workload, Workload::Mid);
        let worst = parse(&[
            "--dir",
            "index-bench",
            "--profile",
            "worst",
            "--chunks",
            "8",
        ])
        .unwrap();
        assert_eq!(worst.workload, Workload::Worst);
        assert_eq!(worst.chunks, 8);
        assert_eq!(worst.path_bytes, DATA_PROOF_BYTES);

        let shuffled = parse(&[
            "--dir",
            "index-bench",
            "--chunks",
            "128",
            "--shuffle-paths",
            "64",
        ])
        .unwrap();
        assert_eq!(shuffled.shuffle_run, Some(64));
        assert!(parse(&["--dir", "index-bench", "--shuffle-paths", "0"]).is_err());
        let per_block = parse(&[
            "--dir",
            "index-bench",
            "--small-txs",
            "1",
            "--txs-per-commit",
            "100",
        ])
        .unwrap();
        assert_eq!(per_block.txs_per_commit, Some(100));
        assert_eq!(per_block.workload, Workload::SmallTxs);

        assert!(parse(&["--dir", "index-bench", "--profile", "filled", "--mid"]).is_err());
        assert!(parse(&["--dir", "index-bench", "--profile", "mid", "--chunks", "10"]).is_err());
        assert!(
            parse(&[
                "--dir",
                "index-bench",
                "--engine",
                "mdbx",
                "--rocks",
                "baseline"
            ])
            .is_err()
        );
        assert!(parse(&["--dir", "index-bench", "--rocks", "missing"]).is_err());
        assert!(parse(&["--dir", "index-bench", "--profile", "huge"]).is_err());
    }

    #[test]
    fn memory_profiles_select_rocks_and_the_step() {
        let dbs = parse(&["--dir", "index-bench", "--profile", "mem-dbs"]).unwrap();
        assert_eq!(dbs.workload, Workload::MemDbs);
        assert_eq!(dbs.engines, Engines::Rocks);
        assert_eq!(dbs.dbs, 8);
        assert_eq!(dbs.chunks, 1_000_000);
        assert_eq!(bloom_bytes(u64::from(dbs.chunks)), 1_250_000);
        assert_eq!(bloom_bytes(PARTITION_CHUNKS), 94_418_000);

        let span = parse(&[
            "--dir",
            "index-bench",
            "--profile",
            "index-span",
            "--chunks",
            "20971520",
        ])
        .unwrap();
        assert_eq!(span.chunks, 20_971_520);
        assert_eq!(span.dbs, 1);

        let rows = parse(&[
            "--dir",
            "index-bench",
            "--profile",
            "offset-rows",
            "--chunks",
            "75534400",
        ])
        .unwrap();
        assert_eq!(rows.chunks, 75_534_400);
        assert!(bloom_bytes(u64::from(rows.chunks)) > BLOCK_CACHE_BYTES as u64);
        let predicted = memory_predict(&rows);
        assert!(predicted.contains("shared_resources=on"), "{predicted}");
        assert!(predicted.contains("bloom_fits=yes"), "{predicted}");
        assert!(
            predicted.contains(&format!(
                "block_cache_bytes={}",
                irys_types::DEFAULT_ROCKSDB_BLOCK_CACHE_BYTES
            )),
            "{predicted}"
        );

        assert!(parse(&["--dir", "index-bench", "--profile", "index-span"]).is_err());
        assert!(
            parse(&[
                "--dir",
                "index-bench",
                "--profile",
                "mem-dbs",
                "--engine",
                "mdbx"
            ])
            .is_err()
        );
    }

    #[test]
    fn filter_reuse_defaults_to_the_first_max_tx_and_rocks() {
        let args = parse(&["--dir", "index-bench", "--profile", "filter-reuse"]).unwrap();
        assert_eq!(args.workload, Workload::FilterReuse);
        assert_eq!(args.engines, Engines::Rocks);
        assert_eq!(args.chunks, 20_971_520);
        assert!(!args.rewrite_filters);
        assert!(args.rocks.partition_filters);
        assert!(args.rocks.share_resources);

        let rewrite = parse(&[
            "--dir",
            "index-bench",
            "--profile",
            "filter-reuse",
            "--rocks",
            "full-filters",
            "--rewrite-filters",
        ])
        .unwrap();
        assert!(rewrite.rewrite_filters);
        assert!(!rewrite.rocks.partition_filters);
        assert_eq!(rewrite.rocks.block_cache_bytes, 128 * 1024 * 1024);
        assert_eq!(rewrite.rocks.cache_shard_bits, 0);
        assert!(!rewrite.rocks.share_resources);

        assert!(parse(&["--dir", "index-bench", "--rewrite-filters"]).is_err());
        assert!(
            parse(&[
                "--dir",
                "index-bench",
                "--profile",
                "filter-reuse",
                "--engine",
                "both"
            ])
            .is_err()
        );
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let samples = [10, 20, 30, 40];
        assert_eq!(percentile(&samples, 50), 20);
        assert_eq!(percentile(&samples, 99), 40);
        assert_eq!(percentile(&[], 50), 0);
    }

    #[test]
    fn head_percentiles_keep_arrival_order() {
        let mut samples = vec![5_u64; HEAD_SAMPLES];
        samples.extend([100, 200, 300, 400]);
        let (head, tail) = head_tail(&samples);
        assert_eq!(head.n, HEAD_SAMPLES as u64);
        assert_eq!(head.p50, 5);
        assert_eq!(head.max, 5);
        assert_eq!(tail.n, 4);
        assert_eq!(tail.p50, 200);
        assert_eq!(tail.p99, 400);
        assert_eq!(tail.max, 400);
        let all = ranks(&samples);
        assert_eq!(all.n, 36);
        assert_eq!(all.p50, 5);
        assert_eq!(all.max, 400);
        let short = head_tail(&[7, 9]);
        assert_eq!(short.0.n, 2);
        assert_eq!(short.1.n, 0);
        assert_eq!(short.1.max, 0);
    }

    #[test]
    fn sample_offsets_stay_inside_the_span() {
        for i in 0..64 {
            assert!(sample_offset(i, 7) < 7);
        }
        assert_eq!(sample_offset(0, 1), 0);
        assert_eq!(seq_offset(0, 10), 0);
        assert_eq!(seq_offset(3, 10), 3);
        assert_eq!(seq_offset(10, 10), 0);
    }

    #[test]
    fn mount_and_diskstats_parsers_match_proc_layout() {
        let line = "36 35 8:48 / /mnt/sm\\0403 rw,relatime - xfs /dev/sdd rw";
        let (mount, major, minor) = parse_mount_line(line).unwrap();
        assert_eq!(mount, "/mnt/sm 3");
        assert_eq!((major, minor), (8, 48));
        let stats = "   8      48 sdd 10 0 20 1 4 0 8 1 0 0 0 0 0 0 0\n";
        let snap = parse_diskstats(stats, 8, 48).unwrap();
        assert_eq!(snap.reads, 10);
        assert_eq!(snap.read_sectors, 20);
        assert_eq!(snap.writes, 4);
        assert_eq!(snap.write_sectors, 8);
        assert!(parse_diskstats(stats, 8, 0).is_none());
    }

    #[test]
    fn tree_shapes_keep_unique_roots_and_a_shared_branch() {
        let small = parse(&["--dir", "index-bench", "--chunks", "10", "--small-txs", "3"]).unwrap();
        assert_eq!(small.workload, Workload::SmallTxs);
        assert_eq!(tx_count(&small), 4);
        let mut starts = Vec::new();
        let mut at = 0_u32;
        while at < small.chunks {
            let place = place_tx(&small, at);
            assert_eq!(place.start, at);
            starts.push(place.start);
            assert_eq!(tx_path_bytes(&small, at).len(), TX_PROOF_BYTES);
            at += place.stored;
        }
        assert_eq!(starts, vec![0, 3, 6, 9]);
        assert_eq!(place_tx(&small, 9).leaves, 1);
        let left = data_path_bytes(&small, 0);
        let right = data_path_bytes(&small, 1);
        assert_eq!(&left[..96], &right[..96]);
        assert_ne!(left, right);
        let other = data_path_bytes(&small, 3);
        assert_ne!(&left[..96], &other[..96]);
        assert_ne!(tx_path_bytes(&small, 0), tx_path_bytes(&small, 1));

        let spans = mid_spans(10_000);
        let mut covered = 0_u32;
        for (index, (start, stored)) in spans.iter().enumerate() {
            assert_eq!(*start, covered);
            if index + 1 < spans.len() {
                assert!((257..=4096).contains(stored), "{stored}");
            }
            covered += stored;
        }
        assert_eq!(covered, 10_000);
        let mid = parse(&[
            "--dir",
            "index-bench",
            "--chunks",
            "10000",
            "--mid-txs",
            "--engine",
            "rocks",
        ])
        .unwrap();
        assert_eq!(mid.mid_spans, spans);
        assert_eq!(tx_count(&mid), spans.len() as u64);

        let worst = parse(&[
            "--dir",
            "index-bench",
            "--worst-case",
            "--chunks",
            "8",
            "--engine",
            "rocks",
        ])
        .unwrap();
        assert_eq!(worst.engines, Engines::Rocks);
        assert_eq!(place_tx(&worst, 3).leaves, MAX_DATA_TX_CHUNKS);
        assert_eq!(data_path_bytes(&worst, 0).len(), DATA_PROOF_BYTES);
        assert_eq!(data_path_bytes(&worst, 1).len(), DATA_PROOF_BYTES);

        let v3 = parse(&[
            "--dir",
            "index-bench",
            "--schema",
            "v3",
            "--engine",
            "rocks",
        ])
        .unwrap();
        assert_eq!(v3.schema, IndexSchema::V3);
        assert!(parse(&["--dir", "index-bench", "--schema", "v1"]).is_err());
        assert!(parse(&["--dir", "index-bench", "--engine", "mdbx", "--schema", "v3"]).is_err());
        assert!(
            parse(&[
                "--dir",
                "index-bench",
                "--profile",
                "filter-reuse",
                "--schema",
                "v2"
            ])
            .is_err()
        );

        assert!(parse(&["--dir", "index-bench", "--small-txs", "1", "--worst-case"]).is_err());
        assert!(parse(&["--dir", "index-bench", "--small-txs", "0"]).is_err());
        assert!(parse(&["--dir", "index-bench", "--txs-per-commit", "0"]).is_err());
        assert!(
            parse(&[
                "--dir",
                "index-bench",
                "--mid-txs",
                "--chunks",
                "10",
                "--path-bytes",
                "8"
            ])
            .is_err()
        );
    }

    #[test]
    fn small_txs_write_one_row_set_per_data_root() -> eyre::Result<()> {
        use irys_database::submodule::SubmoduleStore as _;
        use irys_testing_utils::utils::TempDirBuilder;
        use irys_types::PartitionChunkOffset;

        let args = parse(&[
            "--dir",
            "index-bench",
            "--chunks",
            "5",
            "--small-txs",
            "2",
            "--engine",
            "rocks",
            "--reads",
            "0",
            "--batch",
            "5",
        ])
        .unwrap();
        let dir = TempDirBuilder::new()
            .prefix("index_bench_small_txs")
            .build();
        assert_eq!(args.schema, IndexSchema::V2);
        let store = SubmoduleIndex::open_rocks_bench(dir.path().join("rocks"), args.rocks, false)?;
        write_tx_index(&store, &args, 8)?;
        write_data_paths(&store, &args, 5)?;
        let mut at = 0_u32;
        let mut roots = Vec::new();
        while at < args.chunks {
            let place = place_tx(&args, at);
            let tx_hash = hash_at(u64::from(place.start));
            let root = hash_at(u64::from(place.start) + 0x1000_0000);
            roots.push(root);
            let tx_path = store
                .view(|tx| tx.get_full_tx_path(tx_hash))?
                .expect("tx path");
            assert_eq!(tx_path.len(), TX_PROOF_BYTES);
            let binding = store
                .view(|tx| tx.get_tx_leaf_binding(tx_hash))?
                .expect("binding");
            assert_eq!(binding.data_root, root);
            let infos = store
                .view(|tx| tx.get_data_root_infos_for_data_root(root))?
                .expect("infos");
            assert_eq!(infos.0.len(), 1);
            assert_eq!(infos.0[0].data_size, place.leaves * 256 * 1024);
            assert_eq!(
                infos.0[0].start_offset.0,
                i32::try_from(place.start).unwrap()
            );
            let job = store
                .view(|tx| tx.get_pending_body_migration(PartitionChunkOffset::from(place.start)))?
                .expect("pending job");
            assert_eq!(job.data_root, root);
            assert_eq!(job.data_size, place.leaves * 256 * 1024);
            assert_eq!(job.attempts, 0);
            assert_eq!(job.block_height, 1);
            for step in 0..place.stored {
                let offset = place.start + step;
                let got = store
                    .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(offset)))?
                    .expect("proof");
                assert_eq!(got, data_path_bytes(&args, offset));
                let hashes = store
                    .view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(offset)))?
                    .expect("hashes");
                assert_eq!(hashes.tx_path_hash, Some(tx_hash));
            }
            at += place.stored;
        }
        assert_eq!(roots.len(), 3);
        assert_eq!(
            roots.iter().collect::<std::collections::HashSet<_>>().len(),
            3
        );
        Ok(())
    }

    #[test]
    fn small_txs_v3_stores_a_placement_and_no_tx_path() -> eyre::Result<()> {
        use irys_database::submodule::SubmoduleStore as _;
        use irys_testing_utils::utils::TempDirBuilder;
        use irys_types::PartitionChunkOffset;

        let args = parse(&[
            "--dir",
            "index-bench",
            "--chunks",
            "5",
            "--small-txs",
            "2",
            "--schema",
            "v3",
            "--engine",
            "rocks",
            "--reads",
            "0",
            "--batch",
            "5",
        ])
        .unwrap();
        assert_eq!(args.schema, IndexSchema::V3);
        let dir = TempDirBuilder::new()
            .prefix("index_bench_small_txs_v3")
            .build();
        let store = SubmoduleIndex::open_rocks_bench(dir.path().join("rocks"), args.rocks, true)?;
        write_tx_index(&store, &args, 8)?;
        write_data_paths(&store, &args, 5)?;
        let mut at = 0_u32;
        let mut roots = Vec::new();
        while at < args.chunks {
            let place = place_tx(&args, at);
            let tx_hash = hash_at(u64::from(place.start));
            let root = hash_at(u64::from(place.start) + 0x1000_0000);
            roots.push(root);
            assert!(store.view(|tx| tx.get_full_tx_path(tx_hash))?.is_none());
            assert!(store.view(|tx| tx.get_tx_leaf_binding(tx_hash))?.is_none());
            let got = store
                .view(|tx| tx.placement_at(PartitionChunkOffset::from(place.start)))?
                .expect("placement");
            assert_eq!(got.data_root, root);
            assert_eq!(got.data_size, place.leaves * 256 * 1024);
            assert_eq!(got.tx_start_offset.0, i32::try_from(place.start).unwrap());
            assert_eq!(got.start.0, place.start);
            assert_eq!(got.end.0, place.start + place.stored - 1);
            assert!(got.ledger.is_none());
            assert!(got.block_height.is_none());
            assert!(got.tx_index.is_none());
            for step in 0..place.stored {
                let offset = place.start + step;
                let proof = store
                    .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(offset)))?
                    .expect("proof");
                assert_eq!(proof, data_path_bytes(&args, offset));
                let hashes = store
                    .view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(offset)))?
                    .expect("hashes");
                assert_eq!(hashes.tx_path_hash, Some(root));
                assert_eq!(
                    hashes.data_path_hash,
                    Some(irys_types::H256(irys_types::hash_sha256(&proof)))
                );
            }
            at += place.stored;
        }
        assert_eq!(roots.len(), 3);
        Ok(())
    }

    #[test]
    fn rmw_root_on_a_tiny_v3_index_keeps_the_placement() -> eyre::Result<()> {
        // Breaks if the shape appends an info schema v3 rejects, or if it
        // changes the placement data_size that production stored.
        use irys_database::submodule::SubmoduleStore as _;
        use irys_testing_utils::utils::TempDirBuilder;
        use irys_types::PartitionChunkOffset;

        let args = parse(&[
            "--dir",
            "index-bench",
            "--chunks",
            "4",
            "--small-txs",
            "2",
            "--schema",
            "v3",
            "--engine",
            "rocks",
            "--reads",
            "1",
            "--batch",
            "4",
        ])
        .unwrap();
        let dir = TempDirBuilder::new()
            .prefix("index_bench_rmw_root_v3")
            .build();
        let store = SubmoduleIndex::open_rocks_bench(dir.path().join("rocks"), args.rocks, true)?;
        write_tx_index(&store, &args, 8)?;
        let before = store
            .view(|tx| tx.placement_at(PartitionChunkOffset::from(0)))?
            .expect("placement");
        let outcome = super::rmw_root_sample(&store, &args, 0)?;
        let super::RmwRoot::Skipped(reason) = outcome else {
            panic!("v3 rmw_root wrote a data-root row");
        };
        assert!(reason.contains("schema v3"), "{reason}");
        assert!(reason.contains("read-modify-write"), "{reason}");
        let after = store
            .view(|tx| tx.placement_at(PartitionChunkOffset::from(0)))?
            .expect("placement");
        assert_eq!(after.data_size, before.data_size);
        assert_eq!(after.data_root, before.data_root);
        assert_eq!(after.tx_start_offset, before.tx_start_offset);
        Ok(())
    }

    #[test]
    fn data_fraction_keeps_the_tx_shape_of_the_prefix() {
        // Breaks if the fraction is applied to the full span, or if 0.1 of a
        // partition is not an exact tenth.
        let tenth = parse(&[
            "--dir",
            "index-bench",
            "--chunks",
            "75534400",
            "--schema",
            "v3",
            "--engine",
            "rocks",
            "--data-fraction",
            "0.1",
        ])
        .unwrap();
        assert_eq!(tenth.data_chunks, 7_553_440);
        assert_eq!(tenth.chunks, 75_534_400);
        assert!((tenth.data_fraction - 0.1).abs() < 1e-12);

        let half = parse(&[
            "--dir",
            "index-bench",
            "--chunks",
            "10",
            "--small-txs",
            "3",
            "--schema",
            "v3",
            "--engine",
            "rocks",
            "--data-fraction",
            "0.5",
        ])
        .unwrap();
        assert_eq!(half.data_chunks, 5);
        assert_eq!(place_tx(&half, 0).stored, 3);
        assert_eq!(place_tx(&half, 3).stored, 2);

        let mid = parse(&[
            "--dir",
            "index-bench",
            "--chunks",
            "10000",
            "--mid-txs",
            "--engine",
            "rocks",
            "--schema",
            "v3",
            "--data-fraction",
            "0.5",
        ])
        .unwrap();
        assert_eq!(mid.data_chunks, 5_000);
        let (start, stored) = mid.mid_spans.last().copied().unwrap();
        assert_eq!(start + stored, 5_000);
        assert!(mid.mid_spans.iter().all(|(start, _)| *start < 5_000));

        assert!(parse(&["--dir", "index-bench", "--data-fraction", "0"]).is_err());
        assert!(parse(&["--dir", "index-bench", "--data-fraction", "1.1"]).is_err());
        assert!(
            parse(&[
                "--dir",
                "index-bench",
                "--entropy-fill",
                "--engine",
                "rocks"
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "--dir",
                "index-bench",
                "--with-crc",
                "--schema",
                "v3",
                "--engine",
                "both"
            ])
            .is_err()
        );
        let defaults = parse(&["--dir", "index-bench"]).unwrap();
        assert_eq!(defaults.data_chunks, defaults.chunks);
        assert!(!defaults.with_crc);
        assert!(!defaults.entropy_fill);
    }

    #[test]
    fn with_crc_data_rows_decode_with_the_checksum() -> eyre::Result<()> {
        // Breaks if the data row stays a proof with no trailer, or if the
        // stored checksum is not the one encode_offset_meta writes.
        use irys_database::submodule::SubmoduleStore as _;
        use irys_testing_utils::utils::TempDirBuilder;
        use irys_types::PartitionChunkOffset;

        let args = parse(&[
            "--dir",
            "index-bench",
            "--chunks",
            "4",
            "--small-txs",
            "2",
            "--schema",
            "v3",
            "--engine",
            "rocks",
            "--with-crc",
            "--reads",
            "0",
            "--batch",
            "4",
        ])
        .unwrap();
        let dir = TempDirBuilder::new().prefix("index_bench_with_crc").build();
        let store = SubmoduleIndex::open_rocks_bench(dir.path().join("rocks"), args.rocks, true)?;
        write_tx_index(&store, &args, 8)?;
        write_data_paths(&store, &args, 4)?;
        for (offset, crc) in [
            (0_u32, 0_u32),
            (1, 0x0100_0001),
            (2, 0x0200_0002),
            (3, 0x0300_0003),
        ] {
            let meta = store
                .view(|tx| tx.get_offset_meta(PartitionChunkOffset::from(offset)))?
                .unwrap_or_else(|| panic!("missing row {offset}"));
            assert_eq!(meta.crc, Some(crc), "offset {offset}");
            assert_eq!(
                meta.proof.as_deref(),
                Some(data_path_bytes(&args, offset).as_slice()),
                "offset {offset}"
            );
        }
        Ok(())
    }

    #[test]
    fn entropy_fill_half_is_checksum_only_and_the_new_reads_count_rows() -> eyre::Result<()> {
        // Breaks if the second half gets a proof, if the first half loses its
        // proof, or if the new shapes do not read those rows.
        use irys_database::submodule::SubmoduleStore as _;
        use irys_testing_utils::utils::TempDirBuilder;
        use irys_types::PartitionChunkOffset;

        let args = parse(&[
            "--dir",
            "index-bench",
            "--chunks",
            "4",
            "--small-txs",
            "2",
            "--schema",
            "v3",
            "--engine",
            "rocks",
            "--data-fraction",
            "0.5",
            "--entropy-fill",
            "--reads",
            "1",
            "--range-len",
            "2",
            "--batch",
            "2",
        ])
        .unwrap();
        assert_eq!(args.data_chunks, 2);
        let dir = TempDirBuilder::new()
            .prefix("index_bench_entropy_fill")
            .build();
        let path = dir.path().join("rocks");
        let store = SubmoduleIndex::open_rocks_bench(&path, args.rocks, true)?;
        write_tx_index(&store, &args, 8)?;
        write_data_paths(&store, &args, 2)?;
        write_entropy_rows(&store, &args, 2)?;
        for offset in 0..2_u32 {
            let meta = store
                .view(|tx| tx.get_offset_meta(PartitionChunkOffset::from(offset)))?
                .unwrap_or_else(|| panic!("missing data row {offset}"));
            assert!(meta.crc.is_none(), "data row {offset} stored a checksum");
            assert_eq!(
                meta.proof.as_deref(),
                Some(data_path_bytes(&args, offset).as_slice())
            );
            assert!(
                store
                    .view(|tx| tx.placement_at(PartitionChunkOffset::from(offset)))?
                    .is_some()
            );
        }
        for (offset, crc) in [(2_u32, 0x0200_0002), (3, 0x0300_0003)] {
            let meta = store
                .view(|tx| tx.get_offset_meta(PartitionChunkOffset::from(offset)))?
                .unwrap_or_else(|| panic!("missing entropy row {offset}"));
            assert_eq!(meta.crc, Some(crc));
            assert!(meta.proof.is_none());
            assert!(
                store
                    .view(|tx| tx.placement_at(PartitionChunkOffset::from(offset)))?
                    .is_none()
            );
        }
        assert_eq!(super::read_entropy_range(&store, &args, 0)?, 2);
        assert_eq!(super::read_recall_rows(&store, &args, 0)?, 2);
        store.settle_files()?;
        let size = super::entropy_fill_size(&store, &args)?;
        assert_eq!(size.data_rows, 2);
        assert_eq!(size.entropy_rows, 2);
        assert!(size.sst_bytes > 0);
        drop(store);
        super::read_phase(&path, "rocks", &args, &|dir| {
            SubmoduleIndex::open_rocks_bench(dir, args.rocks, true)
        })?;
        Ok(())
    }
}
