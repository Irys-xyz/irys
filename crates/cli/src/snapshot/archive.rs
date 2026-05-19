use eyre::Context as _;
use sha2::{Digest as _, Sha256};
use std::{
    fs::File,
    io::{BufReader, BufWriter, Read as _},
    path::Path,
};

use super::manifest::{MANIFEST_FILENAME, ManifestFile, SnapshotManifest};

const ZSTD_COMPRESSION_LEVEL: i32 = 3;
const HASH_BUFFER_BYTES: usize = 64 * 1024;

/// Walk `staging_dir`, hashing every regular file (excluding `manifest.json`)
/// and producing a sorted file list suitable for embedding in a `SnapshotManifest`.
pub(crate) fn build_file_list(staging_dir: &Path) -> eyre::Result<Vec<ManifestFile>> {
    let mut entries: Vec<ManifestFile> = Vec::new();
    walk(staging_dir, staging_dir, &mut entries)?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<ManifestFile>) -> eyre::Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk(root, &path, out)?;
        } else if file_type.is_file() {
            let rel = path
                .strip_prefix(root)
                .expect("path is under root")
                .to_string_lossy()
                .replace('\\', "/");
            if rel == MANIFEST_FILENAME {
                continue;
            }
            let (size_bytes, sha256_hex) = hash_file(&path)?;
            out.push(ManifestFile {
                path: rel,
                size_bytes,
                sha256_hex,
            });
        }
    }
    Ok(())
}

/// Stream `reader` through SHA-256, returning `(byte_count, lowercase_hex_digest)`.
/// Shared by export-side manifest building and import-side checksum verification
/// so both sides hash bytes identically.
pub(crate) fn hash_reader<R: std::io::Read>(mut reader: R) -> eyre::Result<(u64, String)> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; HASH_BUFFER_BYTES];
    let mut total: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total = total
            .checked_add(u64::try_from(n).expect("read len fits in u64"))
            .ok_or_else(|| eyre::eyre!("file size overflow while hashing"))?;
    }
    Ok((total, hex::encode(hasher.finalize())))
}

fn hash_file(path: &Path) -> eyre::Result<(u64, String)> {
    let reader = BufReader::new(
        File::open(path).with_context(|| format!("opening {} for hashing", path.display()))?,
    );
    hash_reader(reader).with_context(|| format!("hashing {}", path.display()))
}

/// Pack `staging_dir` (which must already contain `manifest.json`) into a
/// zstd-compressed tarball at `output`.
pub(crate) fn pack(staging_dir: &Path, output: &Path) -> eyre::Result<()> {
    if !staging_dir.join(MANIFEST_FILENAME).is_file() {
        eyre::bail!(
            "staging dir {} is missing {}",
            staging_dir.display(),
            MANIFEST_FILENAME
        );
    }
    let output_file = File::create(output)
        .with_context(|| format!("creating snapshot output {}", output.display()))?;
    let encoder = zstd::Encoder::new(BufWriter::new(output_file), ZSTD_COMPRESSION_LEVEL)
        .context("initializing zstd encoder")?;
    let mut tar_builder = tar::Builder::new(encoder);
    tar_builder.follow_symlinks(false);
    tar_builder
        .append_dir_all(".", staging_dir)
        .with_context(|| format!("packing {} into tar", staging_dir.display()))?;
    let encoder = tar_builder.into_inner().context("finalizing tar")?;
    encoder.finish().context("finalizing zstd")?;
    Ok(())
}

/// Decompress and untar `archive` into `target_dir`, then read and return the manifest.
pub(crate) fn unpack(archive: &Path, target_dir: &Path) -> eyre::Result<SnapshotManifest> {
    std::fs::create_dir_all(target_dir)
        .with_context(|| format!("creating target dir {}", target_dir.display()))?;
    let f =
        File::open(archive).with_context(|| format!("opening snapshot {}", archive.display()))?;
    let decoder = zstd::Decoder::new(BufReader::new(f)).context("initializing zstd decoder")?;
    let mut tar = tar::Archive::new(decoder);
    tar.unpack(target_dir)
        .with_context(|| format!("extracting to {}", target_dir.display()))?;
    read_manifest_from_dir(target_dir)
}

