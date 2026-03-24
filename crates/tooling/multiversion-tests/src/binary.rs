use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use thiserror::Error;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

#[derive(Debug, Error)]
pub enum BinaryError {
    #[error("git worktree creation failed: {0}")]
    WorktreeCreation(String),
    #[error("cargo build failed for {revision}:\n{stderr}")]
    BuildFailed { revision: String, stderr: String },
    #[error("binary not found at {path}")]
    NotFound { path: PathBuf },
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("git rev-parse failed for ref '{git_ref}': {message}")]
    RevParse { git_ref: String, message: String },
    #[error("command timed out after {duration:?}: {context}")]
    Timeout { duration: Duration, context: String },
}

#[derive(Debug, Clone)]
pub struct ResolvedBinary {
    pub path: PathBuf,
    pub label: String,
    pub git_rev: String,
    /// Path to the git worktree used to build this binary, if one was created.
    /// Cleanup is deferred to [`crate::cluster::Cluster::shutdown`] so the
    /// source tree is preserved on test failure for debugging.
    pub worktree_path: Option<PathBuf>,
}

pub struct BinaryResolver {
    repo_root: PathBuf,
    cache_dir: PathBuf,
    /// Cargo profile to build with. When `None`, tries `debug-release` then
    /// falls back to `release` for compatibility with older revisions.
    profile: Option<String>,
}

fn validate_git_ref(git_ref: &str) -> Result<(), BinaryError> {
    if git_ref.is_empty() {
        return Err(BinaryError::RevParse {
            git_ref: git_ref.to_owned(),
            message: "git ref cannot be empty".to_owned(),
        });
    }
    if git_ref.starts_with('-') {
        return Err(BinaryError::RevParse {
            git_ref: git_ref.to_owned(),
            message: "git ref cannot start with '-'".to_owned(),
        });
    }
    if git_ref.contains("..") || git_ref.contains('\0') {
        return Err(BinaryError::RevParse {
            git_ref: git_ref.to_owned(),
            message: "git ref contains invalid characters".to_owned(),
        });
    }
    Ok(())
}

/// Sentinel ref value that means "build from the current working tree"
/// instead of creating a git worktree. Skips the rev-keyed cache and
/// relies on cargo's own fingerprinting to decide whether to rebuild.
pub const CURRENT_REF: &str = "CURRENT";

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const CARGO_BUILD_TIMEOUT: Duration = Duration::from_secs(1_200);

impl BinaryResolver {
    pub fn new(repo_root: &Path) -> Self {
        let cache_dir = repo_root.join("target/multiversion");
        let profile = std::env::var("IRYS_BUILD_PROFILE").ok();
        Self {
            repo_root: repo_root.to_path_buf(),
            cache_dir,
            profile,
        }
    }

    pub async fn resolve_new(&self) -> Result<ResolvedBinary, BinaryError> {
        let git_ref = std::env::var("IRYS_NEW_REF").unwrap_or_else(|_| CURRENT_REF.to_owned());
        self.resolve_binary("IRYS_BINARY_NEW", "new", &git_ref)
            .await
    }

    pub async fn resolve_old(&self, git_ref: &str) -> Result<ResolvedBinary, BinaryError> {
        self.resolve_binary("IRYS_BINARY_OLD", "old", git_ref).await
    }

    /// Shared resolution logic: check env override, then build from the
    /// working tree (`CURRENT`) or from a git ref via worktree.
    async fn resolve_binary(
        &self,
        env_binary_var: &str,
        label: &str,
        git_ref: &str,
    ) -> Result<ResolvedBinary, BinaryError> {
        if let Some(binary) = self.try_env_override(env_binary_var, label).await? {
            return Ok(binary);
        }
        if git_ref == CURRENT_REF {
            self.resolve_current(label).await
        } else {
            self.resolve_from_ref(git_ref, label).await
        }
    }

    /// Build a binary from the current working tree. Skips the rev-keyed
    /// cache — cargo's own fingerprinting decides whether to rebuild.
    async fn resolve_current(&self, label: &str) -> Result<ResolvedBinary, BinaryError> {
        tracing::info!(label, "resolving binary from current working tree");
        let built = self.cargo_build(&self.repo_root).await?;
        Ok(ResolvedBinary {
            path: built,
            label: label.to_owned(),
            git_rev: "current".to_owned(),
            worktree_path: None,
        })
    }

    /// Build a binary from a specific git ref using a worktree.
    async fn resolve_from_ref(
        &self,
        git_ref: &str,
        label: &str,
    ) -> Result<ResolvedBinary, BinaryError> {
        validate_git_ref(git_ref)?;

        let rev = self.git_rev_at(&self.repo_root, git_ref).await?;
        let cached = self.cached_binary_path(&rev);
        let wt_path = self.cache_dir.join(format!("worktree-{rev}"));

        if cached.exists() {
            let worktree = wt_path.exists().then_some(wt_path);
            return Ok(resolved(label, rev, cached, worktree));
        }

        let _lock = self.acquire_build_lock(&rev).await?;
        if cached.exists() {
            let worktree = wt_path.exists().then_some(wt_path);
            return Ok(resolved(label, rev, cached, worktree));
        }

        self.ensure_worktree(git_ref, &rev, &wt_path).await?;
        self.build_at(&wt_path, &cached).await?;

        tracing::info!(path = %wt_path.display(), "worktree kept; cleanup deferred to test teardown");

        Ok(resolved(label, rev, cached, Some(wt_path)))
    }

