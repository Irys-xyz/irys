//! Reth execution-layer snapshotting.
//!
//! Produces a portable copy of the chain-wide Reth state from a stopped or
//! running data directory. The output layout mirrors the input layout for
//! `db/` and `static_files/` so a fresh Reth instance can pick it up by
//! dropping the snapshot into place.
//!
//! In this build, the persistent rocksdb history dir is unused
//! (`RocksDBProvider` is a zero-sized stub — see `crates/cli/src/db_utils.rs`).
//! Only `db/` (mdbx) and `static_files/` are required for bootstrap.
//!
//! Excluded as node-local:
//! * `blobstore/` — mempool blob cache, re-downloaded on sync
//! * `discovery-secret`, `jwt.hex`, `known-peers.json` — node network identity
//! * `reth.toml` — node-specific config
//! * `invalid_block_hooks/` — debug artifacts
//! * `logs/` — runtime logs
//! * `rocksdb/` — unused in this build

use eyre::Context as _;
use irys_database::snapshot::{CopyFlags, copy_mdbx_env};
use reth_db::mdbx::DatabaseArguments;
use reth_db::transaction::DbTx as _;
use reth_db::{Database as _, DatabaseEnv, DatabaseEnvKind, DatabaseError, StageCheckpoints};
use reth_node_core::version::default_client_version;
use std::path::Path;

pub const RETH_DB_DIR: &str = "db";
pub const RETH_STATIC_FILES_DIR: &str = "static_files";
/// Stage ID whose checkpoint marks the highest fully-processed Reth block.
/// Mirrors `reth_stages_types::StageId::Finish.as_ref()`, hard-coded here to
/// avoid pulling another crate just for one constant.
const RETH_FINISH_STAGE: &str = "Finish";
/// MDBX error code returned by `mdbx_dbi_open` when the named subdb has never
/// been created — e.g., reading from `StageCheckpoints` in a DB that has not
/// yet completed any stages. Treated as "no checkpoint" rather than an error.
const MDBX_NOTFOUND: i32 = -30798;

/// Snapshot the chain-wide Reth state under `src_reth_dir` into `dest_reth_dir`.
///
/// `src_reth_dir` is the `.irys/reth/` directory of a node (stopped or running).
/// `dest_reth_dir` is a fresh (empty or non-existent) destination. On success,
/// `dest_reth_dir` will contain:
///
/// ```text
/// dest_reth_dir/
///   db/mdbx.dat
///   static_files/<segments>
/// ```
///
/// Returns the highest fully-processed Reth block number read from the source
/// `StageCheckpoints` table at copy time. `None` if the source has not yet
/// completed any blocks (e.g., a freshly initialized node).
pub fn snapshot_reth_state(
    src_reth_dir: &Path,
    dest_reth_dir: &Path,
    flags: CopyFlags,
) -> eyre::Result<Option<u64>> {
    let src_db = src_reth_dir.join(RETH_DB_DIR);
    if !src_db.is_dir() {
        eyre::bail!(
            "expected reth db at {} (is the path correct?)",
            src_db.display()
        );
    }

    if dest_reth_dir.exists() {
        let mut entries = std::fs::read_dir(dest_reth_dir)
            .with_context(|| format!("reading destination {}", dest_reth_dir.display()))?;
        if entries.next().is_some() {
            eyre::bail!("destination {} is not empty", dest_reth_dir.display());
        }
    } else {
        std::fs::create_dir_all(dest_reth_dir)
            .with_context(|| format!("creating dest {}", dest_reth_dir.display()))?;
    }

    let db = open_reth_db_read_only(&src_db)?;
    let tip = read_finish_checkpoint(&db).context("reading Reth Finish stage checkpoint")?;
    let dest_db = dest_reth_dir.join(RETH_DB_DIR);
    copy_mdbx_env(&db, &dest_db, flags)
        .with_context(|| format!("copying reth mdbx to {}", dest_db.display()))?;

    let src_static = src_reth_dir.join(RETH_STATIC_FILES_DIR);
    if src_static.is_dir() {
        let dest_static = dest_reth_dir.join(RETH_STATIC_FILES_DIR);
        copy_dir_recursive(&src_static, &dest_static).with_context(|| {
            format!(
                "copying static_files from {} to {}",
                src_static.display(),
                dest_static.display()
            )
        })?;
    }

    Ok(tip)
}

fn read_finish_checkpoint(db: &DatabaseEnv) -> eyre::Result<Option<u64>> {
    let tx = db.tx().context("opening RO tx for stage checkpoint")?;
    match tx.get::<StageCheckpoints>(RETH_FINISH_STAGE.to_owned()) {
        Ok(cp) => Ok(cp.map(|c| c.block_number)),
        Err(DatabaseError::Open(info)) if info.code == MDBX_NOTFOUND => Ok(None),
        Err(e) => Err(eyre::Report::new(e).wrap_err("reading StageCheckpoints[Finish]")),
    }
}

