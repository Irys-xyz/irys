//! Path guard and write-region extent check.
//!
//! The bench refuses any path whose canonical form lacks a `loadtest` component.
//! That keeps a bad `--dir` off the node's real submodule files.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::io::AsRawFd as _;
use std::path::{Component, Path, PathBuf};

use eyre::{WrapErr as _, eyre};

pub(crate) const CHUNK_SIZE: u64 = 262_144;

const FIEMAP_EXTENT_LAST: u32 = 0x1;
const FIEMAP_EXTENT_UNKNOWN: u32 = 0x2;
const FIEMAP_EXTENT_DELALLOC: u32 = 0x4;
const FIEMAP_EXTENT_UNWRITTEN: u32 = 0x800;
const FIEMAP_BAD: u32 = FIEMAP_EXTENT_UNKNOWN | FIEMAP_EXTENT_DELALLOC | FIEMAP_EXTENT_UNWRITTEN;
/// `_IOWR('f', 11, struct fiemap)` with a 32-byte header.
const FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B;

#[derive(Debug)]
pub(crate) enum ExtentCheck {
    Written,
    /// The filesystem has no FIEMAP. Allowed only when this process just wrote the bytes.
    Unsupported,
}

pub(crate) fn has_loadtest_component(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => name == "loadtest",
        _ => false,
    })
}

enum MissingPart {
    Normal(std::ffi::OsString),
    CurDir,
    ParentDir,
}

/// Canonicalize the deepest existing ancestor, then apply the missing tail.
/// `..` in that tail is applied, so `loadtest/../../outside` does not keep the component.
fn resolve_with_ancestor(path: &Path) -> eyre::Result<PathBuf> {
    let mut missing = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        if cursor.as_os_str().is_empty() {
            cursor = std::env::current_dir()?;
        }
        if cursor.exists() || is_symlink(&cursor) {
            let mut resolved = cursor
                .canonicalize()
                .wrap_err_with(|| format!("{} could not be canonicalized", cursor.display()))?;
            apply_missing(&mut resolved, &missing)?;
            return Ok(resolved);
        }
        let popped = match cursor.components().next_back() {
            Some(Component::Normal(name)) => MissingPart::Normal(name.to_os_string()),
            Some(Component::CurDir) => MissingPart::CurDir,
            Some(Component::ParentDir) => MissingPart::ParentDir,
            _ => return Err(eyre!("{} has no existing ancestor", path.display())),
        };
        let Some(parent) = cursor.parent() else {
            return Err(eyre!("{} has no existing ancestor", path.display()));
        };
        if parent == cursor {
            return Err(eyre!("{} has no existing ancestor", path.display()));
        }
        missing.push(popped);
        if parent.as_os_str().is_empty() {
            cursor = std::env::current_dir()?;
            continue;
        }
        cursor = parent.to_path_buf();
    }
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

fn apply_missing(base: &mut PathBuf, missing: &[MissingPart]) -> eyre::Result<()> {
    for part in missing.iter().rev() {
        match part {
            MissingPart::Normal(name) => base.push(name),
            MissingPart::CurDir => {}
            MissingPart::ParentDir => {
                if !base.pop() {
                    return Err(eyre!("path escapes the filesystem root"));
                }
            }
        }
    }
    Ok(())
}

/// Refuse unless the path keeps a `loadtest` component after the deepest
/// existing ancestor is canonicalized. Checked before any file is created.
pub(crate) fn require_loadtest_ancestor(label: &str, path: &Path) -> eyre::Result<PathBuf> {
    if !has_loadtest_component(path) {
        return Err(eyre!(
            "{label} {} has no `loadtest` path component; refusing to run",
            path.display()
        ));
    }
    let resolved = resolve_with_ancestor(path)?;
    if !has_loadtest_component(&resolved) {
        return Err(eyre!(
            "{label} resolves to {}, which has no `loadtest` path component; refusing to run",
            resolved.display()
        ));
    }
    Ok(resolved)
}

/// Refuse unless every path has a `loadtest` component. Checked on the caller's
/// path and again after canonicalize, so a symlink cannot leave `loadtest`.
pub(crate) fn require_loadtest(label: &str, path: &Path) -> eyre::Result<PathBuf> {
    if !has_loadtest_component(path) {
        return Err(eyre!(
            "{label} {} has no `loadtest` path component; refusing to run",
            path.display()
        ));
    }
    let canonical = path
        .canonicalize()
        .wrap_err_with(|| format!("{label} {} could not be canonicalized", path.display()))?;
    if !has_loadtest_component(&canonical) {
        return Err(eyre!(
            "{label} canonical path {} has no `loadtest` path component; refusing to run",
            canonical.display()
        ));
    }
    Ok(canonical)
}

