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

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use irys_database::IrysDatabaseArgs as _;
use irys_database::submodule::tables::{DataRootInfo, PendingBodyMigration, TxLeafBinding};
use irys_database::submodule::{
    BLOB_MIN_BYTES, BLOCK_CACHE_BYTES, SubmoduleIndex, SubmoduleStore as _,
};
use irys_types::{DbSyncMode, H256, PartitionChunkOffset, RelativeChunkOffset};
use reth_db::mdbx::DatabaseArguments;

struct Args {
    dir: PathBuf,
    chunks: u32,
    batch: u32,
    tx_chunks: u32,
    path_bytes: usize,
}

fn main() -> eyre::Result<()> {
    let args = parse_args()?;
    prepare_dir(&args.dir)?;
    println!(
        "chunks={} tx_chunks={} batch={} path_bytes={} allocator=system jemalloc=off",
        args.chunks, args.tx_chunks, args.batch, args.path_bytes
    );

    let mdbx_dir = args.dir.join("mdbx");
    fs::create_dir_all(&mdbx_dir)?;
    let map_bytes = mdbx_map_bytes(args.chunks, args.path_bytes);
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
        let tx_path = fill(args.path_bytes, u64::from(offset));
        let start_offset = RelativeChunkOffset(i32::try_from(offset)?);
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
                    data_size: u64::from(count) * 262_144,
                },
            )?;
            tx.add_pending_body_migration(
                start,
                &PendingBodyMigration {
                    data_root,
                    data_size: u64::from(count) * 262_144,
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
                fill(args.path_bytes, u64::from(at)),
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
        path.as_deref() == Some(fill(args.path_bytes, 0).as_slice()),
        "data path readback mismatch"
    );
    Ok(())
}

fn timed(f: impl FnOnce() -> eyre::Result<()>) -> eyre::Result<u128> {
    let started = Instant::now();
    f()?;
    Ok(started.elapsed().as_millis())
}

fn mdbx_map_bytes(chunks: u32, path_bytes: usize) -> usize {
    let estimate = u64::from(chunks)
        .saturating_mul(path_bytes as u64)
        .saturating_mul(4)
        .saturating_add(512 * 1024 * 1024);
    let cap = estimate.clamp(1 << 30, 8 << 30);
    usize::try_from(cap).unwrap_or(usize::MAX)
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
    let mut tx_chunks = 32_u32;
    let mut path_bytes = 4096_usize;
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
            "--tx-chunks" => tx_chunks = required(&mut it, &flag)?.parse()?,
            "--path-bytes" => path_bytes = required(&mut it, &flag)?.parse()?,
            other => eyre::bail!("unknown argument {other}"),
        }
    }
    let Some(dir) = dir else {
        print_help();
        eyre::bail!("--dir is required");
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
    })
}

fn required(it: &mut impl Iterator<Item = String>, flag: &str) -> eyre::Result<String> {
    it.next().ok_or_else(|| eyre::eyre!("{flag} needs a value"))
}

fn print_help() {
    eprintln!(
        "submodule-index-bench --dir <path/index-bench> [--chunks N] [--batch N] [--tx-chunks N] [--path-bytes N]\n\
Writes the same synthetic submodule index on durable MDBX and on RocksDB.\n\
The directory must be empty and must contain an index-bench path component."
    );
}
