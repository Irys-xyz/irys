use eyre::Context as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use irys_database::reth_db::mdbx::DatabaseArguments;
use irys_database::reth_db::{Database as _, DatabaseEnv, DatabaseEnvKind};
use irys_database::snapshot::{CopyFlags, copy_dir_recursive, copy_mdbx_env, strip_node_local};
use irys_database::tables::ConsensusTables;
use irys_database::{
    IrysDatabaseArgs as _, block_index_latest_height, database_schema_version, open_or_create_db,
};
use irys_reth_node_bridge::snapshot::snapshot_reth_state;
use reth_node_core::version::default_client_version;

use super::archive;
use super::manifest::{MANIFEST_FILENAME, SNAPSHOT_FORMAT_VERSION, SnapshotManifest};
use super::{
    BLOCK_INDEX_SUBDIR, GENESIS_COMMITMENTS_FILE, GENESIS_FILE, IRYS_CONSENSUS_SUBDIR, RETH_SUBDIR,
};

#[derive(Debug, Clone)]
pub(crate) struct ExportOpts {
    pub data_dir: PathBuf,
    pub output: PathBuf,
    pub include_caches: bool,
    pub chain_id: u64,
    pub copy_flags: CopyFlags,
}

pub(crate) fn run_export(opts: ExportOpts) -> eyre::Result<()> {
    if !opts.data_dir.is_dir() {
        eyre::bail!(
            "source data dir {} does not exist or is not a directory",
            opts.data_dir.display()
        );
    }

    let staging = tempfile::TempDir::new().context("creating snapshot staging directory")?;
    let staging_path = staging.path();

    let (effective_schema_version, source_tip) = snapshot_irys_consensus(&opts, staging_path)?;
    copy_block_index(&opts, staging_path)?;
    copy_genesis_files(&opts, staging_path)?;
    let reth_tip = snapshot_reth(&opts, staging_path)?;

    let files = archive::build_file_list(staging_path)?;
    let manifest = SnapshotManifest {
        format_version: SNAPSHOT_FORMAT_VERSION,
        chain_id: opts.chain_id,
        irys_schema_version: effective_schema_version,
        irys_tip_height: source_tip,
        reth_tip_height: reth_tip,
        includes_caches: opts.include_caches,
        created_at_unix_secs: now_unix_secs(),
        created_by: env!("CARGO_PKG_VERSION").to_owned(),
        files,
    };
    write_manifest(staging_path, &manifest)?;
    archive::pack(staging_path, &opts.output)
}

/// Open the source consensus DB read-only, snapshot it into staging, then
/// strip node-local tables from the copy. Returns the source's schema version
/// and the tip height. A source DB with no schema-version stamp is a hard
/// error: stamping the archive with this binary's version would let the
/// importer's schema gate pass for un-migrated state.
fn snapshot_irys_consensus(opts: &ExportOpts, staging: &Path) -> eyre::Result<(u32, Option<u64>)> {
    let src_path = opts.data_dir.join(IRYS_CONSENSUS_SUBDIR);
    if !src_path.is_dir() {
        eyre::bail!(
            "expected Irys consensus DB at {} (is --data-dir correct?)",
            src_path.display()
        );
    }

    let (schema_version, tip_height) = {
        let src_db = open_consensus_ro(&src_path)?;
        let mut tx = src_db.tx().context("begin source consensus RO tx")?;
        let schema = database_schema_version(&mut tx)
            .context("reading source schema version")?
            .ok_or_else(|| {
                eyre::eyre!(
                    "source consensus DB at {} has no schema-version stamp; start the \
                     node once so startup migrations write it, then re-run the export",
                    src_path.display()
                )
            })?;
        let tip = block_index_latest_height(&tx).context("reading source tip height")?;

        let dest = staging.join(IRYS_CONSENSUS_SUBDIR);
        copy_mdbx_env(&src_db, &dest, opts.copy_flags).context("copying Irys consensus mdbx")?;

        (schema, tip)
    };

    let dest_path = staging.join(IRYS_CONSENSUS_SUBDIR);
    let copy_db = open_or_create_db(
        &dest_path,
        ConsensusTables::ALL,
        DatabaseArguments::irys_default(irys_types::DbSyncMode::SafeNoSync)
            .context("default db args for copy")?,
    )
    .with_context(|| format!("opening copied Irys consensus at {}", dest_path.display()))?;
    strip_node_local(&copy_db, opts.include_caches)
        .context("stripping node-local rows from Irys consensus copy")?;
    drop(copy_db);

    Ok((schema_version, tip_height))
}

