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
    pub removed_bytes: u64,
}

/// Remove compilation units that no live usage record names.
///
/// A unit is every file in a `deps/` directory that shares one `<name>-<hash>`: its rlib, rmeta or
/// proc-macro `.so`, its `.d`, and its `.dwo` files. Executables are left to
/// [`prune_stale_executables`]. A unit no record names is removed when it was last touched before
/// `keep_after` and either duplicates a recorded unit (same name and crate root, another hash: a
/// different feature set or profile of the same crate) or was last touched before
/// `unused_before`. A build-script directory under `build/` follows the `unused_before` rule.
///
/// Records last written before `unused_before` expire and are deleted. With no live record the
/// pass removes nothing, since nothing is known to be in use.
pub fn prune_unused_units(
    target_dir: &Path,
    workspace_root: &Path,
    keep_after: SystemTime,
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
        prune_deps_units(
            dir,
            workspace_root,
            &used,
            keep_after,
            unused_before,
            &mut report,
        )?;
        if let Some(build) = dir.parent().map(|profile| profile.join("build")) {
            prune_build_dirs(&build, &used, unused_before, &mut report)?;
        }
    }
    Ok(report)
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
    workspace_root: &Path,
    used: &HashSet<PathBuf>,
    keep_after: SystemTime,
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
    let unit_key = |id: &str| -> Option<TargetKey> {
        let dep_info = fs::read_to_string(dir.join(format!("{id}.d"))).ok()?;
        Some(TargetKey {
            stem: id.rsplit_once('-')?.0.to_owned(),
            root: workspace_root.join(crate_root(&dep_info)?),
        })
    };
    let used_ids: HashSet<&str> = units
        .iter()
        .filter(|(_, files)| files.iter().any(|f| used.contains(f)))
        .map(|(id, _)| id.as_str())
        .collect();
    let used_keys: HashSet<TargetKey> = used_ids.iter().filter_map(|id| unit_key(id)).collect();

    for (id, files) in &units {
        if used_ids.contains(id.as_str()) {
            continue;
        }
        // An executable belongs to the executable prune, which knows whether a run superseded it.
        if files.iter().any(|f| {
            f.file_name()
                .is_some_and(|n| !n.to_string_lossy().contains('.'))
        }) {
            continue;
        }
        let Some(touched) = last_touched(files)? else {
            continue;
        };
        if touched > keep_after {
            tracing::debug!(unit = %id, "kept: used within the grace period");
            continue;
        }
        let duplicate = unit_key(id).is_some_and(|key| used_keys.contains(&key));
        if !duplicate && touched > unused_before {
            tracing::debug!(unit = %id, "kept: unrecorded but recent");
            continue;
        }
        tracing::debug!(unit = %id, duplicate, "removing unit");
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
    }
    Ok(())
}

fn prune_build_dirs(
    build: &Path,
    used: &HashSet<PathBuf>,
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
        if !dir.is_dir() || used.iter().any(|p| p.starts_with(&dir)) {
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

/// The latest mtime or atime across `files`; `None` when every one is already gone.
fn last_touched(files: &[PathBuf]) -> io::Result<Option<SystemTime>> {
    let mut latest = None;
    for file in files {
        let meta = match fs::metadata(file) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let touched = meta.modified()?.max(meta.accessed()?);
        latest = latest.max(Some(touched));
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
                let now = SystemTime::now();
                // Grace 30 minutes, unused window 1 day, as the defaults.
                prune_unused_units(
                    &self.target(),
                    self.root.path(),
                    now - Duration::from_secs(1800),
                    now - 24 * HOUR,
                )
                .unwrap()
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
        fn removes_a_duplicate_of_a_used_unit_after_the_grace_period() {
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
            t.record(&[used]);

            let report = t.prune();

            assert_eq!(report.removed_units, 1);
            assert!(t.has("libserde-00000000000000aa.rlib"));
            assert!(!t.has("libserde-00000000000000bb.rlib"));
            assert!(!t.has("serde-00000000000000bb.d"));
            assert!(!t.has("serde-00000000000000bb.x.cgu.0.rcgu.dwo"));
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
        fn keeps_a_duplicate_touched_within_the_grace_period() {
            let t = Target::new();
            let used = t.lib(
                "serde-00000000000000aa",
                "/registry/serde-1.0.0/src/lib.rs",
                HOUR,
            );
            t.lib(
                "serde-00000000000000bb",
                "/registry/serde-1.0.0/src/lib.rs",
                Duration::ZERO,
            );
            t.record(&[used]);

            assert_eq!(t.prune(), UnitPruneReport::default());
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
        fn leaves_executables_to_the_executable_prune() {
            let t = Target::new();
            let used = t.lib("it-00000000000000aa", "crates/cdn/tests/it/main.rs", HOUR);
            let exe = t.deps().join("it-00000000000000bb");
            fs::write(&exe, "exe").unwrap();
            fs::write(
                t.deps().join("it-00000000000000bb.d"),
                format!("{}: crates/cdn/tests/it/main.rs\n", exe.display()),
            )
            .unwrap();
            age_file(&exe, 48 * HOUR);
            t.record(&[used]);

            t.prune();

            assert!(exe.exists());
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
