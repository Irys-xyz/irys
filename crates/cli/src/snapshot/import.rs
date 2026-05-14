use eyre::Context as _;
use irys_database::reth_db::cursor::DbCursorRO as _;
use irys_database::reth_db::mdbx::DatabaseArguments;
use irys_database::reth_db::transaction::DbTx as _;
use irys_database::reth_db::{Database as _, DatabaseEnv, DatabaseEnvKind};
use irys_database::tables::IrysBlockHeaders;
use reth_node_core::version::default_client_version;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use super::archive;
use super::manifest::{MANIFEST_FILENAME, ManifestFile, SNAPSHOT_FORMAT_VERSION, SnapshotManifest};
use super::{IRYS_CONSENSUS_SUBDIR, STORAGE_MODULES_SUBDIR, SUBMODULES_FILE};

#[derive(Debug, Clone)]
pub(crate) struct ImportOpts {
    pub input: PathBuf,
    pub data_dir: PathBuf,
    pub force: bool,
    pub expected_chain_id: u64,
    pub expected_irys_schema_version: u32,
}

/// Drive a snapshot import: verify, extract, checksum, sanity-check, then place into `data_dir`.
///
/// The imported node still needs its own `config.toml` with a fresh mining key
/// and node-local sidecars (the snapshot intentionally omits identity data).
/// Storage modules must be re-packed by the node on first boot.
///
/// Security ordering:
/// 1. Read root manifest, validate format/chain/schema before doing any I/O.
/// 2. Extract to a temp staging dir.
/// 3. Verify SHA-256 of every manifest-declared file.
/// 4. Refuse any extracted entry not declared in the manifest (closes the
///    "smuggle `jwt.hex` inside a valid archive" attack).
/// 5. Sanity-open the staged consensus DB. A bad archive fails here and the
///    target is never touched.
/// 6. Under `--force`, also purge node-local packing state that the snapshot
///    intentionally omits (`storage_modules/`, `irys_submodules.toml`) — these
///    are bound to the previous tenant's mining address and would conflict
///    with the new consensus DB on boot.
/// 7. Place each manifest-declared file into the target.
pub(crate) fn run_import(opts: ImportOpts) -> eyre::Result<()> {
    let manifest = archive::read_manifest_from_archive(&opts.input)?;
    verify_compatibility(&manifest, &opts)?;
    verify_data_dir_state(&opts)?;

    let staging =
        tempfile::TempDir::new().context("creating snapshot extraction staging directory")?;
    let extracted = archive::unpack(&opts.input, staging.path())?;
    verify_checksums(staging.path(), &extracted.files)?;

    let allowed_roots = manifest_root_entries(&extracted.files);
    verify_no_extra_entries(staging.path(), &allowed_roots)?;
    sanity_check_consensus_db(staging.path())?;

    std::fs::create_dir_all(&opts.data_dir)
        .with_context(|| format!("creating data dir {}", opts.data_dir.display()))?;
    if opts.force {
        purge_node_local_state(&opts.data_dir, &allowed_roots)?;
    }
    place_manifest_files(staging.path(), &opts.data_dir, &extracted.files, opts.force)?;
    Ok(())
}

/// Open the consensus mdbx env RO and walk one block header. A truncated
/// `mdbx.dat` fails to open here; an empty DB fails the header check. Run
/// against the staging dir BEFORE placement so bad archives never overwrite
/// the target.
fn sanity_check_consensus_db(root: &Path) -> eyre::Result<()> {
    let consensus_dir = root.join(IRYS_CONSENSUS_SUBDIR);
    if !consensus_dir.join("mdbx.dat").is_file() {
        eyre::bail!(
            "snapshot consensus DB is missing mdbx.dat at {}",
            consensus_dir.display()
        );
    }
    let env = DatabaseEnv::open(
        &consensus_dir,
        DatabaseEnvKind::RO,
        DatabaseArguments::new(default_client_version())
            .with_log_level(None)
            .with_exclusive(Some(false)),
    )
    .with_context(|| {
        format!(
            "sanity opening snapshot consensus DB at {}",
            consensus_dir.display()
        )
    })?;
    let tx = env
        .tx()
        .context("starting RO tx on snapshot consensus DB")?;
    let mut cursor = tx
        .cursor_read::<IrysBlockHeaders>()
        .context("opening IrysBlockHeaders cursor for sanity check")?;
    if cursor
        .first()
        .context("walking IrysBlockHeaders for sanity check")?
        .is_none()
    {
        eyre::bail!(
            "snapshot consensus DB at {} has no block headers — appears empty or corrupt",
            consensus_dir.display()
        );
    }
    drop(tx);
    drop(env);
    Ok(())
}

/// Top-level components of every manifest-declared file path. A manifest entry
/// of `irys_consensus_data/mdbx.dat` yields root `irys_consensus_data`. Used
/// to validate that nothing extra was extracted and to know which target dirs
/// to clear before placement.
fn manifest_root_entries(files: &[ManifestFile]) -> BTreeSet<OsString> {
    files
        .iter()
        .filter_map(|f| match Path::new(&f.path).components().next()? {
            Component::Normal(s) => Some(s.to_os_string()),
            _ => None,
        })
        .collect()
}