fn open_reth_db_read_only(db_path: &Path) -> eyre::Result<DatabaseEnv> {
    DatabaseEnv::open(
        db_path,
        DatabaseEnvKind::RO,
        DatabaseArguments::new(default_client_version())
            .with_log_level(None)
            .with_exclusive(Some(false)),
    )
    .with_context(|| format!("opening reth db at {}", db_path.display()))
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> eyre::Result<()> {
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let entry_path = entry.path();
        let file_type = entry.file_type()?;
        let dest_path = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry_path, &dest_path)?;
        } else if file_type.is_file() {
            std::fs::copy(&entry_path, &dest_path).with_context(|| {
                format!(
                    "copying {} to {}",
                    entry_path.display(),
                    dest_path.display()
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_testing_utils::utils::TempDirBuilder;
    use reth_db::mdbx::DatabaseArguments;
    use reth_db::transaction::DbTxMut as _;
    use reth_stages_types::StageCheckpoint;

    fn write_static_file(dir: &Path, name: &str, content: &[u8]) {
        std::fs::write(dir.join(name), content).expect("write static file");
    }

    fn open_reth_db_rw(path: &Path) -> DatabaseEnv {
        DatabaseEnv::open(
            path,
            DatabaseEnvKind::RW,
            DatabaseArguments::new(default_client_version())
                .with_log_level(None)
                .with_exclusive(Some(false)),
        )
        .expect("open rw")
    }

    fn populate_reth_db(env: &DatabaseEnv) {
        env.update(|_tx| Ok::<(), eyre::Report>(()))
            .expect("rw txn outer")
            .expect("rw txn inner");
    }

    /// Open RW, create the Reth table schema, write a Finish checkpoint, drop.
    /// Mirrors the on-disk layout Reth's `init_db` would produce so that
    /// `read_finish_checkpoint` finds a real subdb to read from.
    fn write_finish_checkpoint(src_db: &Path, block_number: u64) {
        let mut env = open_reth_db_rw(src_db);
        env.create_tables().expect("create tables");
        env.update(|tx| {
            tx.put::<StageCheckpoints>(
                RETH_FINISH_STAGE.to_owned(),
                StageCheckpoint::new(block_number),
            )
        })
        .expect("put checkpoint outer")
        .expect("put checkpoint inner");
    }

    #[test]
    fn snapshot_copies_db_and_static_files() {
        let src_dir = TempDirBuilder::new().build();
        let src_reth = src_dir.path().join("reth");
        let src_db = src_reth.join("db");
        let src_static = src_reth.join("static_files");
        std::fs::create_dir_all(&src_db).expect("create src db");
        std::fs::create_dir_all(&src_static).expect("create src static");

        let env = open_reth_db_rw(&src_db);
        populate_reth_db(&env);
        drop(env);
        write_finish_checkpoint(&src_db, 4_242);

        // Static file fixtures of different sizes so the copy walks more than one entry.
        write_static_file(&src_static, "headers.jf", b"some-headers-payload");
        write_static_file(&src_static, "headers.idx", b"idx");
        std::fs::create_dir(src_static.join("sub")).expect("create sub");
        write_static_file(&src_static.join("sub"), "deep.dat", b"\x00\x01\x02");

        // Add a node-local sidecar that the snapshot must NOT copy.
        std::fs::write(src_reth.join("jwt.hex"), b"secret").expect("write jwt");
        std::fs::write(src_reth.join("discovery-secret"), b"sec").expect("write disc");

        let dest_dir = TempDirBuilder::new().build();
        let dest_reth = dest_dir.path().join("dest-reth");

        let tip =
            snapshot_reth_state(&src_reth, &dest_reth, CopyFlags::default()).expect("snapshot");
        assert_eq!(tip, Some(4_242), "Finish checkpoint surfaced");

        assert!(dest_reth.join("db/mdbx.dat").is_file(), "mdbx.dat copied");
        assert_eq!(
            std::fs::read(dest_reth.join("static_files/headers.jf")).expect("read"),
            b"some-headers-payload"
        );
        assert_eq!(
            std::fs::read(dest_reth.join("static_files/headers.idx")).expect("read"),
            b"idx"
        );
        assert_eq!(
            std::fs::read(dest_reth.join("static_files/sub/deep.dat")).expect("read"),
            b"\x00\x01\x02"
        );
        assert!(
            !dest_reth.join("jwt.hex").exists(),
            "node-local jwt.hex must NOT be copied"
        );
        assert!(
            !dest_reth.join("discovery-secret").exists(),
            "node-local discovery-secret must NOT be copied"
        );
    }

    #[test]
    fn snapshot_fails_when_src_db_missing() {
        let src_dir = TempDirBuilder::new().build();
        let dest_dir = TempDirBuilder::new().build();
        let err = snapshot_reth_state(
            src_dir.path(),
            &dest_dir.path().join("d"),
            CopyFlags::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("expected reth db"), "got: {err}");
    }

    #[test]
    fn snapshot_returns_none_tip_when_finish_checkpoint_absent() {
        let src_dir = TempDirBuilder::new().build();
        let src_reth = src_dir.path().join("reth");
        let src_db = src_reth.join("db");
        std::fs::create_dir_all(&src_db).expect("create src db");
        let env = open_reth_db_rw(&src_db);
        populate_reth_db(&env);
        drop(env);

        let dest_dir = TempDirBuilder::new().build();
        let dest_reth = dest_dir.path().join("dest-reth");

        let tip =
            snapshot_reth_state(&src_reth, &dest_reth, CopyFlags::default()).expect("snapshot");
        assert_eq!(
            tip, None,
            "fresh DB without Finish checkpoint reports no tip"
        );
    }

    #[test]
    fn snapshot_fails_when_dest_not_empty() {
        let src_dir = TempDirBuilder::new().build();
        let src_db = src_dir.path().join("db");
        std::fs::create_dir(&src_db).expect("create db dir");
        let _env = open_reth_db_rw(&src_db);

        let dest_dir = TempDirBuilder::new().build();
        std::fs::write(dest_dir.path().join("squatter"), b"x").expect("write squatter");

        let err =
            snapshot_reth_state(src_dir.path(), dest_dir.path(), CopyFlags::default()).unwrap_err();
        assert!(err.to_string().contains("not empty"), "got: {err}");
    }
}