/// Read the manifest from an already-extracted snapshot directory.
pub(crate) fn read_manifest_from_dir(dir: &Path) -> eyre::Result<SnapshotManifest> {
    let path = dir.join(MANIFEST_FILENAME);
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("reading manifest {}", path.display()))?;
    serde_json::from_str(&contents).with_context(|| format!("parsing manifest {}", path.display()))
}

/// Stream just the manifest entry out of an archive without extracting other files.
/// Useful for verifying compatibility before committing to a full extraction.
///
/// Walks every entry and accepts exactly one root-level manifest
/// (`./manifest.json` or `manifest.json`):
/// * Nested manifests (`foo/manifest.json`) are ignored — a crafted archive
///   cannot smuggle a decoy under a nested path.
/// * Two or more root manifests are rejected outright. `tar::Archive::unpack`
///   overwrites by default (last entry wins on disk) while a first-match read
///   would see the first entry, so a duplicate could present one manifest for
///   the preflight compatibility gate and a different one for placement. The
///   only way preflight and `unpack()` provably agree is to refuse archives
///   that contain more than one root manifest.
pub(crate) fn read_manifest_from_archive(archive: &Path) -> eyre::Result<SnapshotManifest> {
    let f =
        File::open(archive).with_context(|| format!("opening snapshot {}", archive.display()))?;
    let decoder = zstd::Decoder::new(BufReader::new(f)).context("initializing zstd decoder")?;
    let mut tar = tar::Archive::new(decoder);
    let mut manifest_json: Option<String> = None;
    for entry in tar.entries()? {
        let mut entry = entry?;
        if !is_root_manifest_path(&entry.path()?) {
            continue;
        }
        if manifest_json.is_some() {
            eyre::bail!(
                "archive {} contains more than one root {MANIFEST_FILENAME}; refusing to \
                 import (a duplicate manifest can present one manifest for the preflight \
                 compatibility check and another for file placement)",
                archive.display()
            );
        }
        let mut buf = String::new();
        entry.read_to_string(&mut buf)?;
        manifest_json = Some(buf);
    }
    let buf = manifest_json.ok_or_else(|| {
        eyre::eyre!(
            "{MANIFEST_FILENAME} not found at archive root in {}",
            archive.display()
        )
    })?;
    serde_json::from_str(&buf).context("parsing manifest from archive")
}