    /// Ensure a worktree exists at `wt_path` checked out at the commit
    /// identified by `expected_rev`. Creates, recreates, or reuses the
    /// worktree as needed, verifying the checked-out revision on reuse.
    async fn ensure_worktree(
        &self,
        git_ref: &str,
        expected_rev: &str,
        wt_path: &Path,
    ) -> Result<(), BinaryError> {
        if wt_path.exists() {
            let needs_recreate = match self.git_rev_at(wt_path, "HEAD").await {
                Ok(ref wt_rev) if wt_rev == expected_rev => {
                    tracing::info!(
                        path = %wt_path.display(),
                        rev = %expected_rev,
                        "reusing verified worktree"
                    );
                    false
                }
                Ok(wt_rev) => {
                    tracing::info!(
                        path = %wt_path.display(),
                        expected = %expected_rev,
                        actual = %wt_rev,
                        "worktree at wrong revision, recreating"
                    );
                    true
                }
                Err(e) => {
                    tracing::warn!(
                        path = %wt_path.display(),
                        error = %e,
                        "failed to verify worktree revision, recreating"
                    );
                    true
                }
            };
            if needs_recreate {
                tokio::fs::remove_dir_all(wt_path).await?;
                self.prune_stale_worktrees().await;
                self.create_worktree(git_ref, wt_path).await?;
            }
        } else {
            self.prune_stale_worktrees().await;
            self.create_worktree(git_ref, wt_path).await?;
        }
        Ok(())
    }

    async fn try_env_override(
        &self,
        env_var: &str,
        label: &str,
    ) -> Result<Option<ResolvedBinary>, BinaryError> {
        let Ok(raw) = std::env::var(env_var) else {
            return Ok(None);
        };
        let path = PathBuf::from(raw);
        if !path.is_file() {
            return Err(BinaryError::NotFound { path });
        }
        Ok(Some(resolved(label, "env-override".into(), path, None)))
    }

    fn cached_binary_path(&self, rev: &str) -> PathBuf {
        let name = match &self.profile {
            Some(p) => format!("irys-{rev}-{p}"),
            None => format!("irys-{rev}"),
        };
        self.cache_dir.join(name)
    }

    /// Inter-process exclusive lock for a specific revision build.
    /// Prevents parallel test processes from creating the same worktree simultaneously.
    /// The lock is released when the returned File is dropped.
    async fn acquire_build_lock(&self, rev: &str) -> Result<File, BinaryError> {
        tokio::fs::create_dir_all(&self.cache_dir).await?;
        let lock_path = self.cache_dir.join(format!("build-{rev}.lock"));
        tokio::task::spawn_blocking(move || -> Result<File, std::io::Error> {
            let file = File::create(lock_path)?;
            file.lock()?;
            Ok(file)
        })
        .await
        .map_err(|e| BinaryError::Io(std::io::Error::other(e)))?
        .map_err(BinaryError::Io)
    }

    async fn git_rev_at(&self, dir: &Path, git_ref: &str) -> Result<String, BinaryError> {
        validate_git_ref(git_ref)?;

        let output = timeout(
            GIT_TIMEOUT,
            Command::new("git")
                .args(["rev-parse", "--short=12"])
                .arg(git_ref)
                .current_dir(dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output(),
        )
        .await
        .map_err(|_| BinaryError::Timeout {
            duration: GIT_TIMEOUT,
            context: format!("git rev-parse {git_ref}"),
        })??;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BinaryError::RevParse {
                git_ref: git_ref.to_owned(),
                message: stderr.trim().to_owned(),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    async fn create_worktree(&self, git_ref: &str, path: &Path) -> Result<(), BinaryError> {
        tokio::fs::create_dir_all(&self.cache_dir).await?;

        let output = timeout(
            GIT_TIMEOUT,
            Command::new("git")
                .args(["worktree", "add", "--detach"])
                .arg(path)
                .arg(git_ref)
                .current_dir(&self.repo_root)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output(),
        )
        .await
        .map_err(|_| BinaryError::Timeout {
            duration: GIT_TIMEOUT,
            context: format!("git worktree add {}", path.display()),
        })??;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BinaryError::WorktreeCreation(stderr.trim().to_owned()));
        }

        Ok(())
    }

    /// Run `cargo build` in the given directory and return the path to the
    /// built binary. Does not copy to the rev-keyed cache.
    async fn cargo_build(&self, work_dir: &Path) -> Result<PathBuf, BinaryError> {
        if let Some(ref profile) = self.profile {
            let (args, target_dir): (Vec<&str>, &str) = match profile.as_str() {
                "dev" => (vec![], "debug"),
                "release" => (vec!["--release"], "release"),
                p => (vec!["--profile", p], p),
            };
            self.try_cargo_build(work_dir, &args, target_dir).await
        } else {
            // Default: try debug-release, fall back to release for older revisions
            // that don't define the debug-release profile.
            match self
                .try_cargo_build(work_dir, &["--profile", "debug-release"], "debug-release")
                .await
            {
                Ok(path) => Ok(path),
                Err(BinaryError::BuildFailed { ref stderr, .. })
                    if stderr.contains("is not defined") =>
                {
                    tracing::info!("debug-release profile unavailable, falling back to --release");
                    self.try_cargo_build(work_dir, &["--release"], "release")
                        .await
                }
                Err(e) => Err(e),
            }
        }
    }

    /// Build a binary and copy it to the rev-keyed cache.
    async fn build_at(&self, work_dir: &Path, output_path: &Path) -> Result<(), BinaryError> {
        if let Some(parent) = output_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let built = self.cargo_build(work_dir).await?;
        self.copy_to_cache(&built, output_path).await
    }

    /// Atomically copy a built binary into the cache via tmp + rename.
    async fn copy_to_cache(&self, built: &Path, output_path: &Path) -> Result<(), BinaryError> {
        let tmp_path = output_path.with_extension("tmp");
        match tokio::fs::copy(built, &tmp_path).await {
            Ok(_) => {}
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(e.into());
            }
        }
        if let Err(e) = tokio::fs::rename(&tmp_path, output_path).await {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(e.into());
        }
        Ok(())
    }