/// Every top-level entry in `staging` must be either the manifest or one of
/// the manifest-declared roots. Crafted archives smuggling in extra files
/// (e.g., `reth/jwt.hex`) are rejected here.
fn verify_no_extra_entries(staging: &Path, allowed_roots: &BTreeSet<OsString>) -> eyre::Result<()> {
    for entry in
        std::fs::read_dir(staging).with_context(|| format!("reading {}", staging.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        if name == MANIFEST_FILENAME {
            continue;
        }
        if !allowed_roots.contains(&name) {
            eyre::bail!(
                "archive contains unexpected top-level entry {:?} not declared in manifest",
                name
            );
        }
    }
    Ok(())
}

/// Under `--force`, remove any pre-existing manifest roots from the target
/// (so the new files don't merge with stale ones) plus the explicit
/// non-portable state that the snapshot omits by design.
fn purge_node_local_state(target: &Path, allowed_roots: &BTreeSet<OsString>) -> eyre::Result<()> {
    for root in allowed_roots {
        remove_if_exists(&target.join(root))?;
    }
    remove_if_exists(&target.join(STORAGE_MODULES_SUBDIR))?;
    remove_if_exists(&target.join(SUBMODULES_FILE))?;
    Ok(())
}

fn remove_if_exists(path: &Path) -> eyre::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(eyre::Report::new(e).wrap_err(format!("stat {}", path.display()))),
    };
    if metadata.is_dir() {
        std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))?;
    } else {
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

/// Place every manifest-declared file from staging into the target, creating
/// parent directories as needed. Extracted entries NOT in the manifest are
/// ignored — refusing to place them is what makes the smuggling attack inert.
fn place_manifest_files(
    staging: &Path,
    target: &Path,
    files: &[ManifestFile],
    force: bool,
) -> eyre::Result<()> {
    for entry in files {
        let src = staging.join(&entry.path);
        let dst = target.join(&entry.path);
        if dst.exists() {
            if !force {
                eyre::bail!(
                    "target file {} already exists (use --force to overwrite)",
                    dst.display()
                );
            }
            std::fs::remove_file(&dst)
                .with_context(|| format!("removing existing {}", dst.display()))?;
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent {}", parent.display()))?;
        }
        // rename first (fast, atomic on same fs); fall back to copy when the
        // target is on a different filesystem.
        if std::fs::rename(&src, &dst).is_err() {
            std::fs::copy(&src, &dst)
                .with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
        }
    }
    Ok(())
}

fn verify_data_dir_state(opts: &ImportOpts) -> eyre::Result<()> {
    if !opts.data_dir.exists() {
        return Ok(());
    }
    let mut iter = std::fs::read_dir(&opts.data_dir)
        .with_context(|| format!("reading target data dir {}", opts.data_dir.display()))?;
    if iter.next().is_some() && !opts.force {
        eyre::bail!(
            "target data dir {} is not empty — pass --force to overwrite",
            opts.data_dir.display()
        );
    }
    Ok(())
}

fn verify_checksums(staging: &std::path::Path, expected: &[ManifestFile]) -> eyre::Result<()> {
    use sha2::{Digest as _, Sha256};
    use std::io::Read as _;

    let mut buf = vec![0_u8; 64 * 1024];
    for entry in expected {
        let path = staging.join(&entry.path);
        let mut reader = std::io::BufReader::new(
            std::fs::File::open(&path)
                .with_context(|| format!("opening extracted file {}", path.display()))?,
        );
        let mut hasher = Sha256::new();
        let mut total: u64 = 0;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            total = total
                .checked_add(u64::try_from(n).expect("read len fits in u64"))
                .ok_or_else(|| eyre::eyre!("file size overflow on {}", path.display()))?;
        }
        let actual = hex::encode(hasher.finalize());
        if actual != entry.sha256_hex {
            eyre::bail!(
                "checksum mismatch for {}: manifest={}, actual={}",
                entry.path,
                entry.sha256_hex,
                actual
            );
        }
        if total != entry.size_bytes {
            eyre::bail!(
                "size mismatch for {}: manifest={}, actual={}",
                entry.path,
                entry.size_bytes,
                total
            );
        }
    }
    Ok(())
}

