//! Write one synthetic submodule index on MDBX and on RocksDB.
//!
//! Prints wall time and directory bytes. The path must contain an
//! `index-bench` component and must be empty. This binary does not open
//! a live node database.
//!
//! ```text
//! cargo run -p irys-database --features rocksdb --bin submodule-index-bench -- \
//!   --dir /tmp/index-bench --chunks 4096
//! ```
//!
//! `--worst-case` stores a max-size data proof on every chunk. A full
//! partition is `--chunks 75534400` and needs `IRYS_INDEX_BENCH_LARGE=1`.
//! `--mid` writes 1_000_000 chunks of 4096-byte paths, past an HDD cache.
//! It needs the same variable. `--profile filled|mid|worst` selects the
//! same workloads. `--rocks <preset>` changes one RocksDB setting.
//! `--engine rocks` skips MDBX.
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
    ChunkPathHashes, DataRootInfo, PendingBodyMigration, TxLeafBinding,
};
use irys_database::submodule::{RocksTuning, SubmoduleIndex, SubmoduleStore as _};
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
        }
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
}

fn main() -> eyre::Result<()> {
    let args = parse_args()?;
    refuse_large_payload(&args)?;
    prepare_dir(&args.dir)?;
    println!(
        "chunks={} tx_chunks={} batch={} path_bytes={} reads={} range_len={} profile={} engines={} dbs={} group_commit={} allocator=system jemalloc=off",
        args.chunks,
        args.tx_chunks,
        args.batch,
        args.path_bytes,
        args.reads,
        args.range_len,
        args.workload.name(),
        args.engines.name(),
        args.dbs,
        u8::from(args.group_commit),
    );
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
    } else {
        println!("mode=filled");
    }

    if args.engines.includes_mdbx() {
        let mdbx_dir = args.dir.join("mdbx");
        fs::create_dir_all(&mdbx_dir)?;
        let map_bytes = mdbx_map_bytes(args.chunks, args.path_bytes, args.workload.is_worst());
        let mdbx_commits = commits_for(args.workload.is_mid(), args.batch, true, args.group_commit);
        println!(
            "engine=mdbx sync=durable geometry_max_bytes={map_bytes} txs_per_commit={} path_batch={}",
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
        let rocks_commits =
            commits_for(args.workload.is_mid(), args.batch, false, args.group_commit);
        let tuning = args.rocks;
        println!(
            "engine=rocks sync=wal_fsync {} max_open_files=-1 file_opening_threads=16 txs_per_commit={} path_batch={}",
            tuning.describe(),
            rocks_commits.txs_per_commit,
            rocks_commits.path_batch
        );
        measure(&rocks_dir, "rocks", &args, rocks_commits, |path| {
            SubmoduleIndex::open_rocks_with(path, tuning)
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
    let cache = args.rocks.block_cache_bytes as u64;
    let keys = u64::from(args.chunks);
    let bloom = bloom_bytes(keys);
    match args.workload {
        Workload::MemDbs => {
            let serving = cache.saturating_mul(u64::from(args.dbs));
            format!(
                "rows_per_db={keys} block_cache_bytes={cache} serving_cache_bytes={serving} bloom_bytes_per_db={bloom}"
            )
        }
        Workload::IndexSpan => {
            format!("one_update=1 rows={keys} bloom_bytes={bloom} block_cache_bytes={cache}")
        }
        Workload::OffsetRows => {
            let fits = if bloom <= cache { "yes" } else { "no" };
            format!(
                "rows={keys} batch={} bloom_bytes={bloom} block_cache_bytes={cache} bloom_fits={fits}",
                args.batch
            )
        }
        Workload::Filled | Workload::Mid | Workload::Worst => String::new(),
    }
}

fn run_memory(args: &Args) -> eyre::Result<()> {
    match args.workload {
        Workload::MemDbs => run_mem_dbs(args),
        Workload::IndexSpan => run_index_span(args),
        Workload::OffsetRows => run_offset_rows(args),
        Workload::Filled | Workload::Mid | Workload::Worst => {
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
        let store = SubmoduleIndex::open_rocks_with(&dir, args.rocks)?;
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
    let store = SubmoduleIndex::open_rocks_with(&dir, args.rocks)?;
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

/// Offset rows and no data-path blobs. The cold read is one family.
fn run_offset_rows(args: &Args) -> eyre::Result<()> {
    let watch = MemWatch::start();
    let dir = args.dir.join("rocks");
    fs::create_dir_all(&dir)?;
    note_mem("rocks", "before_open", &watch);
    let store = SubmoduleIndex::open_rocks_with(&dir, args.rocks)?;
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
        let open = |path: &Path| SubmoduleIndex::open_rocks_with(path, args.rocks);
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
    let settle_ms = timed(|| store.settle_files())?;
    note_mem(engine, "after_settle", &watch);
    drop(store);
    note_mem(engine, "after_write_drop", &watch);

    let (logical, allocated) = dir_usage(dir)?;
    let open_started = Instant::now();
    let store = open(dir)?;
    let open_ms = open_started.elapsed().as_millis();
    println!(
        "  index_ms={index_ms} data_path_ms={data_path_ms} settle_ms={settle_ms} open_ms={open_ms}"
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
    while offset < args.chunks {
        let mut group = Vec::new();
        for _ in 0..txs_per_commit {
            if offset >= args.chunks {
                break;
            }
            let count = args.tx_chunks.min(args.chunks - offset);
            let start = PartitionChunkOffset::from(offset);
            let end = PartitionChunkOffset::from(offset + count - 1);
            group.push(IndexTx {
                start,
                end,
                tx_hash: hash_at(u64::from(offset)),
                data_root: hash_at(u64::from(offset) + 0x1000_0000),
                tx_path: tx_path_bytes(args, offset),
                start_offset: RelativeChunkOffset(i32::try_from(offset)?),
                data_size: indexed_data_size(args, count),
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
    let mut offset = 0_u32;
    while offset < args.chunks {
        let count = path_batch.min(args.chunks - offset);
        let mut updates = Vec::with_capacity(count as usize);
        for step in 0..count {
            let at = offset + step;
            updates.push((
                PartitionChunkOffset::from(at),
                hash_at(u64::from(at) + 0x2000_0000),
                data_path_bytes(args, at),
            ));
        }
        store.update(|tx| tx.write_data_path_updates(updates))?;
        offset += count;
    }
    Ok(())
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
    // Scattered chunk reads (recall, single-chunk serve).
    let mut random = |store: &SubmoduleIndex, i: u32| {
        read_one_data_path(store, args, sample_offset(i, args.chunks))
    };
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
        |store, i| read_one_data_path(store, args, seq_offset(i, args.chunks)),
    )?;
    time_reopen(dir, disk.as_ref(), "serve", args.reads, open, |store, i| {
        let at = sample_offset(i.wrapping_add(1), args.chunks);
        let expected = hash_at(u64::from(at) + 0x2000_0000);
        store.view(|tx| {
            let hashes = tx
                .get_path_hashes_by_offset(PartitionChunkOffset::from(at))?
                .ok_or_else(|| eyre::eyre!("missing offset row {at}"))?;
            let hash = hashes
                .data_path_hash
                .ok_or_else(|| eyre::eyre!("missing data path hash at {at}"))?;
            eyre::ensure!(hash == expected, "data path hash mismatch at {at}");
            let path = tx
                .get_data_path_by_offset(PartitionChunkOffset::from(at))?
                .ok_or_else(|| eyre::eyre!("missing data path body at {at}"))?;
            eyre::ensure!(path.len() == args.path_bytes, "serve path len at {at}");
            Ok(())
        })
    })?;
    let window = args.range_len.min(args.chunks);
    time_reopen(dir, disk.as_ref(), "range", args.reads, open, |store, i| {
        let start = sample_offset(i.wrapping_add(2), args.chunks - window + 1);
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
    time_reopen(
        dir,
        disk.as_ref(),
        "rmw_path",
        args.reads,
        open,
        |store, i| {
            let at = sample_offset(i.wrapping_add(3), args.chunks);
            // One changed byte. The same bytes let MDBX skip the flush while
            // RocksDB still syncs the WAL.
            let path = rewritten_data_path(args, at);
            store.update(|tx| {
                tx.write_data_path_updates(vec![(
                    PartitionChunkOffset::from(at),
                    hash_at(u64::from(at) + 0x2000_0000),
                    path,
                )])
            })
        },
    )?;
    time_reopen(
        dir,
        disk.as_ref(),
        "rmw_root",
        args.reads,
        open,
        |store, i| {
            let at = sample_offset(i.wrapping_add(4), args.chunks);
            let tx_start = at / args.tx_chunks * args.tx_chunks;
            let root = hash_at(u64::from(tx_start) + 0x1000_0000);
            // Distinct from the indexed row and from every other sample, so
            // the read-before-write always appends and both engines commit.
            let info = DataRootInfo {
                start_offset: RelativeChunkOffset(-1),
                data_size: indexed_data_size(args, 1).saturating_add(u64::from(i) + 1),
            };
            store.update(|tx| tx.add_data_root_info(root, &info))
        },
    )?;
    let store = open(dir)?;
    let kept = store.view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(0)))?;
    let kept = kept.ok_or_else(|| eyre::eyre!("offset 0 missing after rmw"))?;
    eyre::ensure!(
        kept.tx_path_hash == Some(hash_at(0)),
        "rmw_path dropped the tx path hash"
    );
    Ok(())
}

fn read_one_data_path(store: &SubmoduleIndex, args: &Args, at: u32) -> eyre::Result<()> {
    let path = store.view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(at)))?;
    let path = path.ok_or_else(|| eyre::eyre!("missing data path at {at}"))?;
    eyre::ensure!(path.len() == args.path_bytes, "data path len at {at}");
    Ok(())
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
            let meta = fs::symlink_metadata(&path)?;
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                for entry in fs::read_dir(&path)? {
                    stack.push(entry?.path());
                }
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let file = File::open(&path)?;
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

fn timed(f: impl FnOnce() -> eyre::Result<()>) -> eyre::Result<u128> {
    let started = Instant::now();
    f()?;
    Ok(started.elapsed().as_millis())
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
    if !args.workload.is_worst() {
        return fill(args.path_bytes, u64::from(offset));
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
    if !args.workload.is_worst() {
        return fill(args.path_bytes, u64::from(tx_start));
    }
    let ordinal = u64::from(tx_start) / MAX_DATA_TX_CHUNKS;
    let leaf_end = ordinal
        .saturating_add(1)
        .saturating_mul(MAX_DATA_TX_CHUNKS)
        .saturating_mul(CHUNK_SIZE);
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
    let mut state = seed | 1;
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
    let raw = u64::from(args.chunks)
        * if args.workload.is_worst() {
            DATA_PROOF_BYTES as u64
        } else {
            args.path_bytes as u64
        };
    let worst_over = args.workload.is_worst() && raw > LARGE_RAW_BYTES;
    let memory_over = match args.workload {
        // 8 databases of 1_000_000 rows is the default and stays under this.
        Workload::MemDbs => u64::from(args.chunks).saturating_mul(u64::from(args.dbs)) > 8_000_000,
        // 1_000_000 offsets is one step. A max transaction is past it.
        Workload::IndexSpan => args.chunks > 1_000_000,
        // A bloom that still fits in the 64 MiB cache is under this.
        Workload::OffsetRows => args.chunks > 2_000_000,
        Workload::Filled | Workload::Mid | Workload::Worst => false,
    };
    if !args.workload.is_mid() && !worst_over && !memory_over {
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
    eyre::bail!(
        "worst-case raw data proofs are {raw} bytes. Set IRYS_INDEX_BENCH_LARGE=1 to write them. A full partition is --chunks {PARTITION_CHUNKS} ({full} raw bytes per engine).",
        full = PARTITION_CHUNKS * DATA_PROOF_BYTES as u64
    );
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

fn fill(len: usize, seed: u64) -> Vec<u8> {
    let mut out = vec![0_u8; len];
    let mut state = seed | 1;
    for byte in &mut out {
        state = state.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        *byte = (state >> 33) as u8;
    }
    out
}

fn prepare_dir(path: &Path) -> eyre::Result<()> {
    let marked = path
        .components()
        .any(|component| matches!(component, Component::Normal(name) if name == "index-bench"));
    eyre::ensure!(
        marked,
        "refusing {}: path needs an index-bench component",
        path.display()
    );
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
            "--group-commit" => group_commit = true,
            other => eyre::bail!("unknown argument {other}"),
        }
    }
    let Some(dir) = dir else {
        print_help();
        eyre::bail!("--dir is required");
    };
    let workload = resolve_workload(profile, mid, worst_case)?;
    if group_commit && workload.is_memory() {
        eyre::bail!(
            "--group-commit does not apply to --profile {}",
            workload.name()
        );
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
    if worst_case && path_bytes.is_some() {
        eyre::bail!("--path-bytes does not apply to --worst-case");
    }
    if worst_case && tx_chunks.is_some() {
        eyre::bail!("--tx-chunks does not apply to --worst-case");
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
    if mid {
        chunks = MID_CHUNKS;
    }
    let path_bytes = if worst_case {
        DATA_PROOF_BYTES
    } else {
        path_bytes.unwrap_or(4096)
    };
    let tx_chunks = if worst_case {
        u32::try_from(MAX_DATA_TX_CHUNKS)?
    } else {
        tx_chunks.unwrap_or(32)
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
    })
}

fn parse_profile(value: &str) -> eyre::Result<Workload> {
    match value {
        "filled" => Ok(Workload::Filled),
        "mid" => Ok(Workload::Mid),
        "worst" => Ok(Workload::Worst),
        "mem-dbs" => Ok(Workload::MemDbs),
        "index-span" => Ok(Workload::IndexSpan),
        "offset-rows" => Ok(Workload::OffsetRows),
        other => eyre::bail!(
            "unknown profile {other}; use filled, mid, worst, mem-dbs, index-span, or offset-rows"
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

fn resolve_workload(profile: Option<Workload>, mid: bool, worst: bool) -> eyre::Result<Workload> {
    match (profile, mid, worst) {
        (None, false, false) => Ok(Workload::Filled),
        (None, true, false) => Ok(Workload::Mid),
        (None, false, true) => Ok(Workload::Worst),
        (Some(profile), false, false) => Ok(profile),
        (Some(Workload::Mid), true, false) => Ok(Workload::Mid),
        (Some(Workload::Worst), false, true) => Ok(Workload::Worst),
        _ => eyre::bail!("--profile, --mid, and --worst-case disagree"),
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
        "submodule-index-bench --dir <path/index-bench> [--chunks N] [--batch N] [--tx-chunks N] [--path-bytes N] [--reads N] [--range-len N] [--rocks-block-cache N] [--profile filled|mid|worst] [--engine both|mdbx|rocks] [--rocks <preset>] [--mid] [--group-commit] [--worst-case] [--list-rocks]\n\
Writes the same synthetic submodule index on durable MDBX and on RocksDB, then times cold lookups.\n\
The directory must be empty and must contain an index-bench path component.\n\
--profile selects the workload. filled is the default. mid is --mid. worst is --worst-case and still takes --chunks.\n\
mem-dbs, index-span, and offset-rows measure RocksDB RSS. They run rocks only.\n\
mem-dbs keeps --dbs databases open (default 8) after writing --chunks offset rows in each (default 1000000) and scanning them.\n\
index-span writes --chunks offsets in one update. A max transaction is --chunks 20971520.\n\
offset-rows writes --chunks offset rows and no data paths, then does cold point reads. A full partition is --chunks 75534400.\n\
--engine both is the default. A preset sweep uses --engine rocks so MDBX is not repeated.\n\
--rocks selects one RocksDB preset. The default is baseline, which is what production open uses. \
Presets: {presets}. Each preset changes one setting. \
--rocks-block-cache overrides that preset's cache. A new block size needs an empty directory.\n\
--list-rocks prints the presets and exits.\n\
--reads N (default 1024) is the sample count for each shape: data_path_random, data_path_random_open, data_path_seq, serve, range, rmw_path, rmw_root.\n\
data_path_random is scattered get_data_path_by_offset. data_path_random_open repeats it with the engine left open. data_path_seq is the same call on offsets 0, 1, 2, ...\n\
Each read line also prints head_p50_us, head_p99_us, and head_max_us for the first 32 samples, and the same for the tail.\n\
RocksDB prints rocks_bg before and after each shape: running compactions, running flushes, pending flags, and no_file_opens.\n\
The RocksDB engine line shows max_open_files=-1 file_opening_threads=16. That preload is bench-only. Production open stays at 512.\n\
rmw_path rewrites the stored path with one byte changed. rmw_root appends a distinct placement on every sample. Both commit.\n\
Each shape calls posix_fadvise(DONTNEED) on the engine files. It does not drop the host page cache. data_path_random_open keeps the Rocks block cache.\n\
Each engine prints mem lines. rss_kb is resident. anon_kb is heap. file_kb is mapped file pages. size_kb is virtual.\n\
--group-commit makes both engines commit {MID_MDBX_TXS_PER_COMMIT} transactions and {MID_MDBX_PATH_BATCH} data-path rows (or --batch, when that is larger). Without it, only --mid MDBX uses those sizes. RocksDB stays at one transaction and --batch rows.\n\
--mid writes {MID_CHUNKS} chunks of 4096-byte paths (about 4 GiB raw per engine). \
It needs IRYS_INDEX_BENCH_LARGE=1. Do not combine it with --chunks, --path-bytes, or --worst-case.\n\
--worst-case stores a {DATA_PROOF_BYTES}-byte max data proof on every chunk \
(a max transaction is {MAX_DATA_TX_CHUNKS} chunks). \
A full partition is --chunks {PARTITION_CHUNKS} and requires IRYS_INDEX_BENCH_LARGE=1."
    );
}

#[cfg(test)]
mod tests {
    use irys_database::submodule::BLOCK_CACHE_BYTES;

    use super::{
        Args, DATA_PROOF_BYTES, DATA_PROOF_LAYERS, Engines, FILLED_MAP_CEILING, HASH_SIZE,
        HEAD_SAMPLES, LEAF_SIZE, MAX_DATA_TX_CHUNKS, MID_CHUNKS, MID_MDBX_PATH_BATCH,
        MID_MDBX_TXS_PER_COMMIT, NOTE_SIZE, PARTITION_CHUNKS, TX_PROOF_BYTES, TX_PROOF_LAYERS,
        Workload, bloom_bytes, commits_for, data_path_bytes, head_tail, mdbx_map_bytes,
        pairing_layers, parse_arg_list, parse_diskstats, parse_mount_line, parse_proc_status,
        percentile, proof_bytes, ranks, rewritten_data_path, sample_offset, seq_offset,
        shaped_proof,
    };

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
        assert_eq!(cache.engines, Engines::Both);

        let defaults = parse(&["--dir", "index-bench"]).unwrap();
        assert_eq!(defaults.workload, Workload::Filled);
        assert_eq!(defaults.engines, Engines::Both);
        assert_eq!(defaults.rocks.name, "baseline");
        assert_eq!(defaults.rocks.block_cache_bytes, BLOCK_CACHE_BYTES);
        assert_eq!(defaults.chunks, 4096);
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
}