fn copy_block_index(opts: &ExportOpts, staging: &Path) -> eyre::Result<()> {
    let src = opts.data_dir.join(BLOCK_INDEX_SUBDIR);
    if !src.is_dir() {
        // Some Irys versions migrate block_index entries into the consensus DB
        // and stop maintaining the on-disk files. Absence is not an error.
        return Ok(());
    }
    let dest = staging.join(BLOCK_INDEX_SUBDIR);
    copy_dir_recursive(&src, &dest)
        .with_context(|| format!("copying block_index from {}", src.display()))
}

fn copy_genesis_files(opts: &ExportOpts, staging: &Path) -> eyre::Result<()> {
    for name in [GENESIS_FILE, GENESIS_COMMITMENTS_FILE] {
        let src = opts.data_dir.join(name);
        if !src.is_file() {
            continue;
        }
        let dest = staging.join(name);
        std::fs::copy(&src, &dest)
            .with_context(|| format!("copying {} to {}", src.display(), dest.display()))?;
    }
    Ok(())
}

fn snapshot_reth(opts: &ExportOpts, staging: &Path) -> eyre::Result<Option<u64>> {
    let src = opts.data_dir.join(RETH_SUBDIR);
    if !src.is_dir() {
        eyre::bail!(
            "expected Reth state dir at {} (is --data-dir correct?)",
            src.display()
        );
    }
    let dest = staging.join(RETH_SUBDIR);
    snapshot_reth_state(&src, &dest, opts.copy_flags).context("snapshotting Reth state")
}

fn open_consensus_ro(path: &Path) -> eyre::Result<DatabaseEnv> {
    DatabaseEnv::open(
        path,
        DatabaseEnvKind::RO,
        DatabaseArguments::new(default_client_version())
            .with_log_level(None)
            .with_exclusive(Some(false)),
    )
    .with_context(|| format!("opening Irys consensus RO at {}", path.display()))
}

