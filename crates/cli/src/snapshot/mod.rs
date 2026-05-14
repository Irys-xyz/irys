//! Snapshot export and import for Irys node state.
//!
//! Captures the chain-wide consensus and EVM state of a node so a different
//! node can be bootstrapped from the capture. Node-specific identity (signing
//! keys, peer reputation, partition packing) is excluded by design.

pub(crate) mod archive;
pub(crate) mod export;
pub(crate) mod import;
pub(crate) mod manifest;

#[cfg(test)]
mod round_trip_tests {
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
        let irys_path = data_dir.join("irys_consensus_data");
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
            irys_schema_version: 3,
            irys_tip_height: None,
            reth_tip_height: None,
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
            target_dir.join("irys_consensus_data"),
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
            irys_schema_version: 3,
            irys_tip_height: None,
            reth_tip_height: None,
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