/// Create `path` at exactly `bytes` by writing zeros. Does not truncate or resize
/// an existing file. A new recall file is mode `0444`.
pub(crate) fn create_small(path: &Path, bytes: u64, recall: bool) -> eyre::Result<bool> {
    if bytes == 0 || !bytes.is_multiple_of(CHUNK_SIZE) {
        return Err(eyre!(
            "--create-small must be a positive multiple of {CHUNK_SIZE} bytes"
        ));
    }
    require_loadtest_ancestor("path", path)?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let len = path.metadata()?.len();
        if len != bytes {
            return Err(eyre!(
                "{} exists at {len} bytes, not {bytes}; refusing to resize it",
                path.display()
            ));
        }
        if recall {
            chmod_readonly(path)?;
        }
        return Ok(false);
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(path)
        .wrap_err_with(|| format!("failed to create {}", path.display()))?;
    let buf = vec![0_u8; 8 * 1024 * 1024];
    let mut left = bytes;
    let mut wrote = 0_u64;
    let mut next_report = 1024 * 1024 * 1024;
    while left > 0 {
        let n = std::cmp::min(left, buf.len() as u64) as usize;
        file.write_all(&buf[..n])?;
        left -= n as u64;
        wrote += n as u64;
        if wrote >= next_report {
            eprintln!(
                "irys-sm-bench: wrote {wrote} / {bytes} bytes of {}",
                path.display()
            );
            next_report = next_report.saturating_add(1024 * 1024 * 1024);
        }
    }
    file.sync_all()?;
    drop(file);
    if recall {
        chmod_readonly(path)?;
    }
    Ok(true)
}

fn chmod_readonly(path: &Path) -> eyre::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o444);
    fs::set_permissions(path, perms)?;
    Ok(())
}

pub(crate) fn check_written_extents(path: &Path, created_this_run: bool) -> eyre::Result<()> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Err(eyre!("{} is empty; refusing to bench it", path.display()));
    }
    match extents_are_written(&file, len) {
        Ok(ExtentCheck::Written) => Ok(()),
        Ok(ExtentCheck::Unsupported) if created_this_run => {
            eprintln!(
                "irys-sm-bench: {} has no FIEMAP; this process wrote the bytes, continuing",
                path.display()
            );
            Ok(())
        }
        Ok(ExtentCheck::Unsupported) => Err(eyre!(
            "{} does not support FIEMAP; refusing to bench a file whose extents were not checked",
            path.display()
        )),
        Err(error) => {
            Err(error).wrap_err_with(|| format!("extent check failed for {}", path.display()))
        }
    }
}

