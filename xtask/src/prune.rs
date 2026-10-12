//! Delete test executables that the current build no longer uses.
//!
//! Cargo names an executable `<target>-<hash>` in `deps/`, and the hash changes with the feature
//! set, so a `-p X` run and a `--workspace` run can each leave their own 50–200 MB copy of the same
//! test target. Cargo never deletes either. After a run, every other copy of a test target the run
//! built is superseded, and an executable whose crate root no longer exists is orphaned.
//!
//! A target is identified by its executable stem plus its crate root, read from the first source
//! in the `.d` dep-info file beside it. The stem alone is not enough: every crate's integration
//! binary is named `it`.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// What one prune pass removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Every file removed: executables plus their `.d` and `.dwo` siblings.
    pub removed_files: Vec<PathBuf>,
    pub removed_executables: usize,
    pub removed_bytes: u64,
}

/// Why an executable in `deps/` is removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stale {
    /// Another build of a test target that this run built.
    Superseded,
    /// Its crate root no longer exists in the workspace.
    Orphaned,
}

/// Remove stale executables from each `deps/` directory that holds one of `built`.
///
/// `built` are the test executables this run built or reused. A candidate modified or accessed
/// after `keep_after` is kept: a concurrent or recent run may still use it.
pub fn prune_stale_executables(
    workspace_root: &Path,
    built: &[PathBuf],
    keep_after: SystemTime,
) -> io::Result<PruneReport> {
    let mut report = PruneReport::default();
    let dirs: HashSet<&Path> = built
        .iter()
        .filter_map(|exe| exe.parent())
        .filter(|dir| dir.file_name().is_some_and(|n| n == "deps"))
        .collect();

    for dir in dirs {
        let current: HashSet<&Path> = built
            .iter()
            .filter(|exe| exe.parent() == Some(dir))
            .map(PathBuf::as_path)
            .collect();
        let mut keys = HashSet::new();
        for exe in &current {
            match target_key(workspace_root, exe) {
                Some(key) => {
                    keys.insert(key);
                }
                None => {
                    tracing::debug!(exe = %exe.display(), "built executable has no readable crate root; its old copies are kept")
                }
            }
        }

        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if current.contains(path.as_path()) || executable_stem(&path).is_none() {
                continue;
            }
            let Some(key) = target_key(workspace_root, &path) else {
                tracing::debug!(exe = %path.display(), "kept: no readable crate root in its .d file");
                continue;
            };
            let reason = if !key.root.exists() {
                Stale::Orphaned
            } else if keys.contains(&key) {
                Stale::Superseded
            } else {
                tracing::debug!(exe = %path.display(), root = %key.root.display(), "kept: not a target this run built");
                continue;
            };
            let meta = match fs::metadata(&path) {
                Ok(meta) => meta,
                // A concurrent prune of this target dir removed it first.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let (modified, accessed) = (meta.modified()?, meta.accessed()?);
            if modified > keep_after || accessed > keep_after {
                tracing::debug!(exe = %path.display(), ?reason, ?modified, ?accessed, "kept: used within the grace period");
                continue;
            }
            tracing::debug!(exe = %path.display(), root = %key.root.display(), ?reason, "removing");
            remove_with_siblings(dir, &path, &mut report)?;
            if let Some(id) = path.file_name().and_then(|n| n.to_str()) {
                remove_fingerprint(dir, id);
            }
        }
    }
    Ok(report)
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct TargetKey {
    stem: String,
    root: PathBuf,
}

fn target_key(workspace_root: &Path, exe: &Path) -> Option<TargetKey> {
    let stem = executable_stem(exe)?.to_owned();
    let dep_info = fs::read_to_string(exe.with_extension("d")).ok()?;
    let root = crate_root(&dep_info)?;
    Some(TargetKey {
        stem,
        root: workspace_root.join(root),
    })
}

/// `<stem>-<16 hex>` with no extension is a cargo executable in `deps/`; rlibs, rmetas, shared
/// objects and dep-info files all carry an extension.
fn executable_stem(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    if name.contains('.') {
        return None;
    }
    let (stem, hash) = name.rsplit_once('-')?;
    (hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some(stem)
}

/// The first source in a dep-info file is the crate root. rustc writes paths relative to the
/// directory cargo runs it from, the workspace root, so a relative result joins onto that.
fn crate_root(dep_info: &str) -> Option<&Path> {
    let (_, deps) = dep_info.lines().next()?.split_once(": ")?;
    let first = deps.split_whitespace().next()?;
    // An escaped space would split the path; such a file is left alone.
    (!first.ends_with('\\')).then(|| Path::new(first))
}

fn remove_with_siblings(dir: &Path, exe: &Path, report: &mut PruneReport) -> io::Result<()> {
    let name = exe
        .file_name()
        .and_then(|n| n.to_str())
        .expect("executable_stem accepted a UTF-8 name");
    let prefix = format!("{name}.");
    let mut doomed = vec![exe.to_path_buf()];
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(&prefix))
        {
            doomed.push(path);
        }
    }
    let mut exe_removed = false;
    for path in doomed {
        // A file a concurrent prune already removed is skipped, not an error.
        let removed =
            fs::metadata(&path).and_then(|meta| fs::remove_file(&path).map(|()| meta.len()));
        match removed {
            Ok(len) => {
                exe_removed |= path == exe;
                report.removed_bytes += len;
                report.removed_files.push(path);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    if exe_removed {
        report.removed_executables += 1;
    }
    Ok(())
}

/// Delete everything in `target_dir` except the run lock file. The caller holds that lock
/// exclusively: `cargo clean` would unlink the lock file too, and a run that started meanwhile
/// would lock a new file and build into the directory being deleted.
pub fn clean_target_dir(target_dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(target_dir)? {
        let entry = entry?;
        if entry.file_name() == RUN_LOCK {
            continue;
        }
        let removed = if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())
        } else {
            fs::remove_file(entry.path())
        };
        match removed {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Where recorded invocations list the artifacts they used, one file per invocation.
const USAGE_DIR: &str = "xtask-usage";

/// Lock file that xtask runs share while they use the target dir. It is outside [`USAGE_DIR`]
/// because expired records there are deleted, and a deleted lock file locks nothing.
const RUN_LOCK: &str = "xtask-run.lock";

fn open_run_lock(target_dir: &Path) -> io::Result<fs::File> {
    fs::create_dir_all(target_dir)?;
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(target_dir.join(RUN_LOCK))
}

/// Hold a shared lock on the target dir for as long as the returned file lives. A run holds it
/// while it builds and runs tests, so no other run prunes the files it uses. It waits while
/// another run prunes.
pub fn lock_run_shared(target_dir: &Path) -> io::Result<fs::File> {
    let file = open_run_lock(target_dir)?;
    file.lock_shared()?;
    Ok(file)
}

/// The exclusive lock a prune holds, or `None` when another run holds the shared lock. The
/// caller must first drop its own shared lock: a second handle conflicts with it too.
pub fn try_lock_run_exclusive(target_dir: &Path) -> io::Result<Option<fs::File>> {
    let file = open_run_lock(target_dir)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Record that `invocation` used `artifacts`, so [`prune_unused_units`] keeps them while the
/// record is live. A record's age is its file's mtime, refreshed by each run of the same
/// invocation.
pub fn record_usage(target_dir: &Path, invocation: &str, artifacts: &[PathBuf]) -> io::Result<()> {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    invocation.hash(&mut hasher);
    let dir = target_dir.join(USAGE_DIR);
    fs::create_dir_all(&dir)?;
    let mut body = format!("# {invocation}\n");
    for path in artifacts {
        body.push_str(&path.to_string_lossy());
        body.push('\n');
    }
    // Written aside and renamed over the record, so a concurrent prune reads the old record or
    // the new one, never a truncated one. A prune that reads the aside file too only keeps more.
    let record = dir.join(format!("{:016x}.txt", hasher.finish()));
    let aside = dir.join(format!(
        "{:016x}.txt.{}",
        hasher.finish(),
        std::process::id()
    ));
    fs::write(&aside, body)?;
    fs::rename(&aside, &record)
}

/// What one unit prune pass removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UnitPruneReport {
    pub removed_units: usize,
    pub removed_build_dirs: usize,
    /// `.fingerprint` dirs whose unit is already gone.
    pub removed_fingerprints: usize,
    pub removed_bytes: u64,
}

/// Remove compilation units that nothing is known to use.
///
/// A unit is every file in a `deps/` directory that shares one `<name>-<hash>`: its rlib, rmeta,
/// proc-macro shared object or executable, its `.d` and its `.dwo` files, plus its
/// `.fingerprint/<name>-<hash>` dir. A unit is in use when a live usage record names one of its
/// files, or when it is in the dependency closure of an artifact cargo uplifted into the profile
/// dir (see [`uplifted_closure`]): that closure is what the latest plain `cargo build` or
/// `cargo run` uses, which no record covers. Any other unit is removed once it was last touched
/// before `unused_before`. A build-script directory under `build/` follows the same rules.
///
/// Records last written before `unused_before` expire and are deleted. With no live record the
/// pass removes nothing, since nothing is known to be in use.
pub fn prune_unused_units(
    target_dir: &Path,
    unused_before: SystemTime,
) -> io::Result<UnitPruneReport> {
    let mut report = UnitPruneReport::default();
    let used = live_usage(&target_dir.join(USAGE_DIR), unused_before)?;
    if used.is_empty() {
        tracing::debug!("no live usage record; unit prune skipped");
        return Ok(report);
    }

    let deps_dirs: HashSet<&Path> = used
        .iter()
        .filter_map(|p| p.parent())
        .filter(|dir| dir.file_name().is_some_and(|n| n == "deps"))
        .collect();
    for dir in deps_dirs {
        let Some(profile) = dir.parent() else {
            continue;
        };
        let uplifted = uplifted_closure(profile)?;
        tracing::debug!(units = uplifted.len(), profile = %profile.display(), "uplifted closure");
        prune_deps_units(dir, &used, &uplifted, unused_before, &mut report)?;
        prune_build_dirs(
            &profile.join("build"),
            &used,
            &uplifted,
            unused_before,
            &mut report,
        )?;
        prune_orphan_fingerprints(profile, &uplifted, unused_before, &mut report)?;
    }
    Ok(report)
}

/// Remove `.fingerprint` dirs whose unit has no files left in `deps/` or `build/`, such as those an
/// older prune left behind. A unit with no output there (a `cargo doc` unit) follows the
/// `unused_before` rule like every other unit.
fn prune_orphan_fingerprints(
    profile: &Path,
    uplifted: &HashSet<String>,
    unused_before: SystemTime,
    report: &mut UnitPruneReport,
) -> io::Result<()> {
    let mut present: HashSet<String> = HashSet::new();
    for sub in ["deps", "build"] {
        for path in fingerprint_files(&profile.join(sub)) {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                let id = unit_id(name).unwrap_or(name);
                present.insert(unit_hash(id).to_owned());
            }
        }
    }
    for dir in fingerprint_files(&profile.join(".fingerprint")) {
        let Some(hash) = dir.file_name().and_then(|n| n.to_str()).map(unit_hash) else {
            continue;
        };
        if present.contains(hash) || uplifted.contains(hash) {
            continue;
        }
        let touched = last_touched(&fingerprint_files(&dir))?.unwrap_or(SystemTime::UNIX_EPOCH);
        if touched > unused_before {
            continue;
        }
        let bytes = dir_size(&dir);
        match fs::remove_dir_all(&dir) {
            Ok(()) => {
                report.removed_bytes += bytes;
                report.removed_fingerprints += 1;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The unit hashes (the `<hash>` of `<name>-<hash>`) of every artifact cargo uplifted into
/// `profile` (the file in `target/debug/` that is a hard link to one in `deps/`), and of all units
/// those depend on. The hash is the key because the name differs: `deps/` and `build/` use the
/// crate name (`actix_web`), `.fingerprint/` the package name (`actix-web`).
///
/// Cargo uplifts the outputs of the latest plain `cargo build` or `cargo run`, and a fresh build
/// rewrites nothing, so neither a record nor a file time shows that those units are in use. The
/// closure follows each unit's fingerprint JSON: a dependency is listed by its fingerprint hash,
/// which cargo also writes, as little-endian hex, to the dependency's own fingerprint file.
pub fn uplifted_closure(profile: &Path) -> io::Result<HashSet<String>> {
    use std::os::unix::fs::MetadataExt as _;
    let fingerprints = profile.join(".fingerprint");
    let mut by_inode = std::collections::HashMap::new();
    let deps = match fs::read_dir(profile.join("deps")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(e) => return Err(e),
    };
    for entry in deps {
        let entry = entry?;
        let name = entry.file_name();
        if let Some(id) = name.to_str().and_then(unit_id) {
            let meta = entry.metadata()?;
            by_inode.insert((meta.dev(), meta.ino()), unit_hash(id).to_owned());
        }
    }
    let mut roots = Vec::new();
    for entry in fs::read_dir(profile)? {
        let meta = entry?.metadata()?;
        if meta.is_file()
            && let Some(id) = by_inode.get(&(meta.dev(), meta.ino()))
        {
            roots.push(id.clone());
        }
    }

    let unit_dirs: Vec<String> = match fs::read_dir(&fingerprints) {
        Ok(entries) => entries
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(e) => return Err(e),
    };
    // Unit hash -> its fingerprint dir, and fingerprint hash (as cargo writes it) -> unit hash.
    let dir_of: std::collections::HashMap<&str, &String> =
        unit_dirs.iter().map(|d| (unit_hash(d), d)).collect();
    let mut by_hash = std::collections::HashMap::new();
    for unit in &unit_dirs {
        for file in fingerprint_files(&fingerprints.join(unit)) {
            let is_hash_file = file
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| !n.contains('.') && !n.starts_with("dep-"));
            if is_hash_file && let Ok(hash) = fs::read_to_string(&file) {
                by_hash.insert(hash.trim().to_owned(), unit_hash(unit).to_owned());
            }
        }
    }

    let mut closure = HashSet::new();
    while let Some(unit) = roots.pop() {
        if !closure.insert(unit.clone()) {
            continue;
        }
        let Some(dir) = dir_of.get(unit.as_str()) else {
            continue;
        };
        for file in fingerprint_files(&fingerprints.join(dir)) {
            if file.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(json) = fs::read(&file) else { continue };
            let Ok(fingerprint) = serde_json::from_slice::<FingerprintDeps>(&json) else {
                continue;
            };
            for dep in fingerprint.deps {
                let hash = hex_le(dep.3);
                match by_hash.get(&hash) {
                    Some(dep_unit) => roots.push(dep_unit.clone()),
                    None => tracing::debug!(unit, dep = dep.1, "dependency fingerprint not found"),
                }
            }
        }
    }
    Ok(closure)
}

/// The `<hash>` of a `<name>-<hash>` unit id or dir name.
fn unit_hash(id: &str) -> &str {
    id.rsplit_once('-').map_or(id, |(_, hash)| hash)
}

/// The part of a cargo fingerprint JSON the closure reads: each dependency is
/// `[package id hash, crate name, public, fingerprint hash]`.
#[derive(serde::Deserialize)]
struct FingerprintDeps {
    #[serde(default)]
    deps: Vec<(u64, String, bool, u64)>,
}

fn hex_le(hash: u64) -> String {
    hash.to_le_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn fingerprint_files(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| Some(e.ok()?.path()))
        .collect()
}

fn live_usage(usage_dir: &Path, unused_before: SystemTime) -> io::Result<HashSet<PathBuf>> {
    let mut used = HashSet::new();
    let entries = match fs::read_dir(usage_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(used),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        if fs::metadata(&path)?.modified()? < unused_before {
            tracing::debug!(record = %path.display(), "usage record expired");
            fs::remove_file(&path)?;
            continue;
        }
        let body = fs::read_to_string(&path)?;
        used.extend(
            body.lines()
                .filter(|l| !l.starts_with('#') && !l.is_empty())
                .map(PathBuf::from),
        );
    }
    Ok(used)
}

/// `<name>-<16 hex>` for a file in `deps/`: rlibs, rmetas, static libs and proc-macro shared
/// objects (`.so` on Linux, `.dylib` on macOS) carry a `lib` prefix, and `.d` / `.dwo` siblings
/// extend the unit name after a dot.
fn unit_id(name: &str) -> Option<&str> {
    let (base, ext) = name.split_once('.').unwrap_or((name, ""));
    let base = if matches!(ext, "rlib" | "rmeta" | "so" | "dylib" | "a") {
        base.strip_prefix("lib")?
    } else {
        base
    };
    let (_, hash) = base.rsplit_once('-')?;
    (hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some(base)
}

fn prune_deps_units(
    dir: &Path,
    used: &HashSet<PathBuf>,
    uplifted: &HashSet<String>,
    unused_before: SystemTime,
    report: &mut UnitPruneReport,
) -> io::Result<()> {
    let mut units: std::collections::HashMap<String, Vec<PathBuf>> =
        std::collections::HashMap::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if let Some(id) = path.file_name().and_then(|n| n.to_str()).and_then(unit_id) {
            units.entry(id.to_owned()).or_default().push(path);
        }
    }
    for (id, files) in &units {
        if uplifted.contains(unit_hash(id)) {
            continue;
        }
        if files.iter().any(|f| used.contains(f)) {
            continue;
        }
        let Some(touched) = last_touched(files)? else {
            continue;
        };
        if touched > unused_before {
            tracing::debug!(unit = %id, "kept: unrecorded but recent");
            continue;
        }
        tracing::debug!(unit = %id, "removing unit");
        let mut removed_any = false;
        for file in files {
            match fs::metadata(file).and_then(|m| fs::remove_file(file).map(|()| m.len())) {
                Ok(len) => {
                    report.removed_bytes += len;
                    removed_any = true;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        if removed_any {
            report.removed_units += 1;
        }
        remove_fingerprint(dir, id);
    }
    Ok(())
}

/// Remove the `.fingerprint` dir next to `deps` of the unit with id `id`. Without the unit's
/// files it describes nothing, and cargo rebuilds a unit that has no fingerprint. The dir is found
/// by hash, since it carries the package name and `id` the crate name.
fn remove_fingerprint(deps: &Path, id: &str) {
    let Some(fingerprints) = deps.parent().map(|profile| profile.join(".fingerprint")) else {
        return;
    };
    let hash = unit_hash(id);
    for dir in fingerprint_files(&fingerprints) {
        if dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| unit_hash(n) == hash)
            && let Err(e) = fs::remove_dir_all(&dir)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::debug!(dir = %dir.display(), "fingerprint dir not removed: {e}");
        }
    }
}

fn prune_build_dirs(
    build: &Path,
    used: &HashSet<PathBuf>,
    uplifted: &HashSet<String>,
    unused_before: SystemTime,
    report: &mut UnitPruneReport,
) -> io::Result<()> {
    let entries = match fs::read_dir(build) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let dir = entry?.path();
        if !dir.is_dir()
            || used.iter().any(|p| p.starts_with(&dir))
            || dir
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| uplifted.contains(unit_hash(n)))
        {
            continue;
        }
        let files: Vec<PathBuf> = fs::read_dir(&dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        let touched = last_touched(&files)?.unwrap_or(SystemTime::UNIX_EPOCH);
        if touched > unused_before {
            continue;
        }
        tracing::debug!(dir = %dir.display(), "removing build-script dir");
        let bytes = dir_size(&dir);
        match fs::remove_dir_all(&dir) {
            Ok(()) => {
                report.removed_bytes += bytes;
                report.removed_build_dirs += 1;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The latest mtime across `files`: when cargo last wrote them; `None` when every one is already
/// gone. atime is not used: a prune itself reads `.d` and fingerprint files, so on a `relatime`
/// mount every unit would look recently used, and on a `noatime` mount atime never moves. What is
/// in use is known from the usage records, the uplifted closure and the run lock instead.
fn last_touched(files: &[PathBuf]) -> io::Result<Option<SystemTime>> {
    let mut latest = None;
    for file in files {
        let meta = match fs::metadata(file) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        latest = latest.max(Some(meta.modified()?));
    }
    Ok(latest)
}

fn dir_size(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            _ => e.metadata().map(|m| m.len()).unwrap_or(0),
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_testing_utils::TempDirBuilder;
    use std::time::Duration;
    use tempfile::TempDir;

    struct Fixture {
        root: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let root = TempDirBuilder::new().build();
            fs::create_dir_all(root.path().join("target/debug/deps")).unwrap();
            Self { root }
        }

        fn deps(&self) -> PathBuf {
            self.root.path().join("target/debug/deps")
        }

        fn source(&self, rel: &str) {
            let path = self.root.path().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }

        /// An executable with its dep-info and one `.dwo`, rooted at the relative `root`.
        fn exe(&self, name: &str, root: &str) -> PathBuf {
            let exe = self.deps().join(name);
            fs::write(&exe, vec![0_u8; 100]).unwrap();
            fs::write(
                self.deps().join(format!("{name}.d")),
                format!("{}: {root} other.rs\n\n{root}:\n", exe.display()),
            )
            .unwrap();
            fs::write(self.deps().join(format!("{name}.cgu.0.rcgu.dwo")), "dwo").unwrap();
            exe
        }

        fn prune(&self, built: &[PathBuf]) -> PruneReport {
            let later = SystemTime::now() + Duration::from_secs(60);
            prune_stale_executables(self.root.path(), built, later).unwrap()
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(self.deps())
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }
    }

    const CDN: &str = "crates/cdn/tests/it/main.rs";
    const INDEXER: &str = "crates/indexer/tests/it/main.rs";

    #[test]
    fn removes_other_copies_of_a_built_target_with_their_siblings() {
        let f = Fixture::new();
        f.source(CDN);
        let current = f.exe("it-00000000000000aa", CDN);
        f.exe("it-00000000000000bb", CDN);

        let report = f.prune(&[current]);

        assert_eq!(report.removed_executables, 1);
        assert_eq!(report.removed_files.len(), 3);
        assert_eq!(
            f.names(),
            [
                "it-00000000000000aa",
                "it-00000000000000aa.cgu.0.rcgu.dwo",
                "it-00000000000000aa.d"
            ]
        );
    }

    #[test]
    fn removes_siblings_of_an_executable_already_gone() {
        let f = Fixture::new();
        let exe = f.exe("it-00000000000000bb", CDN);
        fs::remove_file(&exe).unwrap();

        let mut report = PruneReport::default();
        remove_with_siblings(&f.deps(), &exe, &mut report).unwrap();

        assert_eq!(report.removed_executables, 0);
        assert_eq!(report.removed_files.len(), 2);
        assert!(f.names().is_empty());
    }

    #[test]
    fn keeps_a_same_named_target_of_another_crate() {
        let f = Fixture::new();
        f.source(CDN);
        f.source(INDEXER);
        let current = f.exe("it-00000000000000aa", CDN);
        f.exe("it-00000000000000bb", INDEXER);

        assert_eq!(f.prune(&[current]), PruneReport::default());
    }

    #[test]
    fn removes_an_executable_whose_crate_root_is_gone() {
        let f = Fixture::new();
        f.source(CDN);
        let current = f.exe("it-00000000000000aa", CDN);
        f.exe("schemas-00000000000000cc", "crates/cdn/tests/schemas.rs");

        let report = f.prune(&[current]);

        assert_eq!(report.removed_executables, 1);
        assert!(!f.deps().join("schemas-00000000000000cc").exists());
    }

    #[test]
    fn keeps_a_copy_modified_within_the_grace_period() {
        let f = Fixture::new();
        f.source(CDN);
        let current = f.exe("it-00000000000000aa", CDN);
        f.exe("it-00000000000000bb", CDN);

        let keep_after = SystemTime::now() - Duration::from_secs(60);
        let report = prune_stale_executables(f.root.path(), &[current], keep_after).unwrap();

        assert_eq!(report, PruneReport::default());
    }

    #[test]
    fn keeps_a_copy_accessed_within_the_grace_period() {
        let f = Fixture::new();
        f.source(CDN);
        let current = f.exe("it-00000000000000aa", CDN);
        let old = f.exe("it-00000000000000bb", CDN);
        let now = SystemTime::now();
        let times = fs::FileTimes::new()
            .set_modified(now - Duration::from_secs(7200))
            .set_accessed(now);
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_times(times)
            .unwrap();

        let keep_after = now - Duration::from_secs(3600);
        let report = prune_stale_executables(f.root.path(), &[current], keep_after).unwrap();

        assert_eq!(report, PruneReport::default());
    }

    #[test]
    fn keeps_an_executable_without_dep_info_and_ignores_non_executables() {
        let f = Fixture::new();
        f.source(CDN);
        let current = f.exe("it-00000000000000aa", CDN);
        fs::write(f.deps().join("it-00000000000000bb"), "no .d").unwrap();
        fs::write(f.deps().join("libcdn-00000000000000dd.rlib"), "").unwrap();

        assert_eq!(f.prune(&[current]), PruneReport::default());
    }

    #[test]
    fn ignores_executables_outside_a_deps_directory() {
        let f = Fixture::new();
        f.source(CDN);
        f.exe("it-00000000000000bb", CDN);
        let uplifted = f.root.path().join("target/debug/it");

        assert_eq!(f.prune(&[uplifted]), PruneReport::default());
    }

    #[test]
    fn executable_stem_needs_a_sixteen_hex_hash_and_no_extension() {
        assert_eq!(
            executable_stem(Path::new("deps/it-0123456789abcdef")),
            Some("it")
        );
        assert_eq!(
            executable_stem(Path::new("deps/nextest-wrapper-0123456789abcdef")),
            Some("nextest-wrapper")
        );
        assert_eq!(
            executable_stem(Path::new("deps/it-0123456789abcdef.d")),
            None
        );
        assert_eq!(executable_stem(Path::new("deps/it-xyz")), None);
    }

    #[test]
    fn a_shared_run_lock_blocks_the_exclusive_prune_lock() {
        let f = Fixture::new();
        let target = f.root.path().join("target");

        let run = lock_run_shared(&target).unwrap();
        assert!(try_lock_run_exclusive(&target).unwrap().is_none());

        drop(run);
        assert!(try_lock_run_exclusive(&target).unwrap().is_some());
    }

    #[test]
    fn clean_target_dir_keeps_only_the_held_run_lock() {
        let f = Fixture::new();
        let target = f.root.path().join("target");
        fs::write(target.join("CACHEDIR.TAG"), "").unwrap();
        let lock = try_lock_run_exclusive(&target).unwrap().unwrap();

        clean_target_dir(&target).unwrap();

        let left: Vec<_> = fs::read_dir(&target)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, [RUN_LOCK]);
        // The lock file is the same inode, so the lock still excludes other runs.
        assert!(lock_run_shared_nonblocking(&target).is_none());
        drop(lock);
    }

    fn lock_run_shared_nonblocking(target: &Path) -> Option<fs::File> {
        let file = open_run_lock(target).unwrap();
        file.try_lock_shared().ok().map(|()| file)
    }

    mod units {
        use super::super::*;
        use irys_testing_utils::TempDirBuilder;
        use std::time::Duration;
        use tempfile::TempDir;

        const HOUR: Duration = Duration::from_secs(3600);

        struct Target {
            root: TempDir,
        }

        impl Target {
            fn new() -> Self {
                let root = TempDirBuilder::new().build();
                fs::create_dir_all(root.path().join("target/debug/deps")).unwrap();
                fs::create_dir_all(root.path().join("target/debug/build")).unwrap();
                Self { root }
            }

            fn target(&self) -> PathBuf {
                self.root.path().join("target")
            }

            fn deps(&self) -> PathBuf {
                self.target().join("debug/deps")
            }

            /// A library unit (rlib, rmeta, .d, .dwo) rooted at `root`, last touched `age` ago.
            fn lib(&self, id: &str, root: &str, age: Duration) -> PathBuf {
                let rlib = self.deps().join(format!("lib{id}.rlib"));
                let files = [
                    rlib.clone(),
                    self.deps().join(format!("lib{id}.rmeta")),
                    self.deps().join(format!("{id}.d")),
                    self.deps().join(format!("{id}.x.cgu.0.rcgu.dwo")),
                ];
                for f in &files {
                    fs::write(f, "unit").unwrap();
                }
                fs::write(
                    self.deps().join(format!("{id}.d")),
                    format!("{}: {root}\n", rlib.display()),
                )
                .unwrap();
                for f in &files {
                    age_file(f, age);
                }
                rlib
            }

            fn record(&self, artifacts: &[PathBuf]) {
                record_usage(&self.target(), "test", artifacts).unwrap();
            }

            fn prune(&self) -> UnitPruneReport {
                // Unused window 1 day, as the default.
                prune_unused_units(&self.target(), SystemTime::now() - 24 * HOUR).unwrap()
            }

            fn fingerprints(&self) -> PathBuf {
                self.target().join("debug/.fingerprint")
            }

            /// The `.fingerprint/<id>` dir cargo writes for a unit: its hash file holds `hash`
            /// (little-endian hex), and its JSON lists `deps` by their fingerprint hashes.
            fn fingerprint(&self, id: &str, kind: &str, hash: u64, deps: &[u64]) {
                let dir = self.fingerprints().join(id);
                fs::create_dir_all(&dir).unwrap();
                fs::write(dir.join(kind), hex_le(hash)).unwrap();
                let deps: Vec<_> = deps.iter().map(|h| (1_u64, "dep", false, *h)).collect();
                fs::write(
                    dir.join(format!("{kind}.json")),
                    serde_json::json!({ "deps": deps }).to_string(),
                )
                .unwrap();
            }

            /// An executable unit with its dep-info, last touched `age` ago.
            fn exe(&self, id: &str, age: Duration) -> PathBuf {
                let exe = self.deps().join(id);
                fs::write(&exe, "exe").unwrap();
                fs::write(
                    self.deps().join(format!("{id}.d")),
                    format!("{}: crates/gateway/src/main.rs\n", exe.display()),
                )
                .unwrap();
                age_file(&exe, age);
                age_file(&self.deps().join(format!("{id}.d")), age);
                exe
            }

            fn has(&self, name: &str) -> bool {
                self.deps().join(name).exists()
            }
        }

        fn age_file(path: &Path, age: Duration) {
            let then = SystemTime::now() - age;
            // Opened read-only so a directory can be aged too.
            fs::File::open(path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(then).set_accessed(then))
                .unwrap();
        }

        #[test]
        fn unit_id_groups_a_units_files() {
            for name in [
                "libfoo-0123456789abcdef.rlib",
                "libfoo-0123456789abcdef.rmeta",
                "libfoo-0123456789abcdef.so",
                "libfoo-0123456789abcdef.dylib",
                "libfoo-0123456789abcdef.a",
                "foo-0123456789abcdef.d",
                "foo-0123456789abcdef.foo.1a2b-cgu.0.rcgu.dwo",
                "foo-0123456789abcdef",
            ] {
                assert_eq!(unit_id(name), Some("foo-0123456789abcdef"), "{name}");
            }
            assert_eq!(
                unit_id("liblibc-0123456789abcdef.rlib"),
                Some("libc-0123456789abcdef")
            );
            assert_eq!(
                unit_id("libc-0123456789abcdef.d"),
                Some("libc-0123456789abcdef")
            );
            assert_eq!(unit_id("notes.txt"), None);
        }

        #[test]
        fn keeps_an_unrecorded_copy_of_a_used_crate_until_the_unused_window_passes() {
            // Another feature set of a recorded crate, as a plain `cargo build` or
            // `cargo nextest run -p` leaves it: in use, but no record names it.
            let t = Target::new();
            let used = t.lib(
                "serde-00000000000000aa",
                "/registry/serde-1.0.0/src/lib.rs",
                HOUR,
            );
            t.lib(
                "serde-00000000000000bb",
                "/registry/serde-1.0.0/src/lib.rs",
                HOUR,
            );
            t.lib(
                "serde-00000000000000cc",
                "/registry/serde-1.0.0/src/lib.rs",
                48 * HOUR,
            );
            t.record(&[used]);

            let report = t.prune();

            assert_eq!(report.removed_units, 1);
            assert!(t.has("libserde-00000000000000aa.rlib"));
            assert!(t.has("libserde-00000000000000bb.rlib"));
            assert!(!t.has("libserde-00000000000000cc.rlib"));
            assert!(!t.has("serde-00000000000000cc.d"));
            assert!(!t.has("serde-00000000000000cc.x.cgu.0.rcgu.dwo"));
        }

        #[test]
        fn keeps_another_version_until_the_unused_window_passes() {
            let t = Target::new();
            let used = t.lib(
                "serde-00000000000000aa",
                "/registry/serde-1.0.1/src/lib.rs",
                HOUR,
            );
            t.lib(
                "serde-00000000000000bb",
                "/registry/serde-1.0.0/src/lib.rs",
                HOUR,
            );
            t.lib(
                "serde-00000000000000cc",
                "/registry/serde-0.9.0/src/lib.rs",
                48 * HOUR,
            );
            t.record(&[used]);

            let report = t.prune();

            assert_eq!(report.removed_units, 1);
            assert!(t.has("libserde-00000000000000bb.rlib"));
            assert!(!t.has("libserde-00000000000000cc.rlib"));
        }

        #[test]
        fn removes_nothing_without_a_live_record_and_drops_expired_ones() {
            let t = Target::new();
            let used = t.lib(
                "serde-00000000000000aa",
                "/registry/serde-1.0.0/src/lib.rs",
                HOUR,
            );
            t.lib(
                "serde-00000000000000bb",
                "/registry/serde-1.0.0/src/lib.rs",
                48 * HOUR,
            );
            t.record(&[used]);
            let record = fs::read_dir(t.target().join(USAGE_DIR))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            age_file(&record, 48 * HOUR);

            assert_eq!(t.prune(), UnitPruneReport::default());
            assert!(!record.exists());
            assert!(t.has("libserde-00000000000000bb.rlib"));
        }

        #[test]
        fn removes_an_old_unrecorded_executable_and_its_fingerprint() {
            let t = Target::new();
            let used = t.lib(
                "serde-00000000000000aa",
                "/registry/serde-1.0.0/src/lib.rs",
                HOUR,
            );
            let old = t.exe("my_gw-00000000000000bb", 48 * HOUR);
            let recent = t.exe("my_gw-00000000000000cc", HOUR);
            t.fingerprint("my-gw-00000000000000bb", "bin-my_gw", 0xbb, &[]);
            t.record(&[used]);

            let report = t.prune();

            assert_eq!(report.removed_units, 1);
            assert!(!old.exists());
            assert!(!t.fingerprints().join("my-gw-00000000000000bb").exists());
            assert!(recent.exists());
        }

        #[test]
        fn keeps_an_uplifted_binary_and_everything_it_depends_on() {
            let t = Target::new();
            let used = t.lib(
                "serde-00000000000000aa",
                "/registry/serde-1.0.0/src/lib.rs",
                HOUR,
            );
            // `cargo build` hard-links its binary into the profile dir.
            let bin = t.exe("gateway-00000000000000b1", 48 * HOUR);
            fs::hard_link(&bin, t.target().join("debug/gateway")).unwrap();
            t.fingerprint("gateway-00000000000000b1", "bin-gateway", 0xb1, &[0xd1]);
            // A library it depends on, which depends on a build-script run. Its package name
            // has a hyphen: `.fingerprint/` and `build/` use it, `deps/` uses the crate name.
            t.lib(
                "my_dep-00000000000000d1",
                "crates/my-dep/src/lib.rs",
                48 * HOUR,
            );
            t.fingerprint("my-dep-00000000000000d1", "lib-my_dep", 0xd1, &[0xd2]);
            t.fingerprint(
                "my-dep-00000000000000d2",
                "run-build-script-build-script-build",
                0xd2,
                &[],
            );
            let out = t.target().join("debug/build/my-dep-00000000000000d2");
            fs::create_dir_all(&out).unwrap();
            fs::write(out.join("output"), "x").unwrap();
            age_file(&out.join("output"), 48 * HOUR);
            // An old unit nothing uses.
            t.lib("old-00000000000000e1", "crates/old/src/lib.rs", 48 * HOUR);
            t.record(&[used]);

            let report = t.prune();

            assert_eq!(report.removed_units, 1);
            assert_eq!(report.removed_build_dirs, 0);
            assert!(bin.exists());
            assert!(t.has("libmy_dep-00000000000000d1.rlib"));
            assert!(out.exists());
            assert!(!t.has("libold-00000000000000e1.rlib"));
        }

        #[test]
        fn removes_old_fingerprint_dirs_whose_unit_is_gone() {
            let t = Target::new();
            let used = t.lib("my_dep-00000000000000a1", "crates/my-dep/src/lib.rs", HOUR);
            t.fingerprint("my-dep-00000000000000a1", "lib-my_dep", 0xa1, &[]);
            t.fingerprint("gone-00000000000000a2", "lib-gone", 0xa2, &[]);
            t.fingerprint("fresh-00000000000000a3", "lib-fresh", 0xa3, &[]);
            for file in fingerprint_files(&t.fingerprints().join("gone-00000000000000a2")) {
                age_file(&file, 48 * HOUR);
            }
            t.record(&[used]);

            let report = t.prune();

            assert_eq!(report.removed_fingerprints, 1);
            assert!(t.fingerprints().join("my-dep-00000000000000a1").exists());
            assert!(!t.fingerprints().join("gone-00000000000000a2").exists());
            assert!(t.fingerprints().join("fresh-00000000000000a3").exists());
        }

        #[test]
        fn removes_an_old_unrecorded_build_script_dir() {
            let t = Target::new();
            let used = t.lib(
                "serde-00000000000000aa",
                "/registry/serde-1.0.0/src/lib.rs",
                HOUR,
            );
            let build = t.target().join("debug/build");
            let (kept, gone) = (
                build.join("ring-00000000000000aa"),
                build.join("ring-00000000000000bb"),
            );
            for dir in [&kept, &gone] {
                fs::create_dir_all(dir.join("out")).unwrap();
                fs::write(dir.join("output"), "x").unwrap();
                age_file(&dir.join("output"), 48 * HOUR);
                age_file(&dir.join("out"), 48 * HOUR);
            }
            t.record(&[used, kept.join("out")]);

            let report = t.prune();

            assert_eq!(report.removed_build_dirs, 1);
            assert!(kept.exists());
            assert!(!gone.exists());
        }
    }
}
