use eyre::Context as _;
use irys_database::reth_db::cursor::DbCursorRO as _;
use irys_database::reth_db::mdbx::DatabaseArguments;
use irys_database::reth_db::transaction::DbTx as _;
use irys_database::reth_db::{Database as _, DatabaseEnv, DatabaseEnvKind};
use irys_database::tables::IrysBlockHeaders;
use reth_node_core::version::default_client_version;
use std::path::{Path, PathBuf};

use super::archive;
use super::manifest::{MANIFEST_FILENAME, ManifestFile, SNAPSHOT_FORMAT_VERSION, SnapshotManifest};

const IRYS_CONSENSUS_SUBDIR: &str = "irys_consensus_data";

#[derive(Debug, Clone)]
pub(crate) struct ImportOpts {
    pub input: PathBuf,
    pub data_dir: PathBuf,
    pub force: bool,
    pub expected_chain_id: u64,
    pub expected_irys_schema_version: u32,
}

/// Drive a snapshot import: verify, extract, checksum, and place into `data_dir`.
///
/// The imported node still needs its own `config.toml` with a fresh mining key
/// and node-local sidecars (the snapshot intentionally omits identity data).
/// Storage modules must be re-packed by the node on first boot.
pub(crate) fn run_import(opts: ImportOpts) -> eyre::Result<()> {
    let manifest = archive::read_manifest_from_archive(&opts.input)?;
    verify_compatibility(&manifest, &opts)?;
    verify_data_dir_state(&opts)?;

    let staging =
        tempfile::TempDir::new().context("creating snapshot extraction staging directory")?;
    let extracted = archive::unpack(&opts.input, staging.path())?;
    verify_checksums(staging.path(), &extracted.files)?;

    std::fs::create_dir_all(&opts.data_dir)
        .with_context(|| format!("creating data dir {}", opts.data_dir.display()))?;
    place_extracted(staging.path(), &opts.data_dir, opts.force)?;
    sanity_check_consensus_db(&opts.data_dir)?;
    Ok(())
}

/// After placing the extracted files, open the consensus mdbx env RO and walk
/// one block header. A truncated `mdbx.dat` from an interrupted extract will
/// fail to open here; an empty DB will fail the header check. Drops the env
/// before returning so the node's normal open path is unaffected.
fn sanity_check_consensus_db(data_dir: &Path) -> eyre::Result<()> {
    let consensus_dir = data_dir.join(IRYS_CONSENSUS_SUBDIR);
    if !consensus_dir.join("mdbx.dat").is_file() {
        eyre::bail!(
            "imported consensus DB is missing mdbx.dat at {}",
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
            "sanity opening imported consensus DB at {}",
            consensus_dir.display()
        )
    })?;
    let tx = env
        .tx()
        .context("starting RO tx on imported consensus DB")?;
    let mut cursor = tx
        .cursor_read::<IrysBlockHeaders>()
        .context("opening IrysBlockHeaders cursor for sanity check")?;
    if cursor
        .first()
        .context("walking IrysBlockHeaders for sanity check")?
        .is_none()
    {
        eyre::bail!(
            "imported consensus DB at {} has no block headers — snapshot appears empty or corrupt",
            consensus_dir.display()
        );
    }
    drop(tx);
    drop(env);
    Ok(())
}

/// Move each top-level entry from `staging` into `target`, skipping the manifest
/// (which lives inside the archive for verification but is not part of the
/// node's runtime state).
fn place_extracted(staging: &Path, target: &Path, force: bool) -> eyre::Result<()> {
    for entry in
        std::fs::read_dir(staging).with_context(|| format!("reading {}", staging.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        if name == MANIFEST_FILENAME {
            continue;
        }
        let src = entry.path();
        let dst = target.join(&name);
        if dst.exists() {
            if !force {
                eyre::bail!(
                    "target entry {} already exists (use --force to overwrite)",
                    dst.display()
                );
            }
            if dst.is_dir() {
                std::fs::remove_dir_all(&dst)
                    .with_context(|| format!("removing existing {}", dst.display()))?;
            } else {
                std::fs::remove_file(&dst)
                    .with_context(|| format!("removing existing {}", dst.display()))?;
            }
        }
        // rename first (fast, atomic on same fs); fall back to recursive copy
        // when the target is on a different filesystem.
        if let Err(rename_err) = std::fs::rename(&src, &dst) {
            move_by_copy(&src, &dst).with_context(|| {
                format!(
                    "moving {} to {} (rename failed: {rename_err})",
                    src.display(),
                    dst.display()
                )
            })?;
        }
    }
    Ok(())
}

fn move_by_copy(src: &Path, dst: &Path) -> eyre::Result<()> {
    let metadata = std::fs::symlink_metadata(src)?;
    if metadata.is_dir() {
        std::fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
        for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
            let entry = entry?;
            move_by_copy(&entry.path(), &dst.join(entry.file_name()))?;
        }
        std::fs::remove_dir(src)
            .with_context(|| format!("removing source dir {}", src.display()))?;
    } else {
        std::fs::copy(src, dst)
            .with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
        std::fs::remove_file(src).with_context(|| format!("removing source {}", src.display()))?;
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
    use tempfile::TempDir;

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

    fn make_archive(manifest: &SnapshotManifest) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
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
