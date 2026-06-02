use eyre::Context as _;
use irys_database::reth_db::cursor::DbCursorRO as _;
use irys_database::reth_db::mdbx::DatabaseArguments;
use irys_database::reth_db::transaction::DbTx as _;
use irys_database::reth_db::{Database as _, DatabaseEnv, DatabaseEnvKind};
use irys_database::tables::IrysBlockHeaders;
use irys_reth_node_bridge::snapshot::RETH_DB_DIR;
use reth_node_core::version::default_client_version;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use super::archive;
use super::manifest::{MANIFEST_FILENAME, ManifestFile, SNAPSHOT_FORMAT_VERSION, SnapshotManifest};
use super::{IRYS_CONSENSUS_SUBDIR, RETH_SUBDIR, STORAGE_MODULES_SUBDIR, SUBMODULES_FILE};

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
    // Validate before any extraction. `read_manifest_from_archive` rejects
    // duplicate root manifests, so this preflight manifest is byte-identical
    // to the one `unpack()` leaves on disk — validating its paths here
    // protects every later `Path::join` on `extracted.files`.
    validate_manifest_paths(&manifest.files)?;
    verify_data_dir_state(&opts)?;

    let staging =
        tempfile::TempDir::new().context("creating snapshot extraction staging directory")?;
    let extracted = archive::unpack(&opts.input, staging.path())?;
    verify_checksums(staging.path(), &extracted.files)?;

    let allowed_roots = manifest_root_entries(&extracted.files);
    verify_no_extra_entries(staging.path(), &extracted.files)?;
    sanity_check_consensus_db(staging.path())?;
    sanity_check_reth_db(staging.path())?;

    std::fs::create_dir_all(&opts.data_dir)
        .with_context(|| format!("creating data dir {}", opts.data_dir.display()))?;
    // NOTE: under `--force` the purge-then-place sequence is not atomic — a
    // mid-import failure (ENOSPC, cross-device) leaves the target partly
    // purged and partly populated. An atomic sibling-staging swap is a
    // multi-day structural change tracked as out-of-scope finding #3 in
    // claude/2026-05-14-002; operators are warned to keep a backup before
    // `--force` import.
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

/// The staged Reth payload must contain an openable execution DB. Without this
/// a repacked archive that drops `reth/` (or ships a corrupt-but-checksummed
/// Reth DB) would import "successfully" and pair the snapshot's consensus
/// state with the importing node's stale execution state. Run against staging
/// BEFORE placement, mirroring `sanity_check_consensus_db`. A valid archive
/// therefore always declares `reth/` in its manifest, so `--force` purges any
/// stale `reth/` via `purge_node_local_state`.
fn sanity_check_reth_db(root: &Path) -> eyre::Result<()> {
    let reth_db_dir = root.join(RETH_SUBDIR).join(RETH_DB_DIR);
    if !reth_db_dir.join("mdbx.dat").is_file() {
        eyre::bail!(
            "snapshot Reth DB is missing mdbx.dat at {}",
            reth_db_dir.display()
        );
    }
    let env = DatabaseEnv::open(
        &reth_db_dir,
        DatabaseEnvKind::RO,
        DatabaseArguments::new(default_client_version())
            .with_log_level(None)
            .with_exclusive(Some(false)),
    )
    .with_context(|| {
        format!(
            "sanity opening snapshot Reth DB at {}",
            reth_db_dir.display()
        )
    })?;
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

/// Every file in the extracted `staging` tree must be either `manifest.json`
/// or one of the manifest-declared paths. Crafted archives smuggling in extra
/// files at any depth (e.g., `reth/jwt.hex`, `irys_consensus_data/extra`) are
/// rejected here. Symlinks, devices, and other non-regular entries are also
/// rejected at the tree level so they never reach checksum or placement.
fn verify_no_extra_entries(staging: &Path, declared: &[ManifestFile]) -> eyre::Result<()> {
    let allowed: BTreeSet<&str> = declared.iter().map(|f| f.path.as_str()).collect();
    walk_and_verify(staging, staging, &allowed)
}

fn walk_and_verify(root: &Path, dir: &Path, allowed: &BTreeSet<&str>) -> eyre::Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk_and_verify(root, &path, allowed)?;
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .expect("path is under root")
            .to_string_lossy()
            .replace('\\', "/");
        if !file_type.is_file() {
            eyre::bail!(
                "archive contains non-regular entry {rel:?} (symlinks and special files are rejected)"
            );
        }
        if rel == MANIFEST_FILENAME {
            continue;
        }
        if !allowed.contains(rel.as_str()) {
            eyre::bail!("archive contains undeclared file {rel:?} not present in manifest");
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
        ensure_regular_file(&src)?;
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

/// Reject anything that is not a regular file before hashing or moving it.
/// Uses non-following `symlink_metadata` so a crafted archive cannot smuggle a
/// symlink (or device/dir) as a manifest entry — `verify_checksums` would
/// otherwise hash the link target and `place_manifest_files` would install the
/// link into `data_dir`, escaping the snapshot boundary. The export side
/// already refuses symlinks (`copy_dir_recursive`), so legitimate archives
/// never contain them.
fn ensure_regular_file(path: &Path) -> eyre::Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("stat extracted file {}", path.display()))?;
    if !meta.file_type().is_file() {
        eyre::bail!(
            "manifest entry {} is not a regular file (symlinks and special files are rejected)",
            path.display()
        );
    }
    Ok(())
}

