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

use std::fs;
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
}

fn main() -> eyre::Result<()> {
    let args = parse_args()?;
    refuse_large_worst_case(&args)?;
    prepare_dir(&args.dir)?;
    println!(
        "chunks={} tx_chunks={} batch={} path_bytes={} allocator=system jemalloc=off",
        args.chunks, args.tx_chunks, args.batch, args.path_bytes
    );
    if args.worst_case {
        let raw = u64::from(args.chunks) * DATA_PROOF_BYTES as u64;
        let full = PARTITION_CHUNKS * DATA_PROOF_BYTES as u64;
        println!(
            "mode=worst-case data_proof_bytes={DATA_PROOF_BYTES} data_proof_branches={DATA_PROOF_LAYERS} tx_proof_bytes={TX_PROOF_BYTES} tx_group_chunks={MAX_DATA_TX_CHUNKS} raw_data_proof_bytes={raw} full_partition_chunks={PARTITION_CHUNKS} full_partition_raw_data_proof_bytes={full}"
        );
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
        "engine=rocks sync=wal_fsync compression=lz4 block_bytes=65536 blob_min_bytes={BLOB_MIN_BYTES} block_cache_bytes={BLOCK_CACHE_BYTES}"
    );
    measure(&rocks_dir, &args, |path| SubmoduleIndex::open_rocks(path))?;
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
    let read_ms = timed(|| read_sample(&store, args))?;
    println!(
        "  index_ms={index_ms} data_path_ms={data_path_ms} settle_ms={settle_ms} open_ms={open_ms} read_ms={read_ms}"
    );
    println!("  logical_bytes={logical} allocated_bytes={allocated}");
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

fn read_sample(store: &SubmoduleIndex, args: &Args) -> eyre::Result<()> {
    let last = args.chunks.min(4096) - 1;
    let rows = store.view(|tx| {
        tx.path_hashes_in_inclusive_range(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(last),
        )
    })?;
    let expected = usize::try_from(last).map(|n| n + 1)?;
    eyre::ensure!(rows.len() == expected, "range read returned {}", rows.len());
    let sample = hash_at(0x2000_0000);
    let path = store.view(|tx| tx.get_full_data_path(sample))?;
    eyre::ensure!(
        path.as_deref() == Some(data_path_bytes(args, 0).as_slice()),
        "data path readback mismatch"
    );
    Ok(())
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
    // Worst-case uses the production submodule map (2 TiB). The filled mode
    // stays inside 8 GiB so the small comparison run does not reserve more.
    let max_map = if worst_case {
        2 * TERABYTE as u64
    } else {
        8 << 30
    };
    let cap = estimate.clamp(1 << 30, max_map);
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

fn refuse_large_worst_case(args: &Args) -> eyre::Result<()> {
    if !args.worst_case {
        return Ok(());
    }
    let raw = u64::from(args.chunks) * DATA_PROOF_BYTES as u64;
    let allowed = std::env::var("IRYS_INDEX_BENCH_LARGE").ok().as_deref() == Some("1");
    if raw > LARGE_RAW_BYTES && !allowed {
        eyre::bail!(
            "worst-case raw data proofs are {raw} bytes. Set IRYS_INDEX_BENCH_LARGE=1 to write them. A full partition is --chunks {PARTITION_CHUNKS} ({full} raw bytes per engine).",
            full = PARTITION_CHUNKS * DATA_PROOF_BYTES as u64
        );
    }
    Ok(())
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
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "--dir" => dir = Some(PathBuf::from(required(&mut it, &flag)?)),
            "--chunks" => chunks = required(&mut it, &flag)?.parse()?,
            "--batch" => batch = required(&mut it, &flag)?.parse()?,
            "--tx-chunks" => tx_chunks = Some(required(&mut it, &flag)?.parse()?),
            "--path-bytes" => path_bytes = Some(required(&mut it, &flag)?.parse()?),
            "--worst-case" => worst_case = true,
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
    eyre::ensure!(chunks > 0, "--chunks must be > 0");
    eyre::ensure!(batch > 0, "--batch must be > 0");
    eyre::ensure!(tx_chunks > 0, "--tx-chunks must be > 0");
    eyre::ensure!(path_bytes > 0, "--path-bytes must be > 0");
    Ok(Args {
        dir,
        chunks,
        batch,
        tx_chunks,
        path_bytes,
        worst_case,
    })
}

fn required(it: &mut impl Iterator<Item = String>, flag: &str) -> eyre::Result<String> {
    it.next().ok_or_else(|| eyre::eyre!("{flag} needs a value"))
}

fn print_help() {
    eprintln!(
        "submodule-index-bench --dir <path/index-bench> [--chunks N] [--batch N] [--tx-chunks N] [--path-bytes N] [--worst-case]\n\
Writes the same synthetic submodule index on durable MDBX and on RocksDB.\n\
The directory must be empty and must contain an index-bench path component.\n\
--worst-case stores a {DATA_PROOF_BYTES}-byte max data proof on every chunk \
(a max transaction is {MAX_DATA_TX_CHUNKS} chunks). \
A full partition is --chunks {PARTITION_CHUNKS} and requires IRYS_INDEX_BENCH_LARGE=1."
    );
}

#[cfg(test)]
mod tests {
    use super::{
        DATA_PROOF_BYTES, DATA_PROOF_LAYERS, HASH_SIZE, LEAF_SIZE, MAX_DATA_TX_CHUNKS, NOTE_SIZE,
        PARTITION_CHUNKS, TX_PROOF_BYTES, TX_PROOF_LAYERS, pairing_layers, proof_bytes,
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
}
