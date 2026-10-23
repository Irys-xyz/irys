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
        // `debug` or `release` may be a symlink while `deps` is a real directory.
        // Refuse that `deps` unless it resolves inside the canonical target, so
        // the deletes below cannot unlink files in the tree the profile points at.
        let Some(target_dir) = dir
            .parent()
            .and_then(|profile| profile.parent())
            .and_then(|target| target.canonicalize().ok())
        else {
            tracing::debug!(
                deps = %dir.display(),
                "executable prune skipped: target dir does not resolve"
            );
            continue;
        };
        match dir.canonicalize() {
            Ok(real) if real.starts_with(&target_dir) => {}
            Ok(real) => {
                tracing::debug!(
                    deps = %real.display(),
                    "skip deps: resolves outside the target dir"
                );
                continue;
            }
            Err(e) => {
                tracing::debug!(deps = %dir.display(), "skip deps: {e}");
                continue;
            }
        }
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

        let Some(entries) = open_real_dir(dir)? else {
            continue;
        };
        for entry in entries {
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
            remove_with_siblings(dir, &path, &target_dir, &mut report)?;
            if let Some(id) = path.file_name().and_then(|n| n.to_str()) {
                remove_fingerprint(dir, id, &target_dir);
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

fn remove_with_siblings(
    dir: &Path,
    exe: &Path,
    target_dir: &Path,
    report: &mut PruneReport,
) -> io::Result<()> {
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
        // `dir` may be reached through a profile symlink. Unlink only entries
        // whose parent resolves inside the canonical target.
        if !delete_inside(target_dir, &path) {
            continue;
        }
        // A file a concurrent prune already removed is skipped, not an error.
        if let Some(len) = remove_path(&path)? {
            exe_removed |= path == exe;
            report.removed_bytes += len;
            report.removed_files.push(path);
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

/// The profile subdirs that hold `<name>-<hash>` units: `deps/` for libraries, binaries and tests,
/// `examples/` for examples.
const UNIT_DIRS: [&str; 2] = ["deps", "examples"];

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

/// What [`prune_recorded`] removed. `None` from that function means no usage record was live.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RecordedPrune {
    pub executables: PruneReport,
    pub units: UnitPruneReport,
}

/// Prune from the usage records under `target_dir`.
///
/// The stale-executable pass treats every live record's `deps/` executable as the build it
/// keeps. The unit pass is the same one a `test` run finishes with. No live record means
/// nothing is known to be in use, so both passes leave the target directory alone. An expired
/// record is deleted by the liveness read and does not count as live.
pub fn prune_recorded(
    workspace_root: &Path,
    target_dir: &Path,
    keep_after: SystemTime,
    unused_before: SystemTime,
) -> io::Result<Option<RecordedPrune>> {
    let target_dir = match target_dir.canonicalize() {
        Ok(dir) => dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let used = live_usage(&target_dir.join(USAGE_DIR), &target_dir, unused_before)?;
    if used.is_empty() {
        return Ok(None);
    }
    let mut built: Vec<PathBuf> = used
        .iter()
        .filter(|path| {
            path.parent()
                .is_some_and(|dir| dir.file_name().is_some_and(|name| name == "deps"))
                && executable_stem(path).is_some()
        })
        .cloned()
        .collect();
    built.sort();
    let executables = prune_stale_executables(workspace_root, &built, keep_after)?;
    let units = prune_unused_units(&target_dir, unused_before)?;
    Ok(Some(RecordedPrune { executables, units }))
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
/// A unit is every file in a `deps/` or `examples/` directory that shares one `<name>-<hash>`: its
/// rlib, rmeta, proc-macro shared object or executable, its `.d` and its `.dwo` files, plus its
/// `.fingerprint/<name>-<hash>` dir. A unit is in use when a live usage record names one of its
/// files, or when it is in the dependency closure of an artifact cargo uplifted into the profile
/// dir (see [`uplifted_closure`]): that closure is what the latest plain `cargo build` or
/// `cargo run` uses, which no record covers. Any other unit is removed once cargo last wrote it, and
/// any executable of it last ran, before `unused_before`. A build-script directory under `build/` follows the same rules.
///
/// Records last written before `unused_before` expire and are deleted. With no live record the
/// pass removes nothing, since nothing is known to be in use.
///
/// A symlink at `deps`, `examples` or `build` is not followed. A symlinked `.fingerprint` is
/// walked when it resolves inside `target_dir`. When it resolves outside, or does not resolve,
/// unit deletion for that profile is skipped. A symlink at the usage directory is not a usage
/// directory: nothing there is read or deleted. A usage path that does not resolve inside
/// `target_dir` is ignored. A delete whose parent resolves outside `target_dir` is skipped.
pub fn prune_unused_units(
    target_dir: &Path,
    unused_before: SystemTime,
) -> io::Result<UnitPruneReport> {
    let mut report = UnitPruneReport::default();
    let target_dir = match target_dir.canonicalize() {
        Ok(dir) => dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            tracing::debug!("target dir does not exist; unit prune skipped");
            return Ok(report);
        }
        Err(e) => return Err(e),
    };
    let used = live_usage(&target_dir.join(USAGE_DIR), &target_dir, unused_before)?;
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
        // `dir` came from a resolved usage path, so this profile should already lie inside
        // the target. Resolve it again and refuse it when it does not.
        let profile = match profile.canonicalize() {
            Ok(profile) if profile.starts_with(&target_dir) => profile,
            Ok(profile) => {
                tracing::debug!(
                    profile = %profile.display(),
                    "skip profile: resolves outside the target dir"
                );
                continue;
            }
            Err(e) => {
                tracing::debug!(profile = %profile.display(), "skip profile: {e}");
                continue;
            }
        };
        // `None`: `.fingerprint` is outside the target or does not resolve. Pruning from
        // the uplifted roots alone would drop dependency units the graph would have kept.
        let Some(uplifted) = uplifted_closure(&profile, &target_dir)? else {
            tracing::debug!(
                profile = %profile.display(),
                "skip profile: fingerprint dir is not inside the target"
            );
            continue;
        };
        tracing::debug!(units = uplifted.len(), profile = %profile.display(), "uplifted closure");
        for unit_dir in UNIT_DIRS {
            prune_deps_units(
                &profile.join(unit_dir),
                &target_dir,
                &used,
                &uplifted,
                unused_before,
                &mut report,
            )?;
        }
        prune_build_dirs(
            &profile.join("build"),
            &target_dir,
            &used,
            &uplifted,
            unused_before,
            &mut report,
        )?;
        prune_orphan_fingerprints(&profile, &target_dir, &uplifted, unused_before, &mut report)?;
    }
    Ok(report)
}

/// Remove `.fingerprint` dirs whose unit has no files left in `deps/` or `build/`, such as those an
/// older prune left behind. A unit with no output there (a `cargo doc` unit) follows the
/// `unused_before` rule like every other unit.
fn prune_orphan_fingerprints(
    profile: &Path,
    target_dir: &Path,
    uplifted: &HashSet<String>,
    unused_before: SystemTime,
    report: &mut UnitPruneReport,
) -> io::Result<()> {
    let mut present: HashSet<String> = HashSet::new();
    for sub in UNIT_DIRS.iter().chain(&["build"]) {
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
        if !delete_inside(target_dir, &dir) {
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
///
/// Returns `None` when `.fingerprint` is a symlink that resolves outside `target_dir` or does
/// not resolve. The caller skips unit deletion for that profile: a closure of only the uplifted
/// roots would drop dependency units the graph would have kept. A symlink that resolves inside
/// `target_dir` is walked. A missing fingerprint directory still returns only the uplifted roots.
pub fn uplifted_closure(profile: &Path, target_dir: &Path) -> io::Result<Option<HashSet<String>>> {
    use std::os::unix::fs::MetadataExt as _;
    let mut fingerprints = profile.join(".fingerprint");
    // Hashed unit files: libraries and binaries in `deps/`, examples in `examples/`.
    let mut by_inode = std::collections::HashMap::new();
    for dir in UNIT_DIRS {
        for path in fingerprint_files(&profile.join(dir)) {
            if let Some(id) = path.file_name().and_then(|n| n.to_str()).and_then(unit_id) {
                let meta = fs::metadata(&path)?;
                by_inode.insert((meta.dev(), meta.ino()), unit_hash(id).to_owned());
            }
        }
    }
    // Uplifted files are the unhashed names (`target/debug/irys`, `examples/dupsort`).
    let mut roots = Vec::new();
    for dir in [profile.to_path_buf(), profile.join("examples")] {
        for path in fingerprint_files(&dir) {
            let uplifted_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| unit_id(n).is_none());
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            if uplifted_name
                && meta.is_file()
                && let Some(id) = by_inode.get(&(meta.dev(), meta.ino()))
            {
                roots.push(id.clone());
            }
        }
    }

    match fs::symlink_metadata(&fingerprints) {
        Ok(meta) if meta.file_type().is_symlink() => match fingerprints.canonicalize() {
            Ok(real) if real.starts_with(target_dir) => fingerprints = real,
            Ok(real) => {
                tracing::debug!(
                    fingerprints = %real.display(),
                    "fingerprint dir resolves outside the target dir"
                );
                return Ok(None);
            }
            Err(e) => {
                tracing::debug!(
                    fingerprints = %fingerprints.display(),
                    "fingerprint dir does not resolve: {e}"
                );
                return Ok(None);
            }
        },
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Without fingerprints cargo rebuilds everything anyway; still keep the roots.
            return Ok(Some(roots.into_iter().collect()));
        }
        Err(e) => return Err(e),
    }

    let unit_dirs: Vec<String> = match open_real_dir(&fingerprints)? {
        Some(entries) => entries
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect(),
        // The directory disappeared after the metadata check. Keep the roots.
        None => return Ok(Some(roots.into_iter().collect())),
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
    Ok(Some(closure))
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
    let Ok(Some(entries)) = open_real_dir(dir) else {
        return Vec::new();
    };
    entries.filter_map(|e| e.ok().map(|e| e.path())).collect()
}

/// `target_dir` is canonical. Only lines that resolve inside it are returned, and the stored
/// path is that resolved path, so a record cannot select a directory outside the target.
///
/// A symlink at `usage_dir` is not a usage directory. Following it would delete aged files in
/// the directory it points at, and would let that directory name units to prune. Nothing is
/// read and nothing is deleted.
fn live_usage(
    usage_dir: &Path,
    target_dir: &Path,
    unused_before: SystemTime,
) -> io::Result<HashSet<PathBuf>> {
    let mut used = HashSet::new();
    let Some(entries) = open_real_dir(usage_dir)? else {
        return Ok(used);
    };
    for entry in entries {
        let path = entry?.path();
        if fs::metadata(&path)?.modified()? < unused_before {
            tracing::debug!(record = %path.display(), "usage record expired");
            fs::remove_file(&path)?;
            continue;
        }
        let body = fs::read_to_string(&path)?;
        for line in body.lines() {
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            if let Some(resolved) = usage_path_in_target(target_dir, line) {
                used.insert(resolved);
            }
        }
    }
    Ok(used)
}

fn usage_path_in_target(target_dir: &Path, line: &str) -> Option<PathBuf> {
    let path = PathBuf::from(line);
    match path.canonicalize() {
        Ok(resolved) if resolved.starts_with(target_dir) => Some(resolved),
        Ok(resolved) => {
            tracing::debug!(
                path = %resolved.display(),
                "usage path resolves outside the target dir"
            );
            None
        }
        Err(e) => {
            tracing::debug!(
                path = %path.display(),
                "usage path does not resolve inside the target dir: {e}"
            );
            None
        }
    }
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
    target_dir: &Path,
    used: &HashSet<PathBuf>,
    uplifted: &HashSet<String>,
    unused_before: SystemTime,
    report: &mut UnitPruneReport,
) -> io::Result<()> {
    let mut units: std::collections::HashMap<String, Vec<PathBuf>> =
        std::collections::HashMap::new();
    let Some(entries) = open_real_dir(dir)? else {
        return Ok(());
    };
    for entry in entries {
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
        if touched.max(last_run(files)?) > unused_before {
            tracing::debug!(unit = %id, "kept: unrecorded but recent");
            continue;
        }
        tracing::debug!(unit = %id, "removing unit");
        let mut removed_any = false;
        for file in files {
            if !delete_inside(target_dir, file) {
                continue;
            }
            if let Some(len) = remove_path(file)? {
                report.removed_bytes += len;
                removed_any = true;
            }
        }
        // Only a unit this pass removed loses its fingerprint; one whose files were all already
        // gone is left to the orphan sweep.
        if removed_any {
            report.removed_units += 1;
            remove_fingerprint(dir, id, target_dir);
        }
    }
    Ok(())
}

/// Remove the `.fingerprint` dir next to `deps` of the unit with id `id`. Without the unit's
/// files it describes nothing, and cargo rebuilds a unit that has no fingerprint. The dir is found
/// by hash, since it carries the package name and `id` the crate name.
fn remove_fingerprint(deps: &Path, id: &str, target_dir: &Path) {
    let Some(fingerprints) = deps.parent().map(|profile| profile.join(".fingerprint")) else {
        return;
    };
    let hash = unit_hash(id);
    for dir in fingerprint_files(&fingerprints) {
        if dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| unit_hash(n) == hash)
        {
            if !delete_inside(target_dir, &dir) {
                continue;
            }
            if let Err(e) = fs::remove_dir_all(&dir)
                && e.kind() != io::ErrorKind::NotFound
            {
                tracing::debug!(dir = %dir.display(), "fingerprint dir not removed: {e}");
            }
        }
    }
}

fn prune_build_dirs(
    build: &Path,
    target_dir: &Path,
    used: &HashSet<PathBuf>,
    uplifted: &HashSet<String>,
    unused_before: SystemTime,
    report: &mut UnitPruneReport,
) -> io::Result<()> {
    let Some(entries) = open_real_dir(build)? else {
        return Ok(());
    };
    for entry in entries {
        let dir = entry?.path();
        if !delete_inside(target_dir, &dir) {
            continue;
        }
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

/// The latest atime of the executables among `files` (extensionless: a test or bin binary). Unlike
/// a `.d` or fingerprint file, a prune never reads an executable, so where atime moves (`relatime`)
/// it shows a recent run, such as a plain `cargo nextest run` that no record covers.
fn last_run(files: &[PathBuf]) -> io::Result<SystemTime> {
    let mut latest = SystemTime::UNIX_EPOCH;
    for file in files {
        if file
            .file_name()
            .is_some_and(|n| n.to_string_lossy().contains('.'))
        {
            continue;
        }
        match fs::metadata(file) {
            Ok(meta) => latest = latest.max(meta.accessed()?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(latest)
}

/// The latest mtime across `files`: when cargo last wrote them; `None` when every one is already
/// gone. atime is not used here: a prune itself reads `.d` and fingerprint files, so on a
/// `relatime` mount every unit would look recently used, and on a `noatime` mount atime never
/// moves. What is in use is known from the usage records, the uplifted closure, the run lock and
/// an executable's own atime ([`last_run`]).
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

/// List `dir` only when it is a real directory.
///
/// A symlink is not opened. `deps`, `examples`, `build` and `.fingerprint` are reached through
/// this, and following one would list whatever tree it points at. `Ok(None)` means the directory
/// is missing or is a symlink.
fn open_real_dir(dir: &Path) -> io::Result<Option<fs::ReadDir>> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            tracing::debug!(dir = %dir.display(), "not followed: symlinked directory");
            Ok(None)
        }
        Ok(_) => Ok(Some(fs::read_dir(dir)?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// `true` when `path`'s parent resolves inside the canonical `target_dir`.
///
/// The last component is not resolved, so a symlink there is unlinked and not followed.
/// A parent outside the target is skipped and logged.
fn delete_inside(target_dir: &Path, path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        tracing::debug!(path = %path.display(), "skip delete: path has no parent");
        return false;
    };
    match parent.canonicalize() {
        Ok(resolved) if resolved.starts_with(target_dir) => true,
        Ok(resolved) => {
            tracing::debug!(
                path = %path.display(),
                parent = %resolved.display(),
                "skip delete: parent resolves outside the target dir"
            );
            false
        }
        Err(e) => {
            tracing::debug!(
                path = %path.display(),
                "skip delete: parent does not resolve inside the target dir: {e}"
            );
            false
        }
    }
}

/// Remove a unit's file, or a directory such as the `<name>-<hash>.dSYM` bundle rustc writes on
/// macOS with `split-debuginfo = "packed"`. Returns the bytes freed, or `None` when it was already
/// gone (a concurrent prune).
fn remove_path(path: &Path) -> io::Result<Option<u64>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let (len, removed) = if meta.is_dir() {
        (dir_size(path), fs::remove_dir_all(path))
    } else {
        (meta.len(), fs::remove_file(path))
    };
    match removed {
        Ok(()) => Ok(Some(len)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
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
        let target = f.root.path().join("target").canonicalize().unwrap();
        remove_with_siblings(&f.deps(), &exe, &target, &mut report).unwrap();

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
    fn does_not_delete_executables_through_a_symlinked_profile() {
        let f = Fixture::new();
        f.source(CDN);
        // `debug` points at a profile outside the target. `deps` there is a real
        // directory, which is the shape cargo reports through the link.
        let outside = TempDirBuilder::new().build();
        let outside_debug = outside.path().join("debug");
        fs::rename(f.root.path().join("target/debug"), &outside_debug).unwrap();
        std::os::unix::fs::symlink(&outside_debug, f.root.path().join("target/debug")).unwrap();
        let current = f.exe("it-00000000000000aa", CDN);
        let stale = f.exe("it-00000000000000bb", CDN);

        let report = f.prune(std::slice::from_ref(&current));

        assert!(
            stale.exists(),
            "stale executable outside the target was deleted"
        );
        assert!(outside_debug.join("deps/it-00000000000000bb.d").exists());
        assert!(
            outside_debug
                .join("deps/it-00000000000000bb.cgu.0.rcgu.dwo")
                .exists()
        );
        assert!(current.exists());
        assert_eq!(report, PruneReport::default());
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
    fn prune_recorded_removes_a_superseded_copy_named_by_a_usage_record() {
        let f = Fixture::new();
        f.source(CDN);
        let current = f.exe("it-00000000000000aa", CDN);
        f.exe("it-00000000000000bb", CDN);
        let rlib = f.deps().join("libcdn-00000000000000dd.rlib");
        fs::write(&rlib, "rlib").unwrap();
        let target = f.root.path().join("target");
        record_usage(&target, "test", &[current.clone(), rlib.clone()]).unwrap();

        let report = prune_recorded(
            f.root.path(),
            &target,
            SystemTime::now() + Duration::from_secs(60),
            SystemTime::UNIX_EPOCH,
        )
        .unwrap()
        .expect("a live usage record");

        assert_eq!(report.executables.removed_executables, 1);
        assert!(current.exists());
        assert!(!f.deps().join("it-00000000000000bb").exists());
        assert!(rlib.exists());
    }

    #[test]
    fn prune_recorded_leaves_files_when_the_only_usage_record_is_expired() {
        let f = Fixture::new();
        f.source(CDN);
        let old = f.exe("it-00000000000000bb", CDN);
        let target = f.root.path().join("target");
        record_usage(&target, "test", std::slice::from_ref(&old)).unwrap();
        let record = fs::read_dir(target.join(USAGE_DIR))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let then = SystemTime::now() - Duration::from_secs(48 * 3600);
        fs::File::open(&record)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(then).set_accessed(then))
            .unwrap();

        let outcome = prune_recorded(
            f.root.path(),
            &target,
            SystemTime::now() + Duration::from_secs(60),
            SystemTime::now() - Duration::from_secs(24 * 3600),
        )
        .unwrap();

        assert!(outcome.is_none());
        assert!(old.exists());
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
        fn prune_recorded_removes_an_old_unit_when_the_record_names_no_executable() {
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
            t.record(std::slice::from_ref(&used));

            let report = prune_recorded(
                t.root.path(),
                &t.target(),
                SystemTime::now() + HOUR,
                SystemTime::now() - 24 * HOUR,
            )
            .unwrap()
            .expect("a live usage record");

            assert_eq!(report.executables.removed_executables, 0);
            assert_eq!(report.units.removed_units, 1);
            assert!(used.exists());
            assert!(!t.has("libserde-00000000000000bb.rlib"));
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
        fn keeps_an_uplifted_example_and_its_dependencies_and_prunes_old_examples() {
            let t = Target::new();
            let used = t.lib(
                "serde-00000000000000aa",
                "/registry/serde-1.0.0/src/lib.rs",
                HOUR,
            );
            let examples = t.target().join("debug/examples");
            fs::create_dir_all(&examples).unwrap();
            let current = examples.join("dupsort-00000000000000c1");
            let old = examples.join("dupsort-00000000000000c0");
            for exe in [&current, &old] {
                fs::write(exe, "exe").unwrap();
                age_file(exe, 48 * HOUR);
            }
            // `cargo run --example dupsort` hard-links the example as `examples/dupsort`.
            fs::hard_link(&current, examples.join("dupsort")).unwrap();
            t.fingerprint("dupsort-00000000000000c1", "example-dupsort", 0xc1, &[0xd1]);
            t.fingerprint("dupsort-00000000000000c0", "example-dupsort", 0xc0, &[]);
            t.lib(
                "dev_dep-00000000000000d1",
                "crates/dev-dep/src/lib.rs",
                48 * HOUR,
            );
            t.fingerprint("dev-dep-00000000000000d1", "lib-dev_dep", 0xd1, &[]);
            t.record(&[used]);

            let report = t.prune();

            assert!(current.exists());
            assert!(t.fingerprints().join("dupsort-00000000000000c1").exists());
            assert!(t.has("libdev_dep-00000000000000d1.rlib"));
            assert!(!old.exists());
            assert!(!t.fingerprints().join("dupsort-00000000000000c0").exists());
            assert_eq!(report.removed_units, 1);
        }

        #[test]
        fn removes_a_units_dsym_directory() {
            let t = Target::new();
            let used = t.lib(
                "serde-00000000000000aa",
                "/registry/serde-1.0.0/src/lib.rs",
                HOUR,
            );
            let exe = t.exe("it-00000000000000b2", 48 * HOUR);
            let dsym = t
                .deps()
                .join("it-00000000000000b2.dSYM/Contents/Resources/DWARF");
            fs::create_dir_all(&dsym).unwrap();
            fs::write(dsym.join("it-00000000000000b2"), "dwarf").unwrap();
            age_file(&t.deps().join("it-00000000000000b2.dSYM"), 48 * HOUR);
            t.record(&[used]);

            assert_eq!(t.prune().removed_units, 1);
            assert!(!exe.exists());
            assert!(!t.deps().join("it-00000000000000b2.dSYM").exists());
        }

        #[test]
        fn keeps_an_old_executable_that_ran_recently() {
            let t = Target::new();
            let used = t.lib(
                "serde-00000000000000aa",
                "/registry/serde-1.0.0/src/lib.rs",
                HOUR,
            );
            // Built two days ago, run a minute ago by a plain `cargo nextest run`.
            let exe = t.exe("it-00000000000000b1", 48 * HOUR);
            fs::File::open(&exe)
                .unwrap()
                .set_times(fs::FileTimes::new().set_accessed(SystemTime::now()))
                .unwrap();
            t.record(&[used]);

            assert_eq!(t.prune().removed_units, 0);
            assert!(exe.exists());
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

        #[test]
        fn does_not_follow_a_symlinked_build_dir() {
            let t = Target::new();
            let used = t.lib(
                "serde-00000000000000aa",
                "/registry/serde-1.0.0/src/lib.rs",
                HOUR,
            );
            t.lib("old-00000000000000bb", "crates/old/src/lib.rs", 48 * HOUR);

            // `debug/build` points at another tree. An old directory there must survive.
            let outside = TempDirBuilder::new().build();
            let victim = outside.path().join("ring-00000000000000cc");
            fs::create_dir_all(&victim).unwrap();
            let marker = victim.join("output");
            fs::write(&marker, "keep").unwrap();
            age_file(&marker, 48 * HOUR);
            let build = t.target().join("debug/build");
            fs::remove_dir_all(&build).unwrap();
            std::os::unix::fs::symlink(outside.path(), &build).unwrap();
            t.record(&[used]);

            let report = t.prune();

            assert!(
                marker.exists(),
                "symlinked build dir was followed and its contents were deleted"
            );
            assert!(
                fs::symlink_metadata(&build)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(report.removed_build_dirs, 0);
            assert!(!t.has("libold-00000000000000bb.rlib"));
            assert!(t.has("libserde-00000000000000aa.rlib"));
        }

        #[test]
        fn ignores_a_usage_path_outside_the_target_dir() {
            let t = Target::new();
            let used = t.lib(
                "serde-00000000000000aa",
                "/registry/serde-1.0.0/src/lib.rs",
                HOUR,
            );
            t.lib("old-00000000000000bb", "crates/old/src/lib.rs", 48 * HOUR);

            let outside = TempDirBuilder::new().build();
            let outside_deps = outside.path().join("deps");
            fs::create_dir_all(&outside_deps).unwrap();
            let named = outside_deps.join("libkeep-0123456789abcdef.rlib");
            let old = outside_deps.join("libold-0123456789abcdef.rlib");
            fs::write(&named, "x").unwrap();
            fs::write(&old, "x").unwrap();
            age_file(&old, 48 * HOUR);
            t.record(&[used, named.clone()]);

            t.prune();

            assert!(
                old.exists(),
                "usage path outside the target dir selected that tree for pruning"
            );
            assert!(named.exists());
            assert!(!t.has("libold-00000000000000bb.rlib"));
            assert!(t.has("libserde-00000000000000aa.rlib"));
        }

        #[test]
        fn does_not_delete_expired_records_through_a_symlinked_usage_dir() {
            let t = Target::new();
            let kept = t.lib(
                "serde-00000000000000aa",
                "/registry/serde-1.0.0/src/lib.rs",
                HOUR,
            );
            t.lib("old-00000000000000bb", "crates/old/src/lib.rs", 48 * HOUR);

            // `xtask-usage` points outside the target. An aged file there must
            // survive, and a live record there must not select units to prune.
            let outside = TempDirBuilder::new().build();
            let expired = outside.path().join("expired.txt");
            fs::write(&expired, "# old\n").unwrap();
            age_file(&expired, 48 * HOUR);
            fs::write(
                outside.path().join("live.txt"),
                format!("# live\n{}\n", kept.display()),
            )
            .unwrap();
            std::os::unix::fs::symlink(outside.path(), t.target().join(USAGE_DIR)).unwrap();

            let report = t.prune();

            assert!(
                expired.exists(),
                "expired usage record outside the target was deleted"
            );
            assert!(
                t.has("libold-00000000000000bb.rlib"),
                "symlinked usage dir was read and its record selected units for pruning"
            );
            assert!(t.has("libserde-00000000000000aa.rlib"));
            assert_eq!(report, UnitPruneReport::default());
        }

        #[test]
        fn keeps_dependency_units_when_the_fingerprint_dir_is_symlinked_inside_the_target() {
            let t = Target::new();
            let used = t.lib(
                "serde-00000000000000aa",
                "/registry/serde-1.0.0/src/lib.rs",
                HOUR,
            );
            let bin = t.exe("gateway-00000000000000b1", 48 * HOUR);
            fs::hard_link(&bin, t.target().join("debug/gateway")).unwrap();
            t.fingerprint("gateway-00000000000000b1", "bin-gateway", 0xb1, &[0xd1]);
            t.lib(
                "my_dep-00000000000000d1",
                "crates/my-dep/src/lib.rs",
                48 * HOUR,
            );
            t.fingerprint("my-dep-00000000000000d1", "lib-my_dep", 0xd1, &[]);
            t.lib("old-00000000000000e1", "crates/old/src/lib.rs", 48 * HOUR);
            // The fingerprint tree stays inside the target, but `.fingerprint` is a symlink.
            let real = t.target().join("debug/fp-real");
            fs::rename(t.fingerprints(), &real).unwrap();
            std::os::unix::fs::symlink(&real, t.fingerprints()).unwrap();
            t.record(&[used]);

            let report = t.prune();

            assert!(
                t.has("libmy_dep-00000000000000d1.rlib"),
                "dependency unit was pruned from a root-only closure"
            );
            assert!(bin.exists());
            assert!(!t.has("libold-00000000000000e1.rlib"));
            assert_eq!(report.removed_units, 1);
        }

        #[test]
        fn skips_unit_deletion_when_the_fingerprint_dir_points_outside_the_target() {
            let t = Target::new();
            let used = t.lib(
                "serde-00000000000000aa",
                "/registry/serde-1.0.0/src/lib.rs",
                HOUR,
            );
            let bin = t.exe("gateway-00000000000000b1", 48 * HOUR);
            fs::hard_link(&bin, t.target().join("debug/gateway")).unwrap();
            t.fingerprint("gateway-00000000000000b1", "bin-gateway", 0xb1, &[0xd1]);
            t.lib(
                "my_dep-00000000000000d1",
                "crates/my-dep/src/lib.rs",
                48 * HOUR,
            );
            t.fingerprint("my-dep-00000000000000d1", "lib-my_dep", 0xd1, &[]);
            t.lib("old-00000000000000e1", "crates/old/src/lib.rs", 48 * HOUR);
            let outside = TempDirBuilder::new().build();
            let real = outside.path().join("fingerprint");
            fs::rename(t.fingerprints(), &real).unwrap();
            std::os::unix::fs::symlink(&real, t.fingerprints()).unwrap();
            let marker = real.join("keep.txt");
            fs::write(&marker, "keep").unwrap();
            t.record(&[used]);

            let report = t.prune();

            assert!(
                t.has("libmy_dep-00000000000000d1.rlib"),
                "dependency unit was pruned from a root-only closure"
            );
            assert!(
                t.has("libold-00000000000000e1.rlib"),
                "unit deletion ran for a profile whose fingerprint dir is outside the target"
            );
            assert!(bin.exists());
            assert!(marker.exists());
            assert_eq!(report.removed_units, 0);
        }

        #[test]
        fn skips_unit_deletion_when_the_fingerprint_dir_does_not_resolve() {
            let t = Target::new();
            let used = t.lib(
                "serde-00000000000000aa",
                "/registry/serde-1.0.0/src/lib.rs",
                HOUR,
            );
            t.lib("old-00000000000000bb", "crates/old/src/lib.rs", 48 * HOUR);
            std::os::unix::fs::symlink(t.target().join("missing-fp"), t.fingerprints()).unwrap();
            t.record(&[used]);

            let report = t.prune();

            assert!(
                t.has("libold-00000000000000bb.rlib"),
                "unit deletion ran from a root-only closure"
            );
            assert_eq!(report.removed_units, 0);
        }
    }
}
