//! Synthetic index rows so the submodule index is not an empty hot cache.
//!
//! Keys are `sha256("irys-sm-bench-prepop-v1" || index)`. One tx path covers 32
//! chunk rows. Puts are idempotent. The counter in `bench-prepop.json` advances
//! only after a commit. A count that ends inside a tx is final: a later larger
//! count is refused, so one data root keeps one data size. No
//! `PendingBodyMigrationsByOffset` rows are written: those are a work backlog,
//! not index bulk.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::Path;

use eyre::{WrapErr as _, eyre};
use irys_database::submodule::tables::{DataRootInfo, TxLeafBinding};
use irys_database::submodule::{SubmoduleStore as _, SubmoduleWrite};
use irys_domain::StorageModule;
use irys_types::{H256, PartitionChunkOffset, RelativeChunkOffset, hash_sha256};
use serde::{Deserialize, Serialize};

const VERSION: u32 = 1;
const TX_CHUNKS: u64 = 32;
const BATCH: u64 = 1024;
const PREFIX: &[u8] = b"irys-sm-bench-prepop-v1";

#[derive(Debug, Serialize, Deserialize)]
struct State {
    version: u32,
    done: u64,
}

pub(crate) fn prepopulate(
    module: &StorageModule,
    dir: &Path,
    chunks: u32,
    count: u64,
) -> eyre::Result<u64> {
    if count == 0 {
        return Ok(0);
    }
    let cap = u64::from(chunks);
    let target = count.min(cap);
    if count > cap {
        eprintln!("irys-sm-bench: capping --prepopulate-index at {cap} (partition chunk count)");
    }
    let path = dir.join("bench-prepop.json");
    let mut done = read_state(&path)?;
    if done > target {
        return Ok(done);
    }
    let submodule = module
        .get_submodule(PartitionChunkOffset::from(0_u32))
        .ok_or_else(|| eyre!("storage module has no submodule at offset 0"))?;
    // MDBX appends a DataRootInfo when the data_size differs. A count that
    // already ends inside a tx must not grow: that write would store a second
    // size for the same data root. The same count replays equal infos.
    if done < target && !done.is_multiple_of(TX_CHUNKS) {
        return Err(eyre!(
            "refusing to grow --prepopulate-index from {done} to {target}: {done} ends inside a tx, and a larger count would store a second data size for that data root"
        ));
    }
    while done < target {
        let mut batch_end = done.saturating_add(BATCH).min(target);
        // Every batch stops on a tx boundary, including the last one. The
        // short tail is its own commit, and only when it finishes `target`.
        let aligned = batch_end - (batch_end % TX_CHUNKS);
        if aligned > done {
            batch_end = aligned;
        } else {
            let remainder = batch_end - done;
            if done.saturating_add(remainder) != target {
                return Err(eyre!(
                    "refusing short prepopulate batch {done}..{batch_end}: it does not finish {target}"
                ));
            }
        }
        // A crash can commit the short tail before the counter rename. The
        // counter then names this tx start, or is missing. A stored short
        // size is terminal unless this count ends on that same size.
        if let Some(short_end) = stored_short_end(module, done, batch_end)? {
            if target != short_end {
                return Err(eyre!(
                    "refusing to grow --prepopulate-index from {done} to {target}: a stored short data size ends at {short_end}, and a larger count would store a second data size for that data root"
                ));
            }
        }
        let from = done - (done % TX_CHUNKS);
        submodule.db.update(|tx| write_batch(tx, from, batch_end))?;
        done = batch_end;
        write_state(&path, done)?;
        eprintln!("irys-sm-bench: prepopulate index {done} / {target}");
    }
    Ok(done)
}