fn verify_compatibility(manifest: &SnapshotManifest, opts: &ImportOpts) -> eyre::Result<()> {
    if manifest.format_version != SNAPSHOT_FORMAT_VERSION {
        eyre::bail!(
            "snapshot format_version {} is incompatible with this binary (expects {})",
            manifest.format_version,
            SNAPSHOT_FORMAT_VERSION
        );
    }
    if manifest.chain_id != opts.expected_chain_id {
        eyre::bail!(
            "snapshot chain_id {} does not match local chain_id {}",
            manifest.chain_id,
            opts.expected_chain_id
        );
    }
    if manifest.irys_schema_version != opts.expected_irys_schema_version && !opts.force {
        eyre::bail!(
            "snapshot irys_schema_version {} does not match local {} — pass --force to override",
            manifest.irys_schema_version,
            opts.expected_irys_schema_version
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::archive::pack;
    use crate::snapshot::manifest::{MANIFEST_FILENAME, ManifestFile};
    use irys_database::tables::IrysTables;
    use irys_database::{IrysDatabaseArgs as _, open_or_create_db};
    use irys_testing_utils::utils::TempDirBuilder;
    use rstest::rstest;

    #[derive(Debug, Clone, Copy)]
    enum BadConsensus {
        MissingMdbx,
        CorruptMdbx,
        EmptyDb,
    }

    fn setup_bad_consensus(kind: BadConsensus) -> tempfile::TempDir {
        let dir = TempDirBuilder::new().build();
        let consensus_dir = dir.path().join(IRYS_CONSENSUS_SUBDIR);
        std::fs::create_dir_all(&consensus_dir).expect("create consensus dir");
        match kind {
            BadConsensus::MissingMdbx => {}
            BadConsensus::CorruptMdbx => {
                std::fs::write(consensus_dir.join("mdbx.dat"), b"not-a-real-mdbx")
                    .expect("write garbage mdbx");
            }
            BadConsensus::EmptyDb => {
                let env = open_or_create_db(
                    &consensus_dir,
                    IrysTables::ALL,
                    DatabaseArguments::irys_testing().expect("testing args"),
                )
                .expect("open empty db");
                drop(env);
            }
        }
        dir
    }

    #[rstest]
    #[case::missing_mdbx(BadConsensus::MissingMdbx, "missing mdbx.dat")]
    #[case::corrupt_mdbx(BadConsensus::CorruptMdbx, "sanity opening")]
    #[case::empty_db(BadConsensus::EmptyDb, "no block headers")]
    fn sanity_check_rejects_invalid_consensus_db(
        #[case] kind: BadConsensus,
        #[case] expected_msg: &str,
    ) {
        let dir = setup_bad_consensus(kind);
        let err = sanity_check_consensus_db(dir.path()).unwrap_err();
        assert!(
            err.to_string().contains(expected_msg),
            "expected `{expected_msg}` in error, got: {err}"
        );
    }

    fn make_archive(manifest: &SnapshotManifest) -> (tempfile::TempDir, PathBuf) {
        let dir = TempDirBuilder::new().build();
        let staging = TempDirBuilder::new().build();
        std::fs::write(
            staging.path().join(MANIFEST_FILENAME),
            serde_json::to_string_pretty(manifest).unwrap(),
        )
        .unwrap();
        let archive_path = dir.path().join("snap.tar.zst");
        pack(staging.path(), &archive_path).unwrap();
        (dir, archive_path)
    }

    fn manifest_with(chain_id: u64, schema: u32, format: u32) -> SnapshotManifest {
        SnapshotManifest {
            format_version: format,
            chain_id,
            irys_schema_version: schema,
            irys_tip_height: None,
            reth_tip_height: None,
            includes_caches: false,
            created_at_unix_secs: 0,
            created_by: "test".to_owned(),
            files: Vec::<ManifestFile>::new(),
        }
    }

    #[test]
    fn rejects_chain_id_mismatch() {
        let manifest = manifest_with(1, 3, SNAPSHOT_FORMAT_VERSION);
        let (_dir, archive_path) = make_archive(&manifest);
        let opts = ImportOpts {
            input: archive_path,
            data_dir: PathBuf::from("/tmp/unused"),
            force: false,
            expected_chain_id: 2,
            expected_irys_schema_version: 3,
        };
        let err = run_import(opts).unwrap_err();
        assert!(err.to_string().contains("chain_id"), "got: {err}");
    }

    #[test]
    fn rejects_format_version_mismatch() {
        let manifest = manifest_with(1, 3, SNAPSHOT_FORMAT_VERSION + 99);
        let (_dir, archive_path) = make_archive(&manifest);
        let opts = ImportOpts {
            input: archive_path,
            data_dir: PathBuf::from("/tmp/unused"),
            force: false,
            expected_chain_id: 1,
            expected_irys_schema_version: 3,
        };
        let err = run_import(opts).unwrap_err();
        assert!(err.to_string().contains("format_version"), "got: {err}");
    }

    #[test]
    fn rejects_schema_mismatch_without_force() {
        let manifest = manifest_with(1, 2, SNAPSHOT_FORMAT_VERSION);
        let (_dir, archive_path) = make_archive(&manifest);
        let opts = ImportOpts {
            input: archive_path,
            data_dir: PathBuf::from("/tmp/unused"),
            force: false,
            expected_chain_id: 1,
            expected_irys_schema_version: 3,
        };
        let err = run_import(opts).unwrap_err();
        assert!(err.to_string().contains("schema_version"), "got: {err}");
    }
}
