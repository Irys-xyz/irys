use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=GIT_SHA");
    println!("cargo:rerun-if-env-changed=GIT_HAS_TAG");
    println!("cargo:rerun-if-env-changed=GIT_DIRTY");

    // When building outside a git repo (e.g. Docker), accept git metadata via env
    // vars and skip all git detection. build_image.sh captures these from the host.
    if let (Ok(sha), Ok(has_tag), Ok(dirty)) = (
        std::env::var("GIT_SHA"),
        std::env::var("GIT_HAS_TAG"),
        std::env::var("GIT_DIRTY"),
    ) {
        let has_tag: bool = has_tag
            .parse()
            .expect("GIT_HAS_TAG must be 'true' or 'false'");
        let dirty: bool = dirty.parse().expect("GIT_DIRTY must be 'true' or 'false'");
        let sha = sha.trim();
        if !has_tag && sha.is_empty() {
            panic!("GIT_SHA must be non-empty when GIT_HAS_TAG is false");
        }
        println!("cargo:rustc-env=GIT_SHA={sha}");
        println!("cargo:rustc-env=GIT_HAS_TAG={has_tag}");
        println!("cargo:rustc-env=GIT_DIRTY={dirty}");
        return;
    } else if std::env::var("GIT_SHA").is_ok()
        || std::env::var("GIT_HAS_TAG").is_ok()
        || std::env::var("GIT_DIRTY").is_ok()
    {
        panic!(
            "GIT_SHA, GIT_HAS_TAG, and GIT_DIRTY must all be set together; \
             found only a subset — check your Docker build args"
        );
    }

    // In worktrees, HEAD lives in the worktree's own git dir, but refs/tags and
    // packed-refs live in the common (main repo) git dir. Track both.

    // If we're not in a git repo at all, fail the build — we require git metadata.
    let git_dir = git_output(&["rev-parse", "--absolute-git-dir"])
        .expect("not inside a git repository — irys-chain requires git metadata to build");
    // --git-common-dir may return a relative path; canonicalize to ensure
    // rerun-if-changed directives use absolute paths.
    let common_dir = git_output(&["rev-parse", "--git-common-dir"])
        .and_then(|p| std::fs::canonicalize(&p).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(git_dir.clone());

    // Track HEAD so cargo rebuilds when the checked-out commit changes
    let head_path = format!("{git_dir}/HEAD");
    println!("cargo:rerun-if-changed={head_path}");

    // If HEAD is a symbolic ref, also track the branch ref itself
    if let Ok(head_content) = std::fs::read_to_string(&head_path)
        && let Some(ref_path) = head_content.trim().strip_prefix("ref: ")
    {
        // For worktrees, the branch ref lives in the common git dir
        println!("cargo:rerun-if-changed={common_dir}/{ref_path}");
    }

    // Track packed-refs so that adding/removing tags triggers a rebuild.
    // Tag refs often live only in packed-refs rather than as loose files.
    let packed_refs = format!("{common_dir}/packed-refs");
    println!("cargo:rerun-if-changed={packed_refs}");
    // Track individual loose tag files — directory-level rerun-if-changed
    // doesn't reliably detect new files on all filesystems. Enumerate existing
    // entries explicitly; new tags arriving via fetch typically land in packed-refs.
    let tags_dir = format!("{common_dir}/refs/tags");
    println!("cargo:rerun-if-changed={tags_dir}");
    if let Ok(entries) = std::fs::read_dir(&tags_dir) {
        for entry in entries.flatten() {
            println!("cargo:rerun-if-changed={}", entry.path().display());
        }
    }

    // Pin to 7-character short SHA for deterministic output regardless of repo size.
    let sha = git_output(&["rev-parse", "--short=7", "HEAD"])
        .expect("git rev-parse --short=7 HEAD failed — cannot determine commit SHA");
    // TODO: filter to release tag patterns (e.g. `v*`) so CI/local tags don't
    // suppress the SHA. Currently any tag on HEAD triggers the "tagged" path.
    let has_tag = git_output(&["describe", "--exact-match", "--tags", "HEAD"]).is_some();
    // Detect uncommitted changes (staged or unstaged, excluding untracked files).
    // Note: no rerun-if-changed for working-tree state — cargo can't practically
    // track all files. Dirty detection is best-effort; a `cargo clean` or commit
    // change will refresh it.
    let is_dirty = Command::new("git")
        .args(["diff-index", "--quiet", "HEAD", "--"])
        .status()
        .map(|s| !s.success())
        .unwrap_or_else(|e| {
            println!("cargo:warning=git diff-index failed ({e}), assuming dirty");
            true
        });

    println!("cargo:rustc-env=GIT_SHA={sha}");
    println!("cargo:rustc-env=GIT_HAS_TAG={has_tag}");
    println!("cargo:rustc-env=GIT_DIRTY={is_dirty}");
}

fn git_output(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
}
