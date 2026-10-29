//! Read-only inspection of `IrysDataTxHeaders`, `IrysDataTxMetadata`, and
//! `MigratedBlockHashes` for a specific tx id.
//!
//! Usage: inspect_tx <db_path> <tx_id_base58> [lca_height]
//!
//! `<db_path>` is the irys_consensus_data directory (the one containing `mdbx.dat`).
//! `<tx_id_base58>` is the base58-encoded transaction id.
//! `[lca_height]` is an optional block height — when provided, the tool also
//! dumps `MigratedBlockHashes[lca_height]` for cross-referencing against a
//! known fork point during divergence post-mortems.
//!
//! Opens the env in RO mode with `with_exclusive(false)` so it can run alongside
//! a live node if needed.

use eyre::{Context as _, eyre};
use irys_database::reth_db::{
    Database as _, DatabaseEnv, DatabaseEnvKind, mdbx::DatabaseArguments, transaction::DbTx as _,
};
use irys_database::tables::{
    IrysBlockHeaders, IrysDataTxHeaders, IrysDataTxMetadata, MigratedBlockHashes,
};
use irys_types::{H256, ToBase58 as _};
use reth_node_core::version::default_client_version;
use std::path::PathBuf;

fn main() -> eyre::Result<()> {
    let mut args = std::env::args().skip(1);
    let db_path: PathBuf = args
        .next()
        .ok_or_else(|| eyre!("usage: inspect_tx <db_path> <tx_id_base58> [lca_height]"))?
        .into();
    let tx_id_b58 = args
        .next()
        .ok_or_else(|| eyre!("usage: inspect_tx <db_path> <tx_id_base58> [lca_height]"))?;
    let lca_height: Option<u64> = args
        .next()
        .map(|s| {
            s.parse()
                .with_context(|| format!("parse lca_height {s:?} as u64"))
        })
        .transpose()?;
    if args.next().is_some() {
        return Err(eyre!(
            "usage: inspect_tx <db_path> <tx_id_base58> [lca_height]"
        ));
    }

    let tx_id = H256::from_base58_result(&tx_id_b58)
        .with_context(|| format!("decode base58 tx id {tx_id_b58:?}"))?;

    let env = DatabaseEnv::open(
        &db_path,
        DatabaseEnvKind::RO,
        DatabaseArguments::new(default_client_version())
            .with_log_level(None)
            .with_exclusive(Some(false)),
    )
    .with_context(|| format!("open env at {db_path:?}"))?;

    let tx = env.tx().context("begin RO txn")?;

    println!(
        "== inspecting tx {tx_id_b58} (== 0x{}) ==",
        hex::encode(tx_id.as_bytes())
    );
    println!("== db_path: {} ==\n", db_path.display());

    // 1. IrysDataTxHeaders[tx_id]
    let header = tx
        .get::<IrysDataTxHeaders>(tx_id)
        .context("read IrysDataTxHeaders")?;
    match &header {
        Some(_) => println!("[IrysDataTxHeaders] tx header EXISTS"),
        None => println!("[IrysDataTxHeaders] tx header MISSING"),
    }

    // 2. IrysDataTxMetadata[tx_id]
    let metadata = tx
        .get::<IrysDataTxMetadata>(tx_id)
        .context("read IrysDataTxMetadata")?;
    let included_height = match &metadata {
        Some(m) => {
            let m: irys_types::DataTransactionMetadata = m.clone().into();
            println!(
                "[IrysDataTxMetadata] included_height={:?} promoted_height={:?}",
                m.included_height, m.promoted_height
            );
            m.included_height
        }
        None => {
            println!("[IrysDataTxMetadata] NO ROW (tx never had migrated metadata)");
            None
        }
    };

    // 3. MigratedBlockHashes[included_height]
    if let Some(h) = included_height {
        let mig = tx
            .get::<MigratedBlockHashes>(h)
            .context("read MigratedBlockHashes")?;
        match mig {
            Some(hash) => {
                println!(
                    "[MigratedBlockHashes][{h}] = {} (hex 0x{})",
                    hash.as_bytes().to_base58(),
                    hex::encode(hash.as_bytes())
                );
                // Also fetch the block header to confirm it's there
                match tx.get::<IrysBlockHeaders>(hash) {
                    Ok(Some(_)) => println!(
                        "  ↳ IrysBlockHeaders[{}] EXISTS",
                        hash.as_bytes().to_base58()
                    ),
                    Ok(None) => println!(
                        "  ↳ IrysBlockHeaders[{}] MISSING (orphaned?)",
                        hash.as_bytes().to_base58()
                    ),
                    Err(e) => println!(
                        "  ↳ IrysBlockHeaders[{}] READ ERROR: {e}",
                        hash.as_bytes().to_base58()
                    ),
                }
            }
            None => println!(
                "[MigratedBlockHashes][{h}] = NONE  ← height not canonical/migrated on this peer"
            ),
        }
    }

    // 4. Optional: dump a specific LCA / fork-point height for cross-referencing.
    if let Some(lca_h) = lca_height {
        let lca = tx
            .get::<MigratedBlockHashes>(lca_h)
            .context("read MigratedBlockHashes[LCA]")?;
        println!(
            "\n[MigratedBlockHashes][LCA={lca_h}] = {}",
            match lca {
                Some(h) => format!(
                    "{} (hex 0x{})",
                    h.as_bytes().to_base58(),
                    hex::encode(h.as_bytes())
                ),
                None => "NONE".to_string(),
            }
        );
    }

    Ok(())
}
