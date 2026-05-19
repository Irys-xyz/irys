//! Snapshot export and import for Irys node state.
//!
//! Captures the chain-wide consensus and EVM state of a node so a different
//! node can be bootstrapped from the capture. Node-specific identity (signing
//! keys, peer reputation, partition packing) is excluded by design.

pub(crate) mod archive;
pub(crate) mod export;
pub(crate) mod import;
pub(crate) mod manifest;

pub(crate) const IRYS_CONSENSUS_SUBDIR: &str = "irys_consensus_data";
pub(crate) const BLOCK_INDEX_SUBDIR: &str = "block_index";
pub(crate) const RETH_SUBDIR: &str = "reth";
pub(crate) const GENESIS_FILE: &str = ".irys_genesis.json";
pub(crate) const GENESIS_COMMITMENTS_FILE: &str = ".irys_genesis_commitments.json";
pub(crate) const SUBMODULES_FILE: &str = "irys_submodules.toml";
pub(crate) const STORAGE_MODULES_SUBDIR: &str = "storage_modules";

#[cfg(test)]
mod round_trip_tests {
    use super::IRYS_CONSENSUS_SUBDIR;
    use super::export::{ExportOpts, run_export};
    use super::import::{ImportOpts, run_import};
    use irys_database::reth_db::cursor::DbCursorRO as _;
    use irys_database::reth_db::mdbx::DatabaseArguments;
    use irys_database::reth_db::transaction::DbTx as _;
    use irys_database::reth_db::{Database as _, DatabaseEnv, DatabaseEnvKind};
    use irys_database::tables::{IrysBlockHeaders, IrysTables, PeerListItems};
    use irys_database::{IrysDatabaseArgs as _, insert_peer_list_item, open_or_create_db};
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{
        IrysAddress, IrysBlockHeader, IrysPeerId, PeerAddress, PeerListItem, PeerScore,
    };
    use reth_node_core::version::default_client_version;
    use std::path::{Path, PathBuf};

