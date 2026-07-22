//! Read-only probe of submodule index tables for heal residual diagnosis.
//!
//! Usage:
//!   inspect_submodule_index <submodule_db_dir> [scan_end_exclusive]
//!
//! Opens RO so it can run while the node holds the writer.

use eyre::{Context as _, eyre};
use irys_database::reth_db::{
    Database as _, DatabaseEnv, DatabaseEnvKind, cursor::DbCursorRO as _, mdbx::DatabaseArguments,
    transaction::DbTx as _,
};
use irys_database::submodule::tables::{ChunkPathHashesByOffset, DataRootInfosByDataRoot};
use irys_types::PartitionChunkOffset;
use reth_node_core::version::default_client_version;
use std::path::PathBuf;

fn main() -> eyre::Result<()> {
    let mut args = std::env::args().skip(1);
    let db_path: PathBuf = args
        .next()
        .ok_or_else(|| eyre!("usage: inspect_submodule_index <db_dir> [scan_end_exclusive]"))?
        .into();
    let scan_end: u32 = args
        .next()
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(2_000);

    let env = DatabaseEnv::open(
        &db_path,
        DatabaseEnvKind::RO,
        DatabaseArguments::new(default_client_version())
            .with_log_level(None)
            .with_exclusive(Some(false)),
    )
    .with_context(|| format!("open env at {db_path:?}"))?;

    let tx = env.tx().context("begin RO txn")?;

    let path_count = tx.entries::<ChunkPathHashesByOffset>()? as u64;
    let root_count = tx.entries::<DataRootInfosByDataRoot>()? as u64;

    let mut path_cur = tx.cursor_read::<ChunkPathHashesByOffset>()?;
    let first = path_cur.first()?;
    let last = path_cur.last()?;

    println!("db={db_path:?}");
    println!("ChunkPathHashesByOffset.entries={path_count}");
    match &first {
        None => println!("ChunkPathHashesByOffset.first=NONE"),
        Some((k, v)) => println!(
            "ChunkPathHashesByOffset.first={} tx_path={} data_path={}",
            **k,
            v.tx_path_hash.is_some(),
            v.data_path_hash.is_some()
        ),
    }
    match &last {
        None => println!("ChunkPathHashesByOffset.last=NONE"),
        Some((k, v)) => println!(
            "ChunkPathHashesByOffset.last={} tx_path={} data_path={}",
            **k,
            v.tx_path_hash.is_some(),
            v.data_path_hash.is_some()
        ),
    }

    // Gap scan [0, scan_end): first missing + count present keys in window
    let start = PartitionChunkOffset::from(0_u32);
    let mut walker = path_cur.walk(Some(start))?;
    let mut expected = 0_u32;
    let mut first_gap: Option<u32> = None;
    let mut present_in_window = 0_u64;
    while let Some(item) = walker.next() {
        let (key, _) = item?;
        let k = *key;
        if k >= scan_end {
            break;
        }
        present_in_window += 1;
        if first_gap.is_none() && k > expected {
            first_gap = Some(expected);
        }
        if k >= expected {
            expected = k.saturating_add(1);
        }
    }
    if first_gap.is_none() && expected < scan_end {
        first_gap = Some(expected);
    }

    println!("scan_window=[0, {scan_end})");
    println!("path_hash_keys_in_window={present_in_window}");
    println!(
        "path_hash_first_gap={}",
        first_gap
            .map(|g| g.to_string())
            .unwrap_or_else(|| "NONE".into())
    );

    let dense_in_window = first_gap.is_none() && present_in_window == u64::from(scan_end);
    println!("path_hash_dense_in_window={dense_in_window}");

    // DataRootInfos samples
    let mut root_cur = tx.cursor_read::<DataRootInfosByDataRoot>()?;
    let mut empty_infos = 0_u64;
    let mut non_empty = 0_u64;
    let mut sample = 0_u64;
    let walker = root_cur.walk(None)?;
    for item in walker {
        let (root, infos) = item?;
        if infos.0.is_empty() {
            empty_infos += 1;
        } else {
            non_empty += 1;
        }
        if sample < 5 {
            println!(
                "  sample data_root={} infos_len={}",
                bs58::encode(root.as_bytes()).into_string(),
                infos.0.len()
            );
            sample += 1;
        }
    }
    println!("DataRootInfosByDataRoot.entries={root_count}");
    println!("DataRootInfosByDataRoot.non_empty={non_empty}");
    println!("DataRootInfosByDataRoot.empty_vec={empty_infos}");

    let narrow = dense_in_window && root_count == 0;
    println!("NARROW_RESIDUAL_HEURISTIC(path_dense_window_AND_zero_data_root_rows)={narrow}");

    let classic = first_gap.is_some() && root_count == 0;
    println!("CLASSIC_NEVER_INDEXED_HEURISTIC(path_gap_AND_zero_data_root_rows)={classic}");

    // Intermediate: path keys present for some of window but roots empty
    let partial_path_no_roots = present_in_window > 0 && root_count == 0;
    println!(
        "PATH_KEYS_PRESENT_BUT_ZERO_DATA_ROOT_ROWS={partial_path_no_roots}"
    );

    Ok(())
}