/// Accept only the archive's root manifest: bare `manifest.json` or `./manifest.json`.
/// Reject anything nested (e.g. `foo/manifest.json`) — those would let a crafted
/// archive bypass `verify_compatibility` while `unpack()` later reads a different
/// root manifest.
fn is_root_manifest_path(path: &std::path::Path) -> bool {
    let mut comps = path.components();
    let Some(first) = comps.next() else {
        return false;
    };
    let first = match first {
        std::path::Component::CurDir => match comps.next() {
            Some(c) => c,
            None => return false,
        },
        other => other,
    };
    if comps.next().is_some() {
        return false;
    }
    matches!(first, std::path::Component::Normal(n) if n == MANIFEST_FILENAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::manifest::SNAPSHOT_FORMAT_VERSION;
    use irys_testing_utils::utils::TempDirBuilder;
    use proptest::prelude::*;
    use std::collections::HashSet;

    fn write_manifest(staging: &Path, manifest: &SnapshotManifest) -> eyre::Result<()> {
        std::fs::write(
            staging.join(MANIFEST_FILENAME),
            serde_json::to_string_pretty(manifest)?,
        )?;
        Ok(())
    }

    fn write_tree(root: &Path, files: &[(String, Vec<u8>)]) {
        for (rel, content) in files {
            let full = root.join(rel);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).expect("create parent dir");
            }
            std::fs::write(&full, content).expect("write file");
        }
    }

    proptest! {
        #[test]
        fn pack_unpack_roundtrip_preserves_content(
            files in proptest::collection::vec(
                (
                    "[a-z]{1,8}(/[a-z]{1,8}){0,3}",
                    proptest::collection::vec(any::<u8>(), 0..2048),
                ),
                1..6,
            )
        ) {
            // Dedupe paths, drop any that would collide with the manifest filename,
            // and drop any pair where one path is an ancestor of another (would create
            // a file/directory conflict on the filesystem).
            let mut seen = HashSet::new();
            let deduped: Vec<(String, Vec<u8>)> = files
                .into_iter()
                .filter(|(p, _)| p != MANIFEST_FILENAME && seen.insert(p.clone()))
                .collect();
            let paths: Vec<String> = deduped.iter().map(|(p, _)| p.clone()).collect();
            let unique: Vec<(String, Vec<u8>)> = deduped
                .into_iter()
                .filter(|(p, _)| {
                    !paths.iter().any(|other| {
                        other != p
                            && (other.starts_with(&format!("{p}/"))
                                || p.starts_with(&format!("{other}/")))
                    })
                })
                .collect();
            prop_assume!(!unique.is_empty());

            let src = TempDirBuilder::new().build();
            let dst = TempDirBuilder::new().build();
            let archive_dir = TempDirBuilder::new().build();
            let archive_path = archive_dir.path().join("snap.tar.zst");

            write_tree(src.path(), &unique);

            let file_list = build_file_list(src.path()).unwrap();
            let manifest = SnapshotManifest {
                format_version: SNAPSHOT_FORMAT_VERSION,
                chain_id: 42,
                irys_schema_version: 3,
                irys_tip_height: None,
                reth_tip_height: None,
                includes_caches: false,
                created_at_unix_secs: 0,
                created_by: "proptest".to_owned(),
                files: file_list,
            };
            write_manifest(src.path(), &manifest).unwrap();

            pack(src.path(), &archive_path).unwrap();
            let read_back = unpack(&archive_path, dst.path()).unwrap();
            prop_assert_eq!(&read_back, &manifest);

            for (rel, content) in &unique {
                let got = std::fs::read(dst.path().join(rel)).unwrap();
                prop_assert_eq!(&got, content);
            }
        }
    }

    #[test]
    fn read_manifest_from_archive_returns_same_manifest() {
        let src = TempDirBuilder::new().build();
        let archive_dir = TempDirBuilder::new().build();
        let archive_path = archive_dir.path().join("snap.tar.zst");

        let tree = vec![
            ("irys/.irys_genesis.json".to_owned(), b"{}".to_vec()),
            ("reth/db/mdbx.dat".to_owned(), vec![1_u8; 100]),
        ];
        write_tree(src.path(), &tree);

        let manifest = SnapshotManifest {
            format_version: SNAPSHOT_FORMAT_VERSION,
            chain_id: 1024,
            irys_schema_version: 3,
            irys_tip_height: Some(7),
            reth_tip_height: Some(7),
            includes_caches: false,
            created_at_unix_secs: 1_000,
            created_by: "test".to_owned(),
            files: build_file_list(src.path()).unwrap(),
        };
        write_manifest(src.path(), &manifest).unwrap();
        pack(src.path(), &archive_path).unwrap();

        let streamed = read_manifest_from_archive(&archive_path).unwrap();
        assert_eq!(streamed, manifest);
    }

    /// CONS-1: a manifest nested under a subdirectory must NOT be accepted as
    /// the archive's root manifest. Without this guard, a crafted archive
    /// with `foo/manifest.json` could lie about chain_id/schema_version and
    /// bypass `verify_compatibility`.
    #[test]
    fn read_manifest_archive_rejects_nested_manifest() {
        let src = TempDirBuilder::new().build();
        let nested = src.path().join("foo");
        std::fs::create_dir(&nested).expect("mkdir");
        let bogus = SnapshotManifest {
            format_version: SNAPSHOT_FORMAT_VERSION,
            chain_id: 9999,
            irys_schema_version: 99,
            irys_tip_height: None,
            reth_tip_height: None,
            includes_caches: false,
            created_at_unix_secs: 0,
            created_by: "decoy".to_owned(),
            files: vec![],
        };
        std::fs::write(
            nested.join(MANIFEST_FILENAME),
            serde_json::to_string_pretty(&bogus).unwrap(),
        )
        .unwrap();
        // No root manifest — only the decoy at foo/manifest.json.
        // We can't use pack() (it asserts a root manifest), so build the tar manually.
        let archive_dir = TempDirBuilder::new().build();
        let archive_path = archive_dir.path().join("snap.tar.zst");
        let f = File::create(&archive_path).unwrap();
        let encoder = zstd::Encoder::new(BufWriter::new(f), ZSTD_COMPRESSION_LEVEL).unwrap();
        let mut tar_builder = tar::Builder::new(encoder);
        tar_builder.follow_symlinks(false);
        tar_builder.append_dir_all(".", src.path()).unwrap();
        tar_builder.into_inner().unwrap().finish().unwrap();

        let err = read_manifest_from_archive(&archive_path).unwrap_err();
        assert!(
            err.to_string().contains("manifest.json"),
            "expected manifest-not-found error, got: {err}"
        );
    }

    #[test]
    fn read_manifest_archive_accepts_root_dot_prefix() {
        // `tar::Builder::append_dir_all(".", staging)` emits entries with
        // `./manifest.json` paths. Confirm both bare and ./ root forms match.
        for prefix in ["", "./"] {
            assert!(is_root_manifest_path(Path::new(&format!(
                "{prefix}{MANIFEST_FILENAME}"
            ))));
        }
    }

    #[test]
    fn read_manifest_archive_rejects_nested_paths() {
        for nested in [
            "foo/manifest.json",
            "a/b/manifest.json",
            "./foo/manifest.json",
            "../manifest.json",
        ] {
            assert!(
                !is_root_manifest_path(Path::new(nested)),
                "nested path {nested:?} must not be accepted as root manifest"
            );
        }
    }

    /// A crafted archive with two root `manifest.json` entries lets the first
    /// (read by the preflight compatibility gate) and the last (the one
    /// `tar::unpack` leaves on disk) disagree. `read_manifest_from_archive`
    /// must reject it outright rather than return either.
    #[test]
    fn read_manifest_archive_rejects_duplicate_root_manifests() {
        fn manifest(chain_id: u64) -> SnapshotManifest {
            SnapshotManifest {
                format_version: SNAPSHOT_FORMAT_VERSION,
                chain_id,
                irys_schema_version: 3,
                irys_tip_height: None,
                reth_tip_height: None,
                includes_caches: false,
                created_at_unix_secs: 0,
                created_by: "test".to_owned(),
                files: vec![],
            }
        }

        let archive_dir = TempDirBuilder::new().build();
        let archive_path = archive_dir.path().join("snap.tar.zst");
        let f = File::create(&archive_path).unwrap();
        let encoder = zstd::Encoder::new(BufWriter::new(f), ZSTD_COMPRESSION_LEVEL).unwrap();
        let mut builder = tar::Builder::new(encoder);
        for chain_id in [1_u64, 9999_u64] {
            let bytes = serde_json::to_vec(&manifest(chain_id)).unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_size(u64::try_from(bytes.len()).expect("len fits in u64"));
            header.set_mode(0o644);
            builder
                .append_data(&mut header, MANIFEST_FILENAME, &bytes[..])
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();

        let err = read_manifest_from_archive(&archive_path).unwrap_err();
        assert!(
            err.to_string().contains("more than one root"),
            "expected duplicate-manifest rejection, got: {err}"
        );
    }
}