fn verify_checksums(staging: &std::path::Path, expected: &[ManifestFile]) -> eyre::Result<()> {
    for entry in expected {
        let path = staging.join(&entry.path);
        ensure_regular_file(&path)?;
        let reader = std::io::BufReader::new(
            std::fs::File::open(&path)
                .with_context(|| format!("opening extracted file {}", path.display()))?,
        );
        let (total, actual) =
            archive::hash_reader(reader).with_context(|| format!("hashing {}", path.display()))?;
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

/// Reject any manifest entry whose `path` is unsafe to join under the staging
/// or target directory. Runs before any extraction so a crafted manifest can
/// never reach `Path::join` in `verify_checksums` or `place_manifest_files`.
///
/// A path is accepted only if it is non-empty, NUL-free, relative, built purely
/// from `Normal` components, and already in canonical form (the exact string of
/// those components joined by single `/`). This rejects absolute paths, Windows
/// prefixes, `.`/`..` traversal, and separator aliasing (`foo//bar`, `foo/`,
/// `./foo`) that could otherwise alias on disk or escape the target.
fn validate_manifest_paths(files: &[ManifestFile]) -> eyre::Result<()> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for entry in files {
        let raw = entry.path.as_str();
        if raw.is_empty() {
            eyre::bail!("manifest contains an empty file path");
        }
        if raw.contains('\0') {
            eyre::bail!("manifest path {raw:?} contains a NUL byte");
        }
        let mut parts: Vec<String> = Vec::new();
        for comp in Path::new(raw).components() {
            match comp {
                Component::Normal(n) => parts.push(n.to_string_lossy().into_owned()),
                Component::RootDir
                | Component::Prefix(_)
                | Component::ParentDir
                | Component::CurDir => {
                    eyre::bail!(
                        "manifest path {raw:?} is not a plain relative path \
                         (absolute, prefix, or `.`/`..` components are rejected)"
                    );
                }
            }
        }
        if parts.is_empty() {
            eyre::bail!("manifest path {raw:?} has no usable path components");
        }
        let canonical = parts.join("/");
        if canonical != raw {
            eyre::bail!(
                "manifest path {raw:?} is not in canonical form (expected {canonical:?}); \
                 separator aliasing is rejected"
            );
        }
        if !seen.insert(canonical) {
            eyre::bail!("manifest declares duplicate path {raw:?}");
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
    // A schema newer than this binary understands is never importable, even
    // with --force: the binary cannot run forward-incompatible state. --force
    // only relaxes the older-than-CURRENT case (migrations run on first boot).
    if manifest.irys_schema_version > opts.expected_irys_schema_version {
        eyre::bail!(
            "snapshot irys_schema_version {} is newer than this binary supports ({}); \
             --force cannot override a forward-incompatible schema",
            manifest.irys_schema_version,
            opts.expected_irys_schema_version
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
    use proptest::prelude::*;
    use rstest::rstest;

    fn mf(path: &str) -> ManifestFile {
        ManifestFile {
            path: path.to_owned(),
            size_bytes: 0,
            sha256_hex: String::new(),
        }
    }

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

    #[test]
    fn sanity_check_reth_db_rejects_missing_mdbx() {
        let dir = TempDirBuilder::new().build();
        let err = sanity_check_reth_db(dir.path()).unwrap_err();
        assert!(err.to_string().contains("missing mdbx.dat"), "got: {err}");
    }

    #[test]
    fn sanity_check_reth_db_rejects_corrupt_mdbx() {
        let dir = TempDirBuilder::new().build();
        let reth_db = dir.path().join(RETH_SUBDIR).join(RETH_DB_DIR);
        std::fs::create_dir_all(&reth_db).expect("mk reth db");
        std::fs::write(reth_db.join("mdbx.dat"), b"not-a-real-mdbx").expect("write garbage");
        let err = sanity_check_reth_db(dir.path()).unwrap_err();
        assert!(err.to_string().contains("sanity opening"), "got: {err}");
    }

    #[test]
    #[cfg(unix)]
    fn ensure_regular_file_rejects_symlink_and_dir() {
        let dir = TempDirBuilder::new().build();
        let regular = dir.path().join("real");
        std::fs::write(&regular, b"x").expect("write regular");
        ensure_regular_file(&regular).expect("regular file accepted");

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&regular, &link).expect("mk symlink");
        let err = ensure_regular_file(&link).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "got: {err}");

        let subdir = dir.path().join("sub");
        std::fs::create_dir(&subdir).expect("mkdir");
        assert!(
            ensure_regular_file(&subdir).is_err(),
            "dir must be rejected"
        );
    }

    /// A crafted archive can ship undeclared files nested under a manifest-declared
    /// directory (e.g. `reth/jwt.hex` while the manifest declares `reth/db/mdbx.dat`).
    /// `verify_no_extra_entries` must walk the staging tree and reject any file
    /// not present in the manifest, not just top-level entries.
    #[test]
    fn verify_no_extra_entries_rejects_nested_undeclared_file() {
        let dir = TempDirBuilder::new().build();
        let reth = dir.path().join("reth");
        std::fs::create_dir_all(&reth).expect("mkdir reth");
        std::fs::write(reth.join("mdbx.dat"), b"db").expect("write declared");
        std::fs::write(reth.join("jwt.hex"), b"smuggled").expect("write undeclared");

        let declared = vec![mf("reth/mdbx.dat")];
        let err = verify_no_extra_entries(dir.path(), &declared).unwrap_err();
        assert!(
            err.to_string().contains("reth/jwt.hex") && err.to_string().contains("undeclared"),
            "got: {err}"
        );
    }

    #[test]
    fn verify_no_extra_entries_accepts_declared_tree() {
        let dir = TempDirBuilder::new().build();
        std::fs::create_dir_all(dir.path().join("reth/db")).expect("mkdir");
        std::fs::write(dir.path().join("reth/db/mdbx.dat"), b"db").expect("write");
        std::fs::write(dir.path().join(MANIFEST_FILENAME), b"{}").expect("write manifest");

        let declared = vec![mf("reth/db/mdbx.dat")];
        verify_no_extra_entries(dir.path(), &declared).expect("declared tree accepted");
    }

    #[test]
    #[cfg(unix)]
    fn verify_no_extra_entries_rejects_nested_symlink() {
        let dir = TempDirBuilder::new().build();
        let reth = dir.path().join("reth");
        std::fs::create_dir_all(&reth).expect("mkdir reth");
        std::fs::write(reth.join("mdbx.dat"), b"db").expect("write declared");
        std::os::unix::fs::symlink("/etc/passwd", reth.join("link")).expect("mk symlink");

        let declared = vec![mf("reth/mdbx.dat"), mf("reth/link")];
        let err = verify_no_extra_entries(dir.path(), &declared).unwrap_err();
        assert!(err.to_string().contains("non-regular"), "got: {err}");
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

    #[rstest]
    #[case::empty("")]
    #[case::nul("foo\u{0}bar")]
    #[case::absolute("/etc/passwd")]
    #[case::parent("../escape")]
    #[case::nested_parent("reth/../../escape")]
    #[case::curdir("./foo")]
    #[case::trailing_slash("foo/")]
    #[case::double_slash("foo//bar")]
    fn validate_manifest_paths_rejects_unsafe(#[case] path: &str) {
        assert!(
            validate_manifest_paths(&[mf(path)]).is_err(),
            "unsafe path {path:?} must be rejected"
        );
    }

    #[test]
    fn validate_manifest_paths_rejects_duplicates() {
        let err =
            validate_manifest_paths(&[mf("reth/db/mdbx.dat"), mf("reth/db/mdbx.dat")]).unwrap_err();
        assert!(err.to_string().contains("duplicate"), "got: {err}");
    }

    #[test]
    fn validate_manifest_paths_accepts_canonical_relative() {
        validate_manifest_paths(&[
            mf("irys_consensus_data/mdbx.dat"),
            mf("reth/db/mdbx.dat"),
            mf("reth/static_files/headers.jf"),
            mf(".irys_genesis.json"),
        ])
        .expect("canonical relative paths are accepted");
    }

    proptest! {
        #[test]
        fn validate_manifest_paths_accepts_only_canonical(
            segs in proptest::collection::vec("[a-z0-9_]{1,8}", 1..5)
        ) {
            let path = segs.join("/");
            prop_assert!(
                validate_manifest_paths(&[mf(&path)]).is_ok(),
                "canonical relative path {path:?} should pass"
            );
            for bad in [
                format!("/{path}"),
                format!("../{path}"),
                format!("./{path}"),
                format!("{path}/"),
                format!("{path}//x"),
            ] {
                prop_assert!(
                    validate_manifest_paths(&[mf(&bad)]).is_err(),
                    "non-canonical/escaping path {bad:?} should be rejected"
                );
            }
        }
    }

    /// Fix #3: a schema strictly newer than this binary supports is rejected
    /// even with `--force` (forward-incompatible state can never be run).
    #[test]
    fn rejects_schema_newer_than_current_even_with_force() {
        let manifest = manifest_with(1, 4, SNAPSHOT_FORMAT_VERSION);
        let (_dir, archive_path) = make_archive(&manifest);
        let opts = ImportOpts {
            input: archive_path,
            data_dir: PathBuf::from("/tmp/unused"),
            force: true,
            expected_chain_id: 1,
            expected_irys_schema_version: 3,
        };
        let err = run_import(opts).unwrap_err();
        assert!(
            err.to_string().contains("newer than this binary supports"),
            "got: {err}"
        );
    }
}