    fn make_peer(byte: u8) -> (IrysPeerId, PeerListItem) {
        let addr = IrysAddress::repeat_byte(byte);
        let peer_id = IrysPeerId::from(addr);
        let item = PeerListItem {
            peer_id,
            mining_address: addr,
            reputation_score: PeerScore::new(60),
            response_time: 7,
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

    fn build_source(data_dir: &Path) -> irys_types::H256 {
        let irys_path = data_dir.join(IRYS_CONSENSUS_SUBDIR);
        let irys_db = open_or_create_db(
            &irys_path,
            IrysTables::ALL,
            DatabaseArguments::irys_testing().expect("testing args"),
        )
        .expect("open irys db");
        let mut block = IrysBlockHeader::new_mock_header();
        block.block_hash.0[0] = 0xCE;
        irys_db
            .update(|tx| irys_database::insert_block_header(tx, &block))
            .expect("insert outer")
            .expect("insert inner");
        let (peer, item) = make_peer(0xC0);
        irys_db
            .update(|tx| insert_peer_list_item(tx, &peer, &item))
            .expect("insert peer outer")
            .expect("insert peer inner");
        // Real nodes stamp this at startup; export now hard-errors on its
        // absence, so this fixture must stamp it too.
        irys_db
            .update(|tx| {
                irys_database::set_database_schema_version(tx, irys_types::DatabaseVersion::CURRENT)
            })
            .expect("stamp schema outer")
            .expect("stamp schema inner");
        drop(irys_db);

        std::fs::write(data_dir.join(".irys_genesis.json"), b"{}").expect("write genesis");

        let reth_db = data_dir.join("reth/db");
        std::fs::create_dir_all(&reth_db).expect("mk reth db");
        let env = DatabaseEnv::open(
            &reth_db,
            DatabaseEnvKind::RW,
            DatabaseArguments::new(default_client_version())
                .with_log_level(None)
                .with_exclusive(Some(false)),
        )
        .expect("open reth rw");
        env.update(|_tx| Ok::<(), eyre::Report>(()))
            .expect("rw outer")
            .expect("rw inner");
        drop(env);

        let static_files = data_dir.join("reth/static_files");
        std::fs::create_dir(&static_files).expect("mk static_files");
        std::fs::write(static_files.join("headers.jf"), b"hdr-payload").expect("write seg");

        // Node-local sidecar that must be excluded from the archive.
        std::fs::write(data_dir.join("reth/jwt.hex"), b"secret").expect("write jwt");

        block.block_hash
    }

    fn count_table<T: irys_database::reth_db::table::Table>(env: &DatabaseEnv) -> usize {
        let tx = env.tx().expect("ro tx");
        let mut cursor = tx.cursor_read::<T>().expect("cursor");
        let mut n = 0;
        while cursor.next().expect("walk").is_some() {
            n += 1;
        }
        n
    }

    #[test]
    fn export_then_import_round_trip() {
        let src_keep = TempDirBuilder::new().build();
        let src_dir: PathBuf = src_keep.path().to_path_buf();
        let block_hash = build_source(&src_dir);

        let out_dir = TempDirBuilder::new().build();
        let archive_path = out_dir.path().join("snap.tar.zst");

        run_export(ExportOpts {
            data_dir: src_dir,
            output: archive_path.clone(),
            include_caches: false,
            chain_id: 7777,
            copy_flags: irys_database::snapshot::CopyFlags {
                compact: true,
                throttle_mvcc: true,
            },
        })
        .expect("export");

        let target_keep = TempDirBuilder::new().build();
        let target_dir = target_keep.path().join("new-node");

        run_import(ImportOpts {
            input: archive_path,
            data_dir: target_dir.clone(),
            force: false,
            expected_chain_id: 7777,
            expected_irys_schema_version: 3,
        })
        .expect("import");

        assert!(
            target_dir.join("irys_consensus_data/mdbx.dat").is_file(),
            "consensus mdbx placed"
        );
        assert!(
            target_dir.join("reth/db/mdbx.dat").is_file(),
            "reth mdbx placed"
        );
        assert!(
            target_dir.join("reth/static_files/headers.jf").is_file(),
            "static_files placed"
        );
        assert!(
            target_dir.join(".irys_genesis.json").is_file(),
            "genesis placed"
        );
        assert!(
            !target_dir.join("manifest.json").exists(),
            "archive manifest does not pollute the data dir"
        );
        assert!(
            !target_dir.join("reth/jwt.hex").exists(),
            "node-local jwt.hex not carried over"
        );

        let imported = open_or_create_db(
            target_dir.join(IRYS_CONSENSUS_SUBDIR),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().expect("testing args"),
        )
        .expect("open imported irys db");
        assert_eq!(count_table::<PeerListItems>(&imported), 0, "peers stripped");
        assert_eq!(
            count_table::<IrysBlockHeaders>(&imported),
            1,
            "block header carried over"
        );
        let tx = imported.tx().expect("ro tx");
        let header: Option<_> = tx.get::<IrysBlockHeaders>(block_hash).expect("get header");
        assert!(header.is_some(), "specific block_hash present");
    }

    /// Helper: build a valid snapshot archive from a source dir, returning its path.
    fn build_snapshot(src_dir: &Path, chain_id: u64) -> (tempfile::TempDir, PathBuf) {
        let out = TempDirBuilder::new().build();
        let archive_path = out.path().join("snap.tar.zst");
        run_export(ExportOpts {
            data_dir: src_dir.to_path_buf(),
            output: archive_path.clone(),
            include_caches: false,
            chain_id,
            copy_flags: irys_database::snapshot::CopyFlags {
                compact: true,
                throttle_mvcc: true,
            },
        })
        .expect("export");
        (out, archive_path)
    }

    /// Helper: repack a snapshot archive after injecting an extra unrelated
    /// file into the staging dir. Used to test that the importer refuses to
    /// place files that aren't declared in the manifest.
    fn repack_with_smuggled_file(
        original: &Path,
        extra_rel: &Path,
        extra_bytes: &[u8],
    ) -> (tempfile::TempDir, PathBuf) {
        let staging = TempDirBuilder::new().build();
        // Unpack the original to staging.
        let f = std::fs::File::open(original).expect("open original");
        let dec = zstd::Decoder::new(std::io::BufReader::new(f)).expect("zstd dec");
        let mut tar = tar::Archive::new(dec);
        tar.unpack(staging.path()).expect("unpack original");

        // Inject the smuggled file inside an existing manifest root.
        let smuggled = staging.path().join(extra_rel);
        std::fs::create_dir_all(smuggled.parent().unwrap()).expect("mkdir for smuggled");
        std::fs::write(&smuggled, extra_bytes).expect("write smuggled");

        // Repack — note this archive will still have a valid manifest.json with
        // checksums for the original files; the smuggled file is NOT in it.
        let out_dir = TempDirBuilder::new().build();
        let out_path = out_dir.path().join("smuggled.tar.zst");
        let out_f = std::fs::File::create(&out_path).expect("create out");
        let enc = zstd::Encoder::new(std::io::BufWriter::new(out_f), 3).expect("zstd enc");
        let mut builder = tar::Builder::new(enc);
        builder.follow_symlinks(false);
        builder.append_dir_all(".", staging.path()).expect("append");
        builder.into_inner().unwrap().finish().expect("finish");
        (out_dir, out_path)
    }

    /// CDX-1: an archive that smuggles in `reth/jwt.hex` (NOT in manifest.files)
    /// must not result in that file being placed in the target data dir.
    #[test]
    fn import_does_not_place_smuggled_files() {
        let src_keep = TempDirBuilder::new().build();
        build_source(src_keep.path());
        let (_orig_keep, original) = build_snapshot(src_keep.path(), 4242);

        let (_keep, smuggled_archive) = repack_with_smuggled_file(
            &original,
            Path::new("reth/jwt.hex"),
            b"ATTACKER_CONTROLLED_SECRET",
        );

        let target_keep = TempDirBuilder::new().build();
        let target_dir = target_keep.path().join("victim");

        run_import(ImportOpts {
            input: smuggled_archive,
            data_dir: target_dir.clone(),
            force: false,
            expected_chain_id: 4242,
            expected_irys_schema_version: 3,
        })
        .expect("import succeeds (smuggled file is silently dropped)");

        assert!(
            !target_dir.join("reth/jwt.hex").exists(),
            "smuggled jwt.hex must not appear in target after import"
        );
        // Legitimate manifest-declared files are still placed.
        assert!(
            target_dir.join("reth/db/mdbx.dat").is_file(),
            "legitimate reth mdbx still placed"
        );
    }

    /// CDX-3: importing with `--force` against a data dir that still has the
    /// previous tenant's `storage_modules/` and `irys_submodules.toml` must
    /// remove those non-portable sidecars so the new consensus DB doesn't
    /// conflict with stale packing params on boot.
    #[test]
    fn import_with_force_purges_stale_storage_modules() {
        let src_keep = TempDirBuilder::new().build();
        build_source(src_keep.path());
        let (_keep, archive) = build_snapshot(src_keep.path(), 4243);

        let target_keep = TempDirBuilder::new().build();
        let target_dir = target_keep.path().join("populated");
        std::fs::create_dir_all(target_dir.join("storage_modules/sm0")).expect("mk stale sm");
        std::fs::write(
            target_dir.join("storage_modules/sm0/packing_params.toml"),
            b"# previous tenant",
        )
        .expect("write stale params");
        std::fs::write(target_dir.join("irys_submodules.toml"), b"old = true")
            .expect("write stale submodules");

        run_import(ImportOpts {
            input: archive,
            data_dir: target_dir.clone(),
            force: true,
            expected_chain_id: 4243,
            expected_irys_schema_version: 3,
        })
        .expect("force import succeeds");

        assert!(
            !target_dir.join("storage_modules").exists(),
            "stale storage_modules/ must be purged under --force"
        );
        assert!(
            !target_dir.join("irys_submodules.toml").exists(),
            "stale irys_submodules.toml must be purged under --force"
        );
        assert!(
            target_dir.join("irys_consensus_data/mdbx.dat").is_file(),
            "new consensus DB placed"
        );
    }

    /// CDX-4: a corrupt archive (sanity check fails) must NOT modify the
    /// pre-existing target — sanity runs against staging before placement.
    #[test]
    fn import_aborts_before_modifying_target_on_bad_archive() {
        let src_keep = TempDirBuilder::new().build();
        build_source(src_keep.path());
        let (_keep, archive) = build_snapshot(src_keep.path(), 4244);

        // Corrupt by stamping over the consensus mdbx with garbage post-pack.
        let staging = TempDirBuilder::new().build();
        let f = std::fs::File::open(&archive).expect("open");
        let dec = zstd::Decoder::new(std::io::BufReader::new(f)).expect("dec");
        tar::Archive::new(dec)
            .unpack(staging.path())
            .expect("unpack");
        std::fs::write(
            staging.path().join("irys_consensus_data/mdbx.dat"),
            b"not-a-real-mdbx",
        )
        .expect("corrupt mdbx");

        // Rebuild manifest checksums against the now-corrupted file so checksum
        // validation passes during import; sanity check is the layer under test.
        let files = super::archive::build_file_list(staging.path()).unwrap();
        let mut manifest: super::manifest::SnapshotManifest = serde_json::from_str(
            &std::fs::read_to_string(staging.path().join("manifest.json")).unwrap(),
        )
        .unwrap();
        manifest.files = files;
        std::fs::write(
            staging.path().join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .expect("rewrite manifest");

        // Output the archive into a SEPARATE dir so it isn't picked up by
        // build_file_list above.
        let out_dir = TempDirBuilder::new().build();
        let bad_path = out_dir.path().join("bad.tar.zst");
        let out_f = std::fs::File::create(&bad_path).expect("create");
        let enc = zstd::Encoder::new(std::io::BufWriter::new(out_f), 3).expect("enc");
        let mut builder = tar::Builder::new(enc);
        builder.follow_symlinks(false);
        builder.append_dir_all(".", staging.path()).expect("append");
        builder.into_inner().unwrap().finish().expect("finish");

        let target_keep = TempDirBuilder::new().build();
        let target_dir = target_keep.path().join("preserve_me");
        std::fs::create_dir_all(target_dir.join("storage_modules")).expect("pre-populate target");
        std::fs::write(target_dir.join("storage_modules/keep_me"), b"untouched")
            .expect("write witness");

        let err = run_import(ImportOpts {
            input: bad_path,
            data_dir: target_dir.clone(),
            force: true,
            expected_chain_id: 4244,
            expected_irys_schema_version: 3,
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("sanity opening")
                || err.to_string().contains("no block headers"),
            "expected sanity-check failure, got: {err}"
        );
        assert!(
            target_dir.join("storage_modules/keep_me").is_file(),
            "target must be untouched when archive fails sanity check"
        );
    }

    #[test]
    fn import_rejects_existing_non_empty_data_dir_without_force() {
        let src_keep = TempDirBuilder::new().build();
        let src_dir = src_keep.path().to_path_buf();
        build_source(&src_dir);

        let out_dir = TempDirBuilder::new().build();
        let archive_path = out_dir.path().join("snap.tar.zst");
        run_export(ExportOpts {
            data_dir: src_dir,
            output: archive_path.clone(),
            include_caches: false,
            chain_id: 11,
            copy_flags: irys_database::snapshot::CopyFlags {
                compact: true,
                throttle_mvcc: true,
            },
        })
        .expect("export");

        let target_keep = TempDirBuilder::new().build();
        let target_dir = target_keep.path().join("populated");
        std::fs::create_dir(&target_dir).expect("mk target");
        std::fs::write(target_dir.join("preexisting"), b"x").expect("squat");

        let err = run_import(ImportOpts {
            input: archive_path,
            data_dir: target_dir,
            force: false,
            expected_chain_id: 11,
            expected_irys_schema_version: 3,
        })
        .unwrap_err();
        assert!(err.to_string().contains("not empty"), "got: {err}");
    }
}