    async fn try_cargo_build(
        &self,
        work_dir: &Path,
        profile_args: &[&str],
        target_dir_name: &str,
    ) -> Result<PathBuf, BinaryError> {
        let mut args = vec!["build"];
        args.extend(profile_args);
        args.extend(["--bin", "irys", "-p", "irys-chain"]);

        tracing::info!(
            dir = %work_dir.display(),
            profile = %target_dir_name,
            "starting cargo build"
        );

        let mut child = Command::new("cargo")
            .args(&args)
            .current_dir(work_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        // Stream stderr line-by-line to eprintln so build progress is visible
        // during tests (with --nocapture), while also collecting it for error
        // reporting on failure.
        let stderr_handle = child.stderr.take();
        let stderr_task = tokio::spawn(async move {
            let mut collected = String::new();
            if let Some(stderr) = stderr_handle {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("[cargo] {line}");
                    collected.push_str(&line);
                    collected.push('\n');
                }
            }
            collected
        });

        let status = match timeout(CARGO_BUILD_TIMEOUT, child.wait()).await {
            Ok(result) => result?,
            Err(_) => {
                let _ = child.start_kill();
                // Reap the child process so it doesn't become a zombie.
                let _ = child.wait().await;
                stderr_task.abort();
                return Err(BinaryError::Timeout {
                    duration: CARGO_BUILD_TIMEOUT,
                    context: format!("cargo build in {}", work_dir.display()),
                });
            }
        };

        let stderr = stderr_task.await.unwrap_or_default();

        if !status.success() {
            return Err(BinaryError::BuildFailed {
                revision: work_dir.display().to_string(),
                stderr,
            });
        }

        let built = work_dir.join(format!("target/{target_dir_name}/irys"));
        if !built.exists() {
            return Err(BinaryError::NotFound { path: built });
        }
        Ok(built)
    }

    async fn prune_stale_worktrees(&self) {
        let result = timeout(
            GIT_TIMEOUT,
            Command::new("git")
                .args(["worktree", "prune"])
                .current_dir(&self.repo_root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
        )
        .await;
        match result {
            Err(_) => tracing::warn!("git worktree prune timed out after {GIT_TIMEOUT:?}"),
            Ok(Err(e)) => tracing::warn!(error = %e, "git worktree prune failed"),
            Ok(Ok(_)) => {}
        }
    }
}

fn resolved(
    label: &str,
    git_rev: String,
    path: PathBuf,
    worktree_path: Option<PathBuf>,
) -> ResolvedBinary {
    ResolvedBinary {
        path,
        label: label.to_owned(),
        git_rev,
        worktree_path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn empty_ref_is_rejected() {
        assert!(validate_git_ref("").is_err());
    }

    proptest! {
        #[test]
        fn dash_prefixed_ref_is_rejected(ref s in "-[a-z0-9]{0,20}") {
            prop_assert!(validate_git_ref(s).is_err());
        }

        #[test]
        fn double_dot_ref_is_rejected(ref s in "[a-z]{1,5}\\.\\.[a-z]{1,5}") {
            prop_assert!(validate_git_ref(s).is_err());
        }

        #[test]
        fn null_byte_ref_is_rejected(ref s in "[a-z]{1,5}\x00[a-z]{1,5}") {
            prop_assert!(validate_git_ref(s).is_err());
        }

        #[test]
        fn valid_ref_is_accepted(ref s in "[a-zA-Z][a-zA-Z0-9_/]{0,49}") {
            prop_assert!(validate_git_ref(s).is_ok());
        }
    }
}
