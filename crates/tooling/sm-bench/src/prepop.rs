//! Synthetic index rows so the submodule index is not an empty hot cache.
//!
//! Keys are `sha256("irys-sm-bench-prepop-v1" || index)`. One tx path covers 32
//! chunk rows. Puts are idempotent. The counter in `bench-prepop.json` advances
//! only after a commit. No `PendingBodyMigrationsByOffset` rows are written:
//! those are a work backlog, not index bulk.

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
    while done < target {
        let mut batch_end = done.saturating_add(BATCH).min(target);
        // Commit on a tx boundary so a resume does not store two data sizes
        // for one data root. The last batch may be a short tx.
        if batch_end < target {
            batch_end -= batch_end % TX_CHUNKS;
            if batch_end <= done {
                batch_end = done.saturating_add(TX_CHUNKS).min(target);
            }
        }
        let from = done - (done % TX_CHUNKS);
        submodule
            .db
            .update(|tx| write_batch(tx, from, batch_end))?;
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
        tx.add_data_root_info(
            data_root,
            &DataRootInfo {
                start_offset: RelativeChunkOffset(start_i32),
                data_size: chunk_count.saturating_mul(crate::guard::CHUNK_SIZE),
            },
        )?;
        let start = PartitionChunkOffset::from(
            u32::try_from(tx_start).map_err(|_| eyre!("prepop offset exceeds u32"))?,
        );
        let end = PartitionChunkOffset::from(
            u32::try_from(tx_end).map_err(|_| eyre!("prepop offset exceeds u32"))?,
        );
        tx.add_tx_path_hash_to_offset_range(start, end, Some(tx_path_hash))?;
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
    Ok(())
}