fn extents_are_written(file: &File, len: u64) -> eyre::Result<ExtentCheck> {
    let mut cursor = 0_u64;
    while cursor < len {
        let mut buf = vec![0_u8; 32 + 64 * 56];
        buf[0..8].copy_from_slice(&cursor.to_ne_bytes());
        buf[8..16].copy_from_slice(&(len - cursor).to_ne_bytes());
        buf[24..28].copy_from_slice(&64_u32.to_ne_bytes());
        let rc = unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_FIEMAP, buf.as_mut_ptr()) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EOPNOTSUPP)
                || err.raw_os_error() == Some(libc::ENOTTY)
            {
                return Ok(ExtentCheck::Unsupported);
            }
            return Err(eyre!("FIEMAP: {err}"));
        }
        let mapped = u32::from_ne_bytes(buf[20..24].try_into().expect("4 bytes"));
        if mapped == 0 {
            return Err(eyre!("hole or unmapped range at byte {cursor}"));
        }
        let mut advanced = false;
        for index in 0..mapped as usize {
            let at = 32 + index * 56;
            let logical = u64::from_ne_bytes(buf[at..at + 8].try_into().expect("8 bytes"));
            let length = u64::from_ne_bytes(buf[at + 16..at + 24].try_into().expect("8 bytes"));
            let flags = u32::from_ne_bytes(buf[at + 40..at + 44].try_into().expect("4 bytes"));
            if logical > cursor {
                return Err(eyre!("hole at byte {cursor} before extent {logical}"));
            }
            if flags & FIEMAP_BAD != 0 {
                return Err(eyre!(
                    "extent at byte {logical} is unwritten, delayed, or unknown (flags {flags:#x})"
                ));
            }
            let next = logical.saturating_add(length);
            if next > cursor {
                cursor = next;
                advanced = true;
            }
            if flags & FIEMAP_EXTENT_LAST != 0 {
                if cursor < len {
                    return Err(eyre!("hole at byte {cursor} after the last extent"));
                }
                return Ok(ExtentCheck::Written);
            }
        }
        if !advanced {
            return Err(eyre!("FIEMAP made no progress at byte {cursor}"));
        }
    }
    Ok(ExtentCheck::Written)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File, OpenOptions};
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use irys_testing_utils::TempDirBuilder;

    use super::*;

    #[test]
    fn loadtest_component_is_exact() {
        assert!(has_loadtest_component(Path::new("/mnt/sm-2/loadtest/db")));
        assert!(has_loadtest_component(Path::new("loadtest")));
        assert!(!has_loadtest_component(Path::new("/mnt/sm-2/db")));
        assert!(!has_loadtest_component(Path::new("/tmp/loadtest-extra/db")));
        assert!(!has_loadtest_component(Path::new("/tmp/not-loadtest/db")));
    }

    #[test]
    fn create_small_does_not_write_through_dotdot() {
        let tmp = TempDirBuilder::new().prefix("sm-bench-escape-").build();
        let safe = tmp.path().join("safe/loadtest");
        fs::create_dir_all(&safe).unwrap();
        let escaped = tmp.path().join("escaped-recall.dat");
        let via = safe.join("../../escaped-recall.dat");
        let err = create_small(&via, CHUNK_SIZE, true).unwrap_err();
        assert!(
            !escaped.exists(),
            "created {} before the canonical check",
            escaped.display()
        );
        let text = format!("{err:#}");
        assert!(text.contains("loadtest"), "{text}");
    }

    #[test]
    fn create_small_does_not_chmod_an_existing_file_outside_loadtest() {
        let tmp = TempDirBuilder::new().prefix("sm-bench-chmod-").build();
        let safe = tmp.path().join("safe/loadtest");
        fs::create_dir_all(&safe).unwrap();
        let escaped = tmp.path().join("escaped-recall.dat");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .open(&escaped)
            .unwrap();
        file.write_all(&[0xAB]).unwrap();
        file.set_len(CHUNK_SIZE).unwrap();
        drop(file);
        let via = safe.join("../../escaped-recall.dat");
        let err = create_small(&via, CHUNK_SIZE, true).unwrap_err();
        let mode = fs::metadata(&escaped).unwrap().permissions().mode() & 0o777;
        assert_ne!(mode, 0o444, "chmod ran before the canonical check");
        let mut byte = [0_u8; 1];
        File::open(&escaped).unwrap().read_exact(&mut byte).unwrap();
        assert_eq!(byte[0], 0xAB);
        let text = format!("{err:#}");
        assert!(text.contains("loadtest"), "{text}");
    }

    #[test]
    fn create_small_does_not_follow_a_loadtest_symlink_outside() {
        let tmp = TempDirBuilder::new().prefix("sm-bench-link-").build();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let safe = tmp.path().join("safe");
        fs::create_dir_all(&safe).unwrap();
        symlink(&outside, safe.join("loadtest")).unwrap();
        let path = safe.join("loadtest/recall.dat");
        let err = create_small(&path, CHUNK_SIZE, true).unwrap_err();
        assert!(
            !outside.join("recall.dat").exists(),
            "wrote through the loadtest symlink"
        );
        let text = format!("{err:#}");
        assert!(text.contains("loadtest"), "{text}");
    }

    #[test]
    fn dotdot_through_a_missing_directory_is_outside_loadtest() {
        let tmp = TempDirBuilder::new().prefix("sm-bench-missing-").build();
        let safe = tmp.path().join("safe/loadtest");
        fs::create_dir_all(&safe).unwrap();
        let via = safe.join("nope/../../../escaped-recall.dat");
        let escaped = tmp.path().join("escaped-recall.dat");
        let err = require_loadtest_ancestor("recall-file", &via).unwrap_err();
        assert!(!escaped.exists());
        assert!(!safe.join("nope").exists());
        let text = format!("{err:#}");
        assert!(text.contains("loadtest"), "{text}");
    }

    #[test]
    fn create_small_writes_a_new_file_under_loadtest() {
        let tmp = TempDirBuilder::new().prefix("sm-bench-create-").build();
        let dir = tmp.path().join("safe/loadtest");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recall.dat");
        assert!(create_small(&path, CHUNK_SIZE, true).unwrap());
        assert_eq!(fs::metadata(&path).unwrap().len(), CHUNK_SIZE);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o444);
    }
}