fn write_batch(tx: &mut dyn SubmoduleWrite, from: u64, to: u64) -> eyre::Result<()> {
    let mut updates = Vec::new();
    let mut offset = from;
    while offset < to {
        let tx_index = offset / TX_CHUNKS;
        let tx_start = tx_index * TX_CHUNKS;
        let tx_end = tx_start.saturating_add(TX_CHUNKS - 1).min(to - 1);
        let tx_hash = keyed(b"tx", tx_index);
        let data_root = H256::from(tx_hash);
        let tx_path = {
            let mut bytes = Vec::with_capacity(64);
            bytes.extend_from_slice(&tx_hash);
            bytes.extend_from_slice(&tx_hash);
            bytes
        };
        let tx_path_hash = H256::from(hash_sha256(&tx_path));
        tx.add_full_tx_path(tx_path_hash, tx_path)?;
        tx.add_tx_leaf_binding(
            tx_path_hash,
            &TxLeafBinding {
                data_root,
                prefix_hash: H256::zero(),
            },
        )?;
        let start_i32 = i32::try_from(tx_start).map_err(|_| eyre!("prepop offset exceeds i32"))?;
        let chunk_count = tx_end - tx_start + 1;
        let start = PartitionChunkOffset::from(
            u32::try_from(tx_start).map_err(|_| eyre!("prepop offset exceeds u32"))?,
        );
        let end = PartitionChunkOffset::from(
            u32::try_from(tx_end).map_err(|_| eyre!("prepop offset exceeds u32"))?,
        );
        // The interval creates the unfilled placement. Rocks fills that
        // placement in this batch, so the data size follows the interval.
        tx.add_tx_path_hash_to_offset_range(start, end, Some(tx_path_hash))?;
        tx.add_data_root_info(
            data_root,
            &DataRootInfo {
                start_offset: RelativeChunkOffset(start_i32),
                data_size: chunk_count.saturating_mul(crate::guard::CHUNK_SIZE),
            },
        )?;
        let mut chunk = tx_start.max(from);
        while chunk <= tx_end {
            let path_hash_bytes = keyed(b"chunk", chunk);
            let mut data_path = Vec::with_capacity(64);
            data_path.extend_from_slice(&path_hash_bytes);
            data_path.extend_from_slice(&path_hash_bytes);
            let path_hash = H256::from(hash_sha256(&data_path));
            let part = PartitionChunkOffset::from(
                u32::try_from(chunk).map_err(|_| eyre!("prepop offset exceeds u32"))?,
            );
            updates.push((part, path_hash, data_path));
            chunk += 1;
        }
        offset = tx_end + 1;
    }
    tx.write_data_path_updates(updates)?;
    Ok(())
}

fn keyed(tag: &[u8], index: u64) -> [u8; 32] {
    let mut buf = Vec::with_capacity(PREFIX.len() + tag.len() + 8);
    buf.extend_from_slice(PREFIX);
    buf.extend_from_slice(tag);
    buf.extend_from_slice(&index.to_le_bytes());
    hash_sha256(&buf)
}

fn read_state(path: &Path) -> eyre::Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let text = fs::read_to_string(path)?;
    let state: State = serde_json::from_str(&text).wrap_err("bench-prepop.json")?;
    if state.version != VERSION {
        return Err(eyre!(
            "bench-prepop.json version {} is not {VERSION}",
            state.version
        ));
    }
    Ok(state.done)
}

fn write_state(path: &Path, done: u64) -> eyre::Result<()> {
    let state = State {
        version: VERSION,
        done,
    };
    let text = serde_json::to_vec(&state)?;
    let tmp = path.with_extension("json.tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(&text)?;
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, path)?;
    // The file sync does not make the rename durable. Sync the directory too.
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        let dir = File::open(parent).wrap_err("open prepop counter directory")?;
        dir.sync_all().wrap_err("sync prepop counter directory")?;
    }
    Ok(())
}