fn write_manifest(staging: &Path, manifest: &SnapshotManifest) -> eyre::Result<()> {
    let path = staging.join(MANIFEST_FILENAME);
    let contents =
        serde_json::to_string_pretty(manifest).context("serializing snapshot manifest")?;
    std::fs::write(&path, contents)
        .with_context(|| format!("writing manifest to {}", path.display()))
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::archive::unpack;
    use irys_database::insert_peer_list_item;
    use irys_database::reth_db::cursor::DbCursorRO as _;
    use irys_database::reth_db::transaction::DbTx as _;
    use irys_database::tables::{IrysBlockHeaders, PeerListItems};
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{
        IrysAddress, IrysBlockHeader, IrysPeerId, PeerAddress, PeerListItem, PeerScore,
    };

    fn make_peer(byte: u8) -> (IrysPeerId, PeerListItem) {
        let addr = IrysAddress::repeat_byte(byte);
        let peer_id = IrysPeerId::from(addr);
        let item = PeerListItem {
            peer_id,
            mining_address: addr,
            reputation_score: PeerScore::new(50),
            response_time: 5,
            address: PeerAddress {
                gossip: "127.0.0.1:1".parse().unwrap(),
                api: "127.0.0.1:2".parse().unwrap(),
                execution: Default::default(),
            },
            last_seen: 0,
            is_online: true,
            protocol_version: Default::default(),
        };
        (peer_id, item)
    }

    fn populate_irys_db(env: &DatabaseEnv) -> irys_types::H256 {
        let mut block = IrysBlockHeader::new_mock_header();
        block.block_hash.0[0] = 0x77;
        env.update(|tx| irys_database::insert_block_header(tx, &block))
            .expect("insert block")
            .expect("insert block ok");

        let (peer_a, item_a) = make_peer(0xA0);
        let (peer_b, item_b) = make_peer(0xB0);
        env.update(|tx| {
            insert_peer_list_item(tx, &peer_a, &item_a)?;
            insert_peer_list_item(tx, &peer_b, &item_b)?;
            Ok::<(), eyre::Report>(())
        })
        .expect("insert peers")
        .expect("insert peers ok");

        // Real nodes stamp this at startup via `ensure_db_version_compatible`;
        // export now hard-errors on its absence, so fixtures must stamp it too.
        env.update(|tx| {
            irys_database::set_database_schema_version(tx, irys_types::DatabaseVersion::CURRENT)
        })
        .expect("stamp schema outer")
        .expect("stamp schema inner");

        block.block_hash
    }

    fn populate_reth_db(path: &Path) {
        let env = DatabaseEnv::open(
            path,
            DatabaseEnvKind::RW,
            DatabaseArguments::new(default_client_version())
                .with_log_level(None)
                .with_exclusive(Some(false)),
        )
        .expect("open reth rw");
        env.update(|_tx| Ok::<(), eyre::Report>(()))
            .expect("rw outer")
            .expect("rw inner");
    }

    fn build_fixture_data_dir() -> (tempfile::TempDir, PathBuf, irys_types::H256) {
        let dir = TempDirBuilder::new().build();
        let data_dir = dir.path().to_path_buf();

        let irys_path = data_dir.join(IRYS_CONSENSUS_SUBDIR);
        let irys_db = open_or_create_db(
            &irys_path,
            ConsensusTables::ALL,
            DatabaseArguments::irys_testing().expect("testing args"),
        )
        .expect("open irys db");
        let block_hash = populate_irys_db(&irys_db);
        drop(irys_db);

        let block_index = data_dir.join(BLOCK_INDEX_SUBDIR);
        std::fs::create_dir(&block_index).expect("mk block_index");
        std::fs::write(block_index.join("supply_state.dat"), [0_u8; 50])
            .expect("write supply_state");

        std::fs::write(data_dir.join(GENESIS_FILE), b"{}").expect("write genesis");

        let reth_dir = data_dir.join(RETH_SUBDIR);
        let reth_db_dir = reth_dir.join("db");
        std::fs::create_dir_all(&reth_db_dir).expect("mk reth db");
        populate_reth_db(&reth_db_dir);
        let static_files = reth_dir.join("static_files");
        std::fs::create_dir(&static_files).expect("mk static_files");
        std::fs::write(static_files.join("headers.jf"), b"hdr").expect("write seg");

        // Node-local sidecars that must NOT survive.
        std::fs::write(reth_dir.join("jwt.hex"), b"secret").expect("write jwt");

        (dir, data_dir, block_hash)
    }

    fn peer_count(env: &DatabaseEnv) -> usize {
        let tx = env.tx().expect("ro tx");
        let mut cursor = tx.cursor_read::<PeerListItems>().expect("cursor");
        let mut n = 0;
        while cursor.next().expect("walk").is_some() {
            n += 1;
        }
        n
    }

    fn block_count(env: &DatabaseEnv) -> usize {
        let tx = env.tx().expect("ro tx");
        let mut cursor = tx.cursor_read::<IrysBlockHeaders>().expect("cursor");
        let mut n = 0;
        while cursor.next().expect("walk").is_some() {
            n += 1;
        }
        n
    }

    #[test]
    fn end_to_end_export_archive() {
        let (_data_keep, data_dir, block_hash) = build_fixture_data_dir();
        let out_dir = TempDirBuilder::new().build();
        let archive_path = out_dir.path().join("snap.tar.zst");

        run_export(ExportOpts {
            data_dir,
            output: archive_path.clone(),
            include_caches: false,
            chain_id: 12345,
            copy_flags: CopyFlags {
                compact: true,
                throttle_mvcc: true,
            },
        })
        .expect("export runs");

        assert!(archive_path.is_file(), "archive produced");

        let unpack_dir = TempDirBuilder::new().build();
        let manifest = unpack(&archive_path, unpack_dir.path()).expect("unpack");
        assert_eq!(manifest.chain_id, 12345);
        assert_eq!(manifest.format_version, SNAPSHOT_FORMAT_VERSION);
        assert!(!manifest.includes_caches);

        let extracted_irys = unpack_dir.path().join(IRYS_CONSENSUS_SUBDIR);
        let extracted_reth_db = unpack_dir.path().join(RETH_SUBDIR).join("db");
        let extracted_reth_seg = unpack_dir
            .path()
            .join(RETH_SUBDIR)
            .join("static_files/headers.jf");
        let extracted_genesis = unpack_dir.path().join(GENESIS_FILE);
        let extracted_supply = unpack_dir
            .path()
            .join(BLOCK_INDEX_SUBDIR)
            .join("supply_state.dat");

        assert!(extracted_irys.join("mdbx.dat").is_file());
        assert!(extracted_reth_db.join("mdbx.dat").is_file());
        assert!(extracted_reth_seg.is_file());
        assert!(extracted_genesis.is_file());
        assert!(extracted_supply.is_file());
        assert!(
            !unpack_dir.path().join(RETH_SUBDIR).join("jwt.hex").exists(),
            "node-local jwt.hex was not exported"
        );

        // Re-open the extracted Irys DB and verify that peers were stripped
        // but the specific block header survives.
        let copy_db = open_or_create_db(
            &extracted_irys,
            ConsensusTables::ALL,
            DatabaseArguments::irys_testing().expect("testing args"),
        )
        .expect("open extracted irys db");
        assert_eq!(peer_count(&copy_db), 0, "peers stripped");
        assert_eq!(block_count(&copy_db), 1, "block header carried over");
        let tx = copy_db.tx().expect("ro tx");
        assert!(
            tx.get::<IrysBlockHeaders>(block_hash)
                .expect("get header")
                .is_some(),
            "specific block_hash {:?} preserved",
            block_hash
        );
    }

    #[test]
    fn export_fails_when_data_dir_missing() {
        let missing = std::path::PathBuf::from("/this/path/should/not/exist/at/all");
        let out_dir = TempDirBuilder::new().build();
        let err = run_export(ExportOpts {
            data_dir: missing,
            output: out_dir.path().join("snap.tar.zst"),
            include_caches: false,
            chain_id: 1,
            copy_flags: CopyFlags::default(),
        })
        .unwrap_err();
        assert!(err.to_string().contains("does not exist"), "got: {err}");
    }

    /// Fix #4: a source consensus DB with no `DBSchemaVersion` stamp must abort
    /// the export rather than silently label the archive with this binary's
    /// version (which would defeat the importer's schema gate).
    #[test]
    fn export_fails_when_source_db_has_no_schema_version() {
        let dir = TempDirBuilder::new().build();
        let data_dir = dir.path().to_path_buf();
        let irys_path = data_dir.join(IRYS_CONSENSUS_SUBDIR);
        let db = open_or_create_db(
            &irys_path,
            ConsensusTables::ALL,
            DatabaseArguments::irys_testing().expect("testing args"),
        )
        .expect("open irys db");
        drop(db);

        let out_dir = TempDirBuilder::new().build();
        let err = run_export(ExportOpts {
            data_dir,
            output: out_dir.path().join("snap.tar.zst"),
            include_caches: false,
            chain_id: 1,
            copy_flags: CopyFlags::default(),
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("no schema-version stamp"),
            "got: {err}"
        );
    }
}
