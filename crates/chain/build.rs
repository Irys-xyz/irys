use std::process::Command;

fn main() {
    // If we're not in a git repo at all, fail the build — we require git metadata.
    let git_dir = git_output(&["rev-parse", "--git-dir"])
        .expect("not inside a git repository — irys-chain requires git metadata to build");

    // Track HEAD so cargo rebuilds when the checked-out commit changes
    let head_path = format!("{git_dir}/HEAD");
    println!("cargo:rerun-if-changed={head_path}");

    // If HEAD is a symbolic ref, also track the branch ref itself
    if let Ok(head_content) = std::fs::read_to_string(&head_path)
        && let Some(ref_path) = head_content.trim().strip_prefix("ref: ")
    {
        // For worktrees, the branch ref lives in the common git dir
        let base = git_output(&["rev-parse", "--git-common-dir"]).unwrap_or(git_dir);
        println!("cargo:rerun-if-changed={base}/{ref_path}");
    }

    // Track packed-refs so that adding/removing tags triggers a rebuild.
    // Tag refs often live only in packed-refs rather than as loose files.
    if let Some(base) = git_output(&["rev-parse", "--git-common-dir"]) {
        let packed_refs = format!("{base}/packed-refs");
        println!("cargo:rerun-if-changed={packed_refs}");
        // Track individual loose tag files — directory-level rerun-if-changed
        // doesn't reliably detect new files on all filesystems.
        let tags_dir = format!("{base}/refs/tags");
        // Track the directory itself so new/removed loose tags trigger a rebuild.
        println!("cargo:rerun-if-changed={tags_dir}");
        if let Ok(entries) = std::fs::read_dir(&tags_dir) {
            for entry in entries.flatten() {
                println!("cargo:rerun-if-changed={}", entry.path().display());
            }
        }
    }

    // Pin to 7-character short SHA for deterministic output regardless of repo size.
    let sha = git_output(&["rev-parse", "--short=7", "HEAD"])
        .expect("git rev-parse --short=7 HEAD failed — cannot determine commit SHA");
    // describe --exact-match exits non-zero when HEAD has no tag, which is normal
    let has_tag = git_output(&["describe", "--exact-match", "--tags", "HEAD"]).is_some();
    // Detect uncommitted changes (staged or unstaged, excluding untracked files).
    let is_dirty = Command::new("git")
        .args(["diff-index", "--quiet", "HEAD", "--"])
        .status()
        .map(|s| !s.success())
        .unwrap_or(false);

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
