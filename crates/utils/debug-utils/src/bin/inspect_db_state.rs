//! Wider DB-state inspector for divergence post-mortems.
//!
//! Usage: inspect_db_state <db_path> <subcommand> [args]
//!
//! Subcommands:
//!   migrated-range <low> <high>        Dump MigratedBlockHashes[low..=high] with each block's
//!                                      Publish/Submit ledger tx_ids
//!   block <hash_b58>                   Look up IrysBlockHeaders[hash] and dump ledger contents
//!   scan-promoted <low> <high>         Full-scan IrysDataTxMetadata; report all entries with
//!                                      promoted_height in [low..=high]
//!   ingress-proofs <data_root_b58>     Look up CachedIngressProofs for a data_root

use eyre::{Context as _, eyre};
use irys_database::reth_db::{
    Database as _, DatabaseEnv, DatabaseEnvKind, cursor::DbCursorRO as _, mdbx::DatabaseArguments,
    transaction::DbTx as _,
};
use irys_database::tables::{IrysBlockHeaders, IrysDataTxMetadata, MigratedBlockHashes};
use irys_types::H256;
use reth_node_core::version::default_client_version;
use std::path::PathBuf;

fn parse_hash(s: &str) -> eyre::Result<H256> {
    let bytes = bs58::decode(s)
        .into_vec()
        .with_context(|| format!("decode base58 {s:?}"))?;
    if bytes.len() != 32 {
        return Err(eyre!("hash must decode to 32 bytes, got {}", bytes.len()));
    }
    let mut arr = [0_u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(H256(arr))
}

fn open_ro(db_path: &PathBuf) -> eyre::Result<DatabaseEnv> {
    DatabaseEnv::open(
        db_path,
        DatabaseEnvKind::RO,
        DatabaseArguments::new(default_client_version())
            .with_log_level(None)
            .with_exclusive(Some(false)),
    )
    .with_context(|| format!("open env at {db_path:?}"))
}

fn b58(h: &H256) -> String {
    bs58::encode(h.as_bytes()).into_string()
}

fn dump_block(
    tx: &<DatabaseEnv as irys_database::reth_db::Database>::TX,
    hash: &H256,
    indent: &str,
) -> eyre::Result<()> {
    match tx.get::<IrysBlockHeaders>(*hash)? {
        None => println!("{indent}IrysBlockHeaders[{}] MISSING", b58(hash)),
        Some(compact) => {
            let header: irys_types::IrysBlockHeader = compact.into();
            println!(
                "{indent}IrysBlockHeaders[{}] height={} parent={}",
                b58(hash),
                header.height,
                b58(&header.previous_block_hash)
            );
            for dl in &header.data_ledgers {
                println!(
                    "{indent}  ledger {} tx_ids({}): {:?}",
                    dl.ledger_id,
                    dl.tx_ids.0.len(),
                    dl.tx_ids.0.iter().map(b58).collect::<Vec<_>>()
                );
            }
        }
    }
    Ok(())
}

fn main() -> eyre::Result<()> {
    let mut args = std::env::args().skip(1);
    let db_path: PathBuf = args
        .next()
        .ok_or_else(|| eyre!("usage: inspect_db_state <db_path> <subcommand> [args]"))?
        .into();
    let sub = args.next().ok_or_else(|| eyre!("missing subcommand"))?;

    let env = open_ro(&db_path)?;
    let tx = env.tx().context("begin RO txn")?;

    match sub.as_str() {
        "migrated-range" => {
            let low: u64 = args.next().ok_or_else(|| eyre!("missing low"))?.parse()?;
            let high: u64 = args.next().ok_or_else(|| eyre!("missing high"))?.parse()?;
            println!("== MigratedBlockHashes[{low}..={high}] ==");
            for h in low..=high {
                match tx.get::<MigratedBlockHashes>(h)? {
                    None => println!("  [{h}] NONE"),
                    Some(hash) => {
                        println!("  [{h}] {}", b58(&hash));
                        dump_block(&tx, &hash, "        ")?;
                    }
                }
            }
        }
        "block" => {
            let hash = parse_hash(&args.next().ok_or_else(|| eyre!("missing hash"))?)?;
            dump_block(&tx, &hash, "")?;
        }
        "scan-promoted" => {
            let low: u64 = args.next().ok_or_else(|| eyre!("missing low"))?.parse()?;
            let high: u64 = args.next().ok_or_else(|| eyre!("missing high"))?.parse()?;
            println!("== scanning IrysDataTxMetadata for promoted_height in [{low}..={high}] ==");
            let mut cur = tx.cursor_read::<IrysDataTxMetadata>()?;
            let mut found = 0_u64;
            let mut scanned = 0_u64;
            let walker = cur.walk(None)?;
            for item in walker {
                let (tx_id, compact) = item?;
                scanned += 1;
                let meta: irys_types::DataTransactionMetadata = compact.clone().into();
                if let Some(ph) = meta.promoted_height
                    && ph >= low
                    && ph <= high
                {
                    found += 1;
                    println!(
                        "  tx={} included={:?} promoted={}",
                        b58(&tx_id),
                        meta.included_height,
                        ph
                    );
                }
            }
            println!("scanned {scanned} rows, found {found} matching");
        }
        other => return Err(eyre!("unknown subcommand: {other}")),
    }

    Ok(())
}