/// Shortest stored data size in `[from, to)` that does not fill its tx.
///
/// `Some(end)` is the chunk count where that tx stops. A full tx is not
/// terminal: replaying it writes the same size. The smallest short size wins
/// when one root has more than one.
fn stored_short_end(module: &StorageModule, from: u64, to: u64) -> eyre::Result<Option<u64>> {
    let full = TX_CHUNKS.saturating_mul(crate::guard::CHUNK_SIZE);
    let mut tx_at = from - (from % TX_CHUNKS);
    while tx_at < to {
        let root = H256::from(keyed(b"tx", tx_at / TX_CHUNKS));
        let infos = module.collect_data_root_infos(root)?;
        let start = i32::try_from(tx_at).map_err(|_| eyre!("prepop offset exceeds i32"))?;
        let mut short_end: Option<u64> = None;
        for info in &infos.0 {
            if info.start_offset.0 != start {
                continue;
            }
            let size = info.data_size;
            if size == 0 || size >= full || !size.is_multiple_of(crate::guard::CHUNK_SIZE) {
                continue;
            }
            let end = tx_at.saturating_add(size / crate::guard::CHUNK_SIZE);
            short_end = Some(short_end.map_or(end, |prev| prev.min(end)));
        }
        if let Some(end) = short_end {
            return Ok(Some(end));
        }
        tx_at += TX_CHUNKS;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io::Write as _;
    use std::path::Path;

    use irys_database::submodule::RocksTuning;
    use irys_domain::{BenchIndex, StorageModule};
    use irys_testing_utils::TempDirBuilder;
    use irys_types::H256;

    use super::{TX_CHUNKS, prepopulate};
    use crate::guard::CHUNK_SIZE;
    use crate::module::{OpenRequest, open_module};

    fn write_chunk_file(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = File::create(path).unwrap();
        file.write_all(&vec![0xA5_u8; CHUNK_SIZE as usize]).unwrap();
        file.sync_all().unwrap();
    }

    fn open_at(dir: &Path, chunks: u32) -> crate::module::OpenedModule {
        open_index(dir, chunks, BenchIndex::Mdbx)
    }

    fn open_index(dir: &Path, chunks: u32, index: BenchIndex) -> crate::module::OpenedModule {
        let root = dir.parent().expect("module dir has a parent");
        fs::create_dir_all(root).unwrap();
        let recall = root.join("recall.dat");
        let write = root.join("write.dat");
        if !recall.exists() {
            write_chunk_file(&recall);
            write_chunk_file(&write);
        }
        open_module(OpenRequest {
            dir,
            recall_file: &recall,
            write_file: &write,
            partition_chunks: chunks,
            recall_chunks: 1,
            write_origin: 0,
            write_chunks: 1,
            num_writes_before_sync: 100,
            entropy_sweep_max_bytes: CHUNK_SIZE,
            entropy_read_grace_ms: 0,
            entropy_read_batch_bytes: CHUNK_SIZE,
            entropy_readahead_bytes: 0,
            entropy_readahead_cache_bytes: 0,
            intervals_persist_secs: 0,
            min_durable_interval_ms: 0,
            durable_headroom_permille: 1000,
            recall_direct_io: false,
            recall_offset_chunks: 0,
            index,
            index_group_commit: 0,
        })
        .unwrap_or_else(|error| panic!("open {}: {error:#}", dir.display()))
    }

    fn bytes(chunks: u64) -> u64 {
        chunks.saturating_mul(CHUNK_SIZE)
    }

    fn sizes(module: &StorageModule, tx_index: u64) -> Vec<(i32, u64)> {
        let root = H256::from(super::keyed(b"tx", tx_index));
        let infos = module
            .collect_data_root_infos(root)
            .unwrap_or_else(|error| panic!("infos for tx {tx_index}: {error:#}"));
        let mut pairs: Vec<(i32, u64)> = infos
            .0
            .iter()
            .map(|info| (info.start_offset.0, info.data_size))
            .collect();
        pairs.sort_unstable();
        pairs
    }

    #[test]
    fn extend_past_a_short_final_tx_keeps_one_data_size() {
        // Breaks if a later count rewrites the short tx with a larger data_size.
        // MDBX appends that info, so the data root then has two sizes.
        let tmp = TempDirBuilder::new().prefix("sm-prepop-extend-").build();
        let root = tmp.path().join("loadtest");
        let chunks = u32::try_from(TX_CHUNKS.saturating_mul(2)).unwrap();
        let short = TX_CHUNKS + 8;
        let opened = open_at(&root.join("mod"), chunks);
        let module = opened.module.as_ref();
        assert_eq!(
            prepopulate(module, &root.join("mod"), chunks, short).unwrap(),
            short
        );
        assert_eq!(sizes(module, 0), vec![(0, bytes(TX_CHUNKS))]);
        let short_start = i32::try_from(TX_CHUNKS).unwrap();
        assert_eq!(sizes(module, 1), vec![(short_start, bytes(8))]);

        let full = TX_CHUNKS * 2;
        match prepopulate(module, &root.join("mod"), chunks, full) {
            Ok(done) => panic!("grew a short tx to {done}; infos {:?}", sizes(module, 1)),
            Err(error) => {
                let message = format!("{error:#}");
                assert!(
                    message.contains("second data size"),
                    "unexpected error: {message}"
                );
            }
        }
        assert_eq!(sizes(module, 1), vec![(short_start, bytes(8))]);
        assert!(sizes(module, 2).is_empty());
        let text = fs::read_to_string(root.join("mod").join("bench-prepop.json")).unwrap();
        let state: super::State = serde_json::from_str(&text).unwrap();
        assert_eq!(state.done, short);
    }

    #[test]
    fn short_final_tx_resume_and_replay_keep_one_data_size() {
        // Same count, a missing counter, and a counter rewound to the tx
        // boundary all replay the short tx. Each replay keeps one data size.
        let tmp = TempDirBuilder::new().prefix("sm-prepop-replay-").build();
        let root = tmp.path().join("loadtest");
        let dir = root.join("mod");
        let chunks = u32::try_from(TX_CHUNKS.saturating_mul(2)).unwrap();
        let short = TX_CHUNKS + 8;
        let opened = open_at(&dir, chunks);
        let module = opened.module.as_ref();
        let short_start = i32::try_from(TX_CHUNKS).unwrap();
        assert_eq!(prepopulate(module, &dir, chunks, short).unwrap(), short);
        assert_eq!(prepopulate(module, &dir, chunks, short).unwrap(), short);
        assert_eq!(sizes(module, 0), vec![(0, bytes(TX_CHUNKS))]);
        assert_eq!(sizes(module, 1), vec![(short_start, bytes(8))]);

        fs::remove_file(dir.join("bench-prepop.json")).unwrap();
        assert_eq!(prepopulate(module, &dir, chunks, short).unwrap(), short);
        assert_eq!(sizes(module, 0), vec![(0, bytes(TX_CHUNKS))]);
        assert_eq!(sizes(module, 1), vec![(short_start, bytes(8))]);

        super::write_state(&dir.join("bench-prepop.json"), TX_CHUNKS).unwrap();
        assert_eq!(prepopulate(module, &dir, chunks, short).unwrap(), short);
        assert_eq!(sizes(module, 0), vec![(0, bytes(TX_CHUNKS))]);
        assert_eq!(sizes(module, 1), vec![(short_start, bytes(8))]);
    }

    #[test]
    fn aligned_extend_keeps_one_data_size_per_root() {
        // A count on a tx boundary still grows by whole txs. A short tail
        // after that boundary is still written once.
        let tmp = TempDirBuilder::new().prefix("sm-prepop-aligned-").build();
        let root = tmp.path().join("loadtest");
        let chunks = u32::try_from(TX_CHUNKS.saturating_mul(2)).unwrap();
        let full = TX_CHUNKS * 2;
        let grow = open_at(&root.join("grow"), chunks);
        let module = grow.module.as_ref();
        let dir = root.join("grow");
        assert_eq!(
            prepopulate(module, &dir, chunks, TX_CHUNKS).unwrap(),
            TX_CHUNKS
        );
        assert_eq!(sizes(module, 0), vec![(0, bytes(TX_CHUNKS))]);
        assert!(sizes(module, 1).is_empty());
        assert_eq!(prepopulate(module, &dir, chunks, full).unwrap(), full);
        assert_eq!(sizes(module, 0), vec![(0, bytes(TX_CHUNKS))]);
        let next = i32::try_from(TX_CHUNKS).unwrap();
        assert_eq!(sizes(module, 1), vec![(next, bytes(TX_CHUNKS))]);
        assert_eq!(prepopulate(module, &dir, chunks, full).unwrap(), full);
        assert_eq!(sizes(module, 1), vec![(next, bytes(TX_CHUNKS))]);
        drop(grow);

        let tail = open_at(&root.join("tail"), chunks);
        let module = tail.module.as_ref();
        let dir = root.join("tail");
        let short = TX_CHUNKS + 8;
        assert_eq!(
            prepopulate(module, &dir, chunks, TX_CHUNKS).unwrap(),
            TX_CHUNKS
        );
        assert_eq!(prepopulate(module, &dir, chunks, short).unwrap(), short);
        assert_eq!(sizes(module, 0), vec![(0, bytes(TX_CHUNKS))]);
        assert_eq!(sizes(module, 1), vec![(next, bytes(8))]);
    }

    #[test]
    fn rocks_prepopulate_inserts_one_data_size() {
        // Breaks if the data size is stored before the interval. Rocks fills
        // one unfilled placement created in this batch, so the first put
        // errors and the index stays empty. A second pass of the same count
        // must keep that one size.
        let tmp = TempDirBuilder::new().prefix("sm-prepop-rocks-").build();
        let root = tmp.path().join("loadtest");
        let chunks = u32::try_from(TX_CHUNKS).unwrap();
        let tuning = RocksTuning::baseline().with_block_cache(64 * 1024 * 1024);
        let opened = open_index(&root.join("mod"), chunks, BenchIndex::Rocks(tuning));
        let module = opened.module.as_ref();
        let dir = root.join("mod");
        assert_eq!(
            prepopulate(module, &dir, chunks, TX_CHUNKS)
                .unwrap_or_else(|error| panic!("rocks prepopulate: {error:#}")),
            TX_CHUNKS
        );
        assert_eq!(sizes(module, 0), vec![(0, bytes(TX_CHUNKS))]);
        assert_eq!(
            prepopulate(module, &dir, chunks, TX_CHUNKS).unwrap(),
            TX_CHUNKS
        );
        assert_eq!(sizes(module, 0), vec![(0, bytes(TX_CHUNKS))]);
    }

    #[test]
    fn crash_before_the_counter_rename_then_a_larger_count_keeps_one_size() {
        // The short tail is durable and the counter still names the tx
        // boundary. A larger count must refuse before it stores a second size.
        // The same short count is still allowed and stays one size.
        let tmp = TempDirBuilder::new().prefix("sm-prepop-crash-").build();
        let root = tmp.path().join("loadtest");
        let dir = root.join("mod");
        let chunks = u32::try_from(TX_CHUNKS.saturating_mul(2)).unwrap();
        let short = TX_CHUNKS + 8;
        let opened = open_at(&dir, chunks);
        let module = opened.module.as_ref();
        let short_start = i32::try_from(TX_CHUNKS).unwrap();
        assert_eq!(prepopulate(module, &dir, chunks, short).unwrap(), short);
        super::write_state(&dir.join("bench-prepop.json"), TX_CHUNKS).unwrap();

        let full = TX_CHUNKS * 2;
        match prepopulate(module, &dir, chunks, full) {
            Ok(done) => panic!(
                "grew a lost short tail to {done}; infos {:?}",
                sizes(module, 1)
            ),
            Err(error) => {
                let message = format!("{error:#}");
                assert!(
                    message.contains("second data size"),
                    "unexpected error: {message}"
                );
            }
        }
        assert_eq!(sizes(module, 1), vec![(short_start, bytes(8))]);
        assert!(sizes(module, 2).is_empty());
        let text = fs::read_to_string(dir.join("bench-prepop.json")).unwrap();
        let state: super::State = serde_json::from_str(&text).unwrap();
        assert_eq!(state.done, TX_CHUNKS);

        assert_eq!(prepopulate(module, &dir, chunks, short).unwrap(), short);
        assert_eq!(sizes(module, 0), vec![(0, bytes(TX_CHUNKS))]);
        assert_eq!(sizes(module, 1), vec![(short_start, bytes(8))]);
    }

    #[test]
    fn missing_counter_after_a_short_first_batch_keeps_one_size() {
        // The only commit is a short tail, and the counter file is gone.
        // A larger count must refuse. Replaying that short count keeps one size.
        let tmp = TempDirBuilder::new().prefix("sm-prepop-first-").build();
        let root = tmp.path().join("loadtest");
        let dir = root.join("mod");
        let chunks = u32::try_from(TX_CHUNKS.saturating_mul(2)).unwrap();
        let short = 8_u64;
        let opened = open_at(&dir, chunks);
        let module = opened.module.as_ref();
        assert_eq!(prepopulate(module, &dir, chunks, short).unwrap(), short);
        assert_eq!(sizes(module, 0), vec![(0, bytes(short))]);
        fs::remove_file(dir.join("bench-prepop.json")).unwrap();

        let full = TX_CHUNKS * 2;
        match prepopulate(module, &dir, chunks, full) {
            Ok(done) => panic!(
                "grew a lost first short batch to {done}; infos {:?}",
                sizes(module, 0)
            ),
            Err(error) => {
                let message = format!("{error:#}");
                assert!(
                    message.contains("second data size"),
                    "unexpected error: {message}"
                );
            }
        }
        assert_eq!(sizes(module, 0), vec![(0, bytes(short))]);
        assert!(sizes(module, 1).is_empty());
        assert!(!dir.join("bench-prepop.json").exists());

        assert_eq!(prepopulate(module, &dir, chunks, short).unwrap(), short);
        assert_eq!(sizes(module, 0), vec![(0, bytes(short))]);
        assert!(sizes(module, 1).is_empty());
    }

    #[test]
    fn missing_counter_after_a_committed_short_tail_keeps_one_size() {
        // Both the aligned tx and the short tail are durable, and the counter
        // file is gone. A larger count may replay the full tx, then must stop
        // before a second size. The original short count still replays as one size.
        let tmp = TempDirBuilder::new().prefix("sm-prepop-lost-").build();
        let root = tmp.path().join("loadtest");
        let dir = root.join("mod");
        let chunks = u32::try_from(TX_CHUNKS.saturating_mul(2)).unwrap();
        let short = TX_CHUNKS + 8;
        let opened = open_at(&dir, chunks);
        let module = opened.module.as_ref();
        let short_start = i32::try_from(TX_CHUNKS).unwrap();
        assert_eq!(prepopulate(module, &dir, chunks, short).unwrap(), short);
        fs::remove_file(dir.join("bench-prepop.json")).unwrap();

        let full = TX_CHUNKS * 2;
        match prepopulate(module, &dir, chunks, full) {
            Ok(done) => panic!(
                "grew a lost short tail to {done}; infos {:?}",
                sizes(module, 1)
            ),
            Err(error) => {
                let message = format!("{error:#}");
                assert!(
                    message.contains("second data size"),
                    "unexpected error: {message}"
                );
            }
        }
        assert_eq!(sizes(module, 0), vec![(0, bytes(TX_CHUNKS))]);
        assert_eq!(sizes(module, 1), vec![(short_start, bytes(8))]);
        assert!(sizes(module, 2).is_empty());

        assert_eq!(prepopulate(module, &dir, chunks, short).unwrap(), short);
        assert_eq!(sizes(module, 1), vec![(short_start, bytes(8))]);
        let text = fs::read_to_string(dir.join("bench-prepop.json")).unwrap();
        let state: super::State = serde_json::from_str(&text).unwrap();
        assert_eq!(state.done, short);
    }
}
