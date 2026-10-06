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
//! It needs the same variable.
//!
//! After the writes, each engine drops its file cache with `posix_fadvise`
//! and times random lookups. This does not call `drop_caches`.

use std::fs::{self, File};
use std::io::Error;
use std::os::unix::io::AsRawFd as _;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use irys_database::IrysDatabaseArgs as _;
use irys_database::submodule::tables::{DataRootInfo, PendingBodyMigration, TxLeafBinding};
use irys_database::submodule::{
    BLOB_MIN_BYTES, BLOCK_CACHE_BYTES, SubmoduleIndex, SubmoduleStore as _,
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

struct Args {
    dir: PathBuf,
    chunks: u32,
    batch: u32,
    /// Offsets covered by one transaction commit.
    tx_chunks: u32,
    path_bytes: usize,
    /// Every chunk stores a max-depth data proof. Transactions are max size.
    worst_case: bool,
    /// 1_000_000 chunks of the default 4096-byte path. Larger than an HDD cache.
    mid: bool,
    /// Random samples of each read shape. Zero skips the read phase.
    reads: u32,
    /// Inclusive offset window for one range lookup.
    range_len: u32,
    /// RocksDB block cache. MDBX ignores this.
    block_cache: usize,
}

fn main() -> eyre::Result<()> {
    let args = parse_args()?;
    refuse_large_payload(&args)?;
    prepare_dir(&args.dir)?;
    println!(
        "chunks={} tx_chunks={} batch={} path_bytes={} reads={} range_len={} allocator=system jemalloc=off",
        args.chunks, args.tx_chunks, args.batch, args.path_bytes, args.reads, args.range_len
    );
    if args.worst_case {
        let raw = u64::from(args.chunks) * DATA_PROOF_BYTES as u64;
        let full = PARTITION_CHUNKS * DATA_PROOF_BYTES as u64;
        println!(
            "mode=worst-case data_proof_bytes={DATA_PROOF_BYTES} data_proof_branches={DATA_PROOF_LAYERS} tx_proof_bytes={TX_PROOF_BYTES} tx_group_chunks={MAX_DATA_TX_CHUNKS} raw_data_proof_bytes={raw} full_partition_chunks={PARTITION_CHUNKS} full_partition_raw_data_proof_bytes={full}"
        );
    } else if args.mid {
        let raw = u64::from(args.chunks) * args.path_bytes as u64;
        println!("mode=mid raw_data_path_bytes={raw}");
    } else {
        println!("mode=filled");
    }

    let mdbx_dir = args.dir.join("mdbx");
    fs::create_dir_all(&mdbx_dir)?;
    let map_bytes = mdbx_map_bytes(args.chunks, args.path_bytes, args.worst_case);
    println!("engine=mdbx sync=durable geometry_max_bytes={map_bytes}");
    measure(&mdbx_dir, &args, |path| {
        SubmoduleIndex::open_mdbx(
            path,
            DatabaseArguments::irys_default(DbSyncMode::Durable)?
                .with_geometry_max_size(Some(map_bytes)),
        )
    })?;

    let rocks_dir = args.dir.join("rocks");
    fs::create_dir_all(&rocks_dir)?;
    println!(
        "engine=rocks sync=wal_fsync compression=lz4 block_bytes=65536 blob_min_bytes={BLOB_MIN_BYTES} block_cache_bytes={}",
        args.block_cache
    );
    let cache = args.block_cache;
    measure(&rocks_dir, &args, |path| {
        SubmoduleIndex::open_rocks_with_block_cache(path, cache)
    })?;
    Ok(())
}

fn measure(
    dir: &Path,
    args: &Args,
    open: impl Fn(&Path) -> eyre::Result<SubmoduleIndex>,
) -> eyre::Result<()> {
    let store = open(dir)?;
    let index_ms = timed(|| write_tx_index(&store, args))?;
    let data_path_ms = timed(|| write_data_paths(&store, args))?;
    let settle_ms = timed(|| store.settle_files())?;
    drop(store);

    let (logical, allocated) = dir_usage(dir)?;
    let open_started = Instant::now();
    let store = open(dir)?;
    let open_ms = open_started.elapsed().as_millis();
    println!(
        "  index_ms={index_ms} data_path_ms={data_path_ms} settle_ms={settle_ms} open_ms={open_ms}"
    );
    println!("  logical_bytes={logical} allocated_bytes={allocated}");
    drop(store);
    read_phase(dir, args, &open)?;
    Ok(())
}

fn write_tx_index(store: &SubmoduleIndex, args: &Args) -> eyre::Result<()> {
    let mut offset = 0_u32;
    while offset < args.chunks {
        let count = args.tx_chunks.min(args.chunks - offset);
        let start = PartitionChunkOffset::from(offset);
        let end = PartitionChunkOffset::from(offset + count - 1);
        let tx_hash = hash_at(u64::from(offset));
        let data_root = hash_at(u64::from(offset) + 0x1000_0000);
        let tx_path = tx_path_bytes(args, offset);
        let start_offset = RelativeChunkOffset(i32::try_from(offset)?);
        let data_size = indexed_data_size(args, count);
        store.update(|tx| {
            tx.add_full_tx_path(tx_hash, tx_path)?;
            tx.add_tx_leaf_binding(
                tx_hash,
                &TxLeafBinding {
                    data_root,
                    prefix_hash: H256::zero(),
                },
            )?;
            tx.add_tx_path_hash_to_offset_range(start, end, Some(tx_hash))?;
            tx.add_data_root_info(
                data_root,
                &DataRootInfo {
                    start_offset,
                    data_size,
                },
            )?;
            tx.add_pending_body_migration(
                start,
                &PendingBodyMigration {
                    data_root,
                    data_size,
                    start_offset,
                    block_height: 1,
                    attempts: 0,
                },
            )?;
            Ok(())
        })?;
        offset += count;
    }
    Ok(())
}

fn write_data_paths(store: &SubmoduleIndex, args: &Args) -> eyre::Result<()> {
    let mut offset = 0_u32;
    while offset < args.chunks {
        let count = args.batch.min(args.chunks - offset);
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
fn read_phase(
    dir: &Path,
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
    time_shape(
        dir,
        disk.as_ref(),
        "data_path_random",
        args.reads,
        open,
        |store, i| read_one_data_path(store, args, sample_offset(i, args.chunks)),
    )?;
    // Consecutive chunk reads (a span serve). Same call, offsets 0, 1, 2, ...
    time_shape(
        dir,
        disk.as_ref(),
        "data_path_seq",
        args.reads,
        open,
        |store, i| read_one_data_path(store, args, seq_offset(i, args.chunks)),
    )?;
    time_shape(dir, disk.as_ref(), "serve", args.reads, open, |store, i| {
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
                .get_full_data_path(hash)?
                .ok_or_else(|| eyre::eyre!("missing data path body at {at}"))?;
            eyre::ensure!(path.len() == args.path_bytes, "serve path len at {at}");
            Ok(())
        })
    })?;
    let window = args.range_len.min(args.chunks);
    time_shape(dir, disk.as_ref(), "range", args.reads, open, |store, i| {
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
    time_shape(
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
    time_shape(
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

fn time_shape(
    dir: &Path,
    disk: Option<&DiskId>,
    shape: &str,
    reads: u32,
    open: &impl Fn(&Path) -> eyre::Result<SubmoduleIndex>,
    mut one: impl FnMut(&SubmoduleIndex, u32) -> eyre::Result<()>,
) -> eyre::Result<()> {
    // Drop pages left by the previous shape, then open a new cache.
    evict_cache(dir)?;
    let store = open(dir)?;
    // Open reads metadata. Drop those pages. Rocks keeps filters it copied.
    let files = evict_cache(dir)?;
    let before = disk_counters(disk);
    let mut samples = Vec::with_capacity(usize::try_from(reads)?);
    for i in 0..reads {
        let started = Instant::now();
        one(&store, i)?;
        samples.push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
    }
    drop(store);
    let after = disk_counters(disk);
    samples.sort_unstable();
    let n = samples.len() as u64;
    let p50 = percentile(&samples, 50);
    let p99 = percentile(&samples, 99);
    let max = samples.last().copied().unwrap_or(0);
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
            println!(
                "  read shape={shape} n={n} p50_us={p50} p99_us={p99} max_us={max} evict_files={files} disk_reads={disk_reads} disk_read_bytes={disk_read_bytes} disk_writes={disk_writes} disk_write_bytes={disk_write_bytes} per_lookup_read_bytes={}",
                disk_read_bytes / n
            );
        }
        _ => {
            println!(
                "  read shape={shape} n={n} p50_us={p50} p99_us={p99} max_us={max} evict_files={files} disk=none"
            );
        }
    }
    Ok(())
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
    let chunks = if args.worst_case {
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
    if !args.worst_case {
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
    if !args.worst_case {
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
        * if args.worst_case {
            DATA_PROOF_BYTES as u64
        } else {
            args.path_bytes as u64
        };
    let worst_over = args.worst_case && raw > LARGE_RAW_BYTES;
    if !args.mid && !worst_over {
        return Ok(());
    }
    let allowed = std::env::var("IRYS_INDEX_BENCH_LARGE").ok().as_deref() == Some("1");
    if allowed {
        return Ok(());
    }
    if args.mid {
        eyre::bail!(
            "--mid writes {raw} raw data-path bytes per engine (about 9 GiB on disk, two engines). Set IRYS_INDEX_BENCH_LARGE=1 to run it."
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
    let mut dir = None;
    let mut chunks = 4096_u32;
    let mut batch = 512_u32;
    let mut tx_chunks: Option<u32> = None;
    let mut path_bytes: Option<usize> = None;
    let mut worst_case = false;
    let mut mid = false;
    let mut chunks_set = false;
    let mut reads = 1024_u32;
    let mut range_len = 32_u32;
    let mut block_cache: Option<usize> = None;
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--help" | "-h" => {
                print_help();
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
            "--worst-case" => worst_case = true,
            "--mid" => mid = true,
            other => eyre::bail!("unknown argument {other}"),
        }
    }
    let Some(dir) = dir else {
        print_help();
        eyre::bail!("--dir is required");
    };
    if worst_case && path_bytes.is_some() {
        eyre::bail!("--path-bytes does not apply to --worst-case");
    }
    if worst_case && tx_chunks.is_some() {
        eyre::bail!("--tx-chunks does not apply to --worst-case");
    }
    if mid && worst_case {
        eyre::bail!("--mid does not apply to --worst-case");
    }
    if mid && path_bytes.is_some() {
        eyre::bail!("--path-bytes does not apply to --mid");
    }
    if mid && chunks_set {
        eyre::bail!("--chunks does not apply to --mid");
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
    let block_cache = block_cache.unwrap_or(BLOCK_CACHE_BYTES);
    eyre::ensure!(chunks > 0, "--chunks must be > 0");
    eyre::ensure!(batch > 0, "--batch must be > 0");
    eyre::ensure!(tx_chunks > 0, "--tx-chunks must be > 0");
    eyre::ensure!(path_bytes > 0, "--path-bytes must be > 0");
    eyre::ensure!(range_len > 0, "--range-len must be > 0");
    eyre::ensure!(block_cache > 0, "--rocks-block-cache must be > 0");
    Ok(Args {
        dir,
        chunks,
        batch,
        tx_chunks,
        path_bytes,
        worst_case,
        mid,
        reads,
        range_len,
        block_cache,
    })
}

fn required(it: &mut impl Iterator<Item = String>, flag: &str) -> eyre::Result<String> {
    it.next().ok_or_else(|| eyre::eyre!("{flag} needs a value"))
}

fn print_help() {
    eprintln!(
        "submodule-index-bench --dir <path/index-bench> [--chunks N] [--batch N] [--tx-chunks N] [--path-bytes N] [--reads N] [--range-len N] [--rocks-block-cache N] [--mid] [--worst-case]\n\
Writes the same synthetic submodule index on durable MDBX and on RocksDB, then times cold lookups.\n\
The directory must be empty and must contain an index-bench path component.\n\
--reads N (default 1024) is the sample count for each shape: data_path_random, data_path_seq, serve, range, rmw_path, rmw_root.\n\
data_path_random is scattered get_data_path_by_offset. data_path_seq is the same call on offsets 0, 1, 2, ...\n\
rmw_path rewrites the stored path with one byte changed. rmw_root appends a distinct placement on every sample. Both commit.\n\
Each shape calls posix_fadvise(DONTNEED) on the engine files. It does not drop the host page cache.\n\
--mid writes {MID_CHUNKS} chunks of 4096-byte paths (about 4 GiB raw per engine). \
It needs IRYS_INDEX_BENCH_LARGE=1. Do not combine it with --chunks, --path-bytes, or --worst-case.\n\
--worst-case stores a {DATA_PROOF_BYTES}-byte max data proof on every chunk \
(a max transaction is {MAX_DATA_TX_CHUNKS} chunks). \
A full partition is --chunks {PARTITION_CHUNKS} and requires IRYS_INDEX_BENCH_LARGE=1."
    );
}

#[cfg(test)]
mod tests {
    use super::{
        Args, DATA_PROOF_BYTES, DATA_PROOF_LAYERS, FILLED_MAP_CEILING, HASH_SIZE, LEAF_SIZE,
        MAX_DATA_TX_CHUNKS, MID_CHUNKS, NOTE_SIZE, PARTITION_CHUNKS, TX_PROOF_BYTES,
        TX_PROOF_LAYERS, data_path_bytes, mdbx_map_bytes, pairing_layers, parse_diskstats,
        parse_mount_line, percentile, proof_bytes, rewritten_data_path, sample_offset, seq_offset,
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
            worst_case: false,
            mid: false,
            reads: 1,
            range_len: 4,
            block_cache: 1024,
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

    #[test]
    fn percentiles_use_nearest_rank() {
        let samples = [10, 20, 30, 40];
        assert_eq!(percentile(&samples, 50), 20);
        assert_eq!(percentile(&samples, 99), 40);
        assert_eq!(percentile(&[], 50), 0);
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
