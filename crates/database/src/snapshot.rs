//! MDBX environment snapshotting and node-local table stripping.
//!
//! Used by the snapshot CLI to produce a portable copy of the Irys consensus
//! database with node-identity data removed.

use eyre::Context as _;
use reth_db::Database as _;
use reth_db::DatabaseEnv;
use reth_db::mdbx::ffi;
use reth_db::transaction::{DbTx as _, DbTxMut as _};
use std::ffi::CString;
use std::path::Path;

use crate::tables::{
    CachedChunks, CachedChunksIndex, CachedDataRoots, IngressProofs, PeerListItems,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct CopyFlags {
    /// Reclaim free space; produces a smaller, defragmented copy.
    pub compact: bool,
    /// Yield to writers under MVCC pressure (recommended when copying a live env).
    pub throttle_mvcc: bool,
}

/// Copy an MDBX environment into `dest_dir` via `mdbx_env_copy(2)`.
///
/// The source environment may have active readers/writers; the copy is
/// transactionally consistent at the moment of invocation. `dest_dir` must
/// either not exist (it will be created) or be an existing empty directory;
/// MDBX writes `mdbx.dat` inside it, producing a directory layout that
/// `open_or_create_db` can re-open.
pub fn copy_mdbx_env(src: &DatabaseEnv, dest_dir: &Path, flags: CopyFlags) -> eyre::Result<()> {
    if dest_dir.exists() {
        if !dest_dir.is_dir() {
            eyre::bail!(
                "destination {} exists but is not a directory",
                dest_dir.display()
            );
        }
        let mut entries = std::fs::read_dir(dest_dir)
            .with_context(|| format!("reading destination {}", dest_dir.display()))?;
        if entries.next().is_some() {
            eyre::bail!("destination directory {} is not empty", dest_dir.display());
        }
    } else {
        std::fs::create_dir_all(dest_dir)
            .with_context(|| format!("creating destination dir {}", dest_dir.display()))?;
    }

    // mdbx_env_copy(2) produces a single file. The source env is opened in
    // subdir mode (a directory containing mdbx.dat + lock + sidecars), so we
    // place the copied file inside dest_dir/mdbx.dat to match that layout.
    // The version file + lock are recreated when the copy is later opened
    // via open_or_create_db.
    let mdbx_path = dest_dir.join("mdbx.dat");
    let dest_str = mdbx_path.to_str().ok_or_else(|| {
        eyre::eyre!(
            "destination path must be valid UTF-8: {}",
            mdbx_path.display()
        )
    })?;
    let dest_cstr = CString::new(dest_str)
        .map_err(|_| eyre::eyre!("destination path contains NUL byte: {dest_str}"))?;

    let mut copy_flags: ffi::MDBX_copy_flags_t = ffi::MDBX_CP_DEFAULTS;
    if flags.compact {
        copy_flags |= ffi::MDBX_CP_COMPACT;
    }
    if flags.throttle_mvcc {
        copy_flags |= ffi::MDBX_CP_THROTTLE_MVCC;
    }

    let rc = src.with_raw_env_ptr(|env_ptr| unsafe {
        ffi::mdbx_env_copy(env_ptr, dest_cstr.as_ptr(), copy_flags)
    });
    if rc != 0 {
        let msg = unsafe {
            std::ffi::CStr::from_ptr(ffi::mdbx_strerror(rc))
                .to_string_lossy()
                .into_owned()
        };
        eyre::bail!("mdbx_env_copy failed: rc={rc} {msg}");
    }
    Ok(())
}

/// Clear node-local tables in an opened consensus-DB env.
///
/// `PeerListItems` is always cleared. Cache tables (`CachedDataRoots`,
/// `CachedChunksIndex`, `CachedChunks`, `IngressProofs`) are cleared when
/// `include_caches` is false.
pub fn strip_node_local(env: &DatabaseEnv, include_caches: bool) -> eyre::Result<()> {
    let tx = env
        .tx_mut()
        .context("opening write tx for snapshot strip")?;
    tx.clear::<PeerListItems>()
        .context("clearing PeerListItems")?;
    if !include_caches {
        tx.clear::<CachedDataRoots>()
            .context("clearing CachedDataRoots")?;
        tx.clear::<CachedChunksIndex>()
            .context("clearing CachedChunksIndex")?;
        tx.clear::<CachedChunks>()
            .context("clearing CachedChunks")?;
        tx.clear::<IngressProofs>()
            .context("clearing IngressProofs")?;
    }
    tx.commit().context("committing snapshot strip")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::IrysTables;
    use crate::{
        IrysDatabaseArgs as _, insert_block_header, insert_peer_list_item, open_or_create_db,
    };
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{
        IrysAddress, IrysBlockHeader, IrysPeerId, PeerAddress, PeerListItem, PeerScore,
    };
    use reth_db::cursor::DbCursorRO as _;
    use reth_db::mdbx::DatabaseArguments;
    use rstest::rstest;

    fn make_peer_list_item(byte: u8) -> (IrysPeerId, PeerListItem) {
        let addr = IrysAddress::repeat_byte(byte);
        let peer_id = IrysPeerId::from(addr);
        let item = PeerListItem {
            peer_id,
            mining_address: addr,
            reputation_score: PeerScore::new(50),
            response_time: 10,
            address: PeerAddress {
                gossip: "127.0.0.1:1".parse().expect("valid socket addr"),
                api: "127.0.0.1:2".parse().expect("valid socket addr"),
                execution: Default::default(),
            },
            last_seen: 0,
            is_online: true,
            protocol_version: Default::default(),
        };
        (peer_id, item)
    }

    fn open_fixture() -> (tempfile::TempDir, DatabaseEnv) {
        let dir = TempDirBuilder::new().build();
        let db = open_or_create_db(
            dir.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().expect("testing args"),
        )
        .expect("open fixture db");
        (dir, db)
    }

    fn count_peers(env: &DatabaseEnv) -> usize {
        let tx = env.tx().expect("open ro tx");
        let mut cursor = tx.cursor_read::<PeerListItems>().expect("open peer cursor");
        let mut count = 0;
        while cursor.next().expect("walk peers").is_some() {
            count += 1;
        }
        count
    }

    fn block_present(env: &DatabaseEnv, block_hash: &irys_types::H256) -> bool {
        let tx = env.tx().expect("open ro tx");
        tx.get::<crate::tables::IrysBlockHeaders>(*block_hash)
            .expect("get header")
            .is_some()
    }

    fn populate_source(env: &DatabaseEnv) -> irys_types::H256 {
        let mut block = IrysBlockHeader::new_mock_header();
        block.block_hash.0[0] = 0x42;
        env.update(|tx| insert_block_header(tx, &block))
            .expect("insert block")
            .expect("insert block ok");

        let (peer_a, item_a) = make_peer_list_item(0xA0);
        let (peer_b, item_b) = make_peer_list_item(0xB0);
        env.update(|tx| {
            insert_peer_list_item(tx, &peer_a, &item_a)?;
            insert_peer_list_item(tx, &peer_b, &item_b)?;
            Ok::<(), eyre::Report>(())
        })
        .expect("insert peers")
        .expect("insert peers ok");

        block.block_hash
    }

    #[rstest]
    #[case::strip_caches(false)]
    #[case::keep_caches(true)]
    fn copy_and_strip_round_trip(#[case] include_caches: bool) {
        let (_src_dir, src_db) = open_fixture();
        let block_hash = populate_source(&src_db);
        assert_eq!(count_peers(&src_db), 2);

        let dest_dir = TempDirBuilder::new().build();
        let dest_path = dest_dir.path().join("copy.mdbx");

        copy_mdbx_env(
            &src_db,
            &dest_path,
            CopyFlags {
                compact: true,
                throttle_mvcc: true,
            },
        )
        .expect("copy succeeds");

        let copy_db = open_or_create_db(
            &dest_path,
            IrysTables::ALL,
            DatabaseArguments::irys_testing().expect("testing args"),
        )
        .expect("open copied db");

        assert_eq!(count_peers(&copy_db), 2, "peers carried into the copy");
        assert!(block_present(&copy_db, &block_hash));

        strip_node_local(&copy_db, include_caches).expect("strip succeeds");

        assert_eq!(count_peers(&copy_db), 0, "peer table cleared after strip");
        assert!(
            block_present(&copy_db, &block_hash),
            "block headers survive strip"
        );

        assert_eq!(count_peers(&src_db), 2, "source db left untouched");
        assert!(block_present(&src_db, &block_hash));
    }

    #[test]
    fn copy_fails_when_destination_is_a_non_empty_dir() {
        let (_src_dir, src_db) = open_fixture();
        let dest_dir = TempDirBuilder::new().build();
        let dest_path = dest_dir.path().join("preexisting");
        std::fs::create_dir(&dest_path).expect("create dest");
        std::fs::write(dest_path.join("squatter"), b"x").expect("write squatter");

        let err = copy_mdbx_env(&src_db, &dest_path, CopyFlags::default()).unwrap_err();
        assert!(
            err.to_string().contains("not empty"),
            "expected not-empty error, got: {err}"
        );
    }

    #[test]
    fn copy_fails_when_destination_is_a_file() {
        let (_src_dir, src_db) = open_fixture();
        let dest_dir = TempDirBuilder::new().build();
        let dest_path = dest_dir.path().join("not-a-dir");
        std::fs::write(&dest_path, b"x").expect("write file");

        let err = copy_mdbx_env(&src_db, &dest_path, CopyFlags::default()).unwrap_err();
        assert!(
            err.to_string().contains("not a directory"),
            "expected not-a-directory error, got: {err}"
        );
    }
}
