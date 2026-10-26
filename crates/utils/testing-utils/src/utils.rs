use color_eyre::eyre;
use irys_utils::shutdown::setup_panic_hook;
use rand::{Rng as _, SeedableRng as _};
use std::{fs::create_dir_all, path::PathBuf, sync::Once};
pub use tempfile;
use tempfile::TempDir;
use tracing::debug;
use tracing_error::ErrorLayer;
use tracing_subscriber::{
    EnvFilter,
    fmt::{self, SubscriberBuilder},
    layer::SubscriberExt as _,
    util::SubscriberInitExt as _,
};

static TRACING_INIT: Once = Once::new();

pub fn initialize_tracing() {
    TRACING_INIT.call_once(|| {
        if std::env::var_os("RUST_BACKTRACE").is_none() {
            unsafe { std::env::set_var("RUST_BACKTRACE", "full") };
        }
        let _ = SubscriberBuilder::default()
            .with_env_filter(EnvFilter::from_default_env())
            .with_span_events(fmt::format::FmtSpan::NONE)
            .finish()
            .with(ErrorLayer::default())
            .try_init();
        let _ = setup_panic_hook();
    });
}

/// Constant used to make sure .tmp shows up in the right place all the time
pub const CARGO_MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

pub fn tmp_base_dir() -> PathBuf {
    if let Ok(custom_tmp) = &std::env::var("IRYS_CUSTOM_TMP_DIR") {
        // try to parse the value as a path
        let path = PathBuf::from(custom_tmp);
        if path.is_absolute()
            || path.exists()
            || custom_tmp.starts_with('/')
            || custom_tmp.starts_with("./")
        {
            // note: we add `.tmp` here as that's the pattern the core pinning logic uses to determine if integration tests are actually tests
            return path.join(".tmp");
        }

        // if it doesn't look like a path, try to use it as an env var key
        if let Ok(env_value) = std::env::var(custom_tmp) {
            return PathBuf::from(env_value).join(".tmp");
        }

        // If both failed, print error and fall back
        eprintln!(
            "Warning: IRYS_CUSTOM_TMP_DIR='{}' is not a valid path and does not reference a valid environment variable. Falling back to default.",
            custom_tmp
        );
    }

    // Default fallback — three levels up from crates/utils/testing-utils/ to workspace root
    PathBuf::from(CARGO_MANIFEST_DIR).join("../../../.tmp")
}

/// Builder for test temporary directories.
///
/// # Examples
/// ```ignore
/// // All defaults (prefix "irys-test-", auto-delete)
/// let dir = TempDirBuilder::new().build();
///
/// // Custom prefix
/// let dir = TempDirBuilder::new().prefix("my-test-").build();
///
/// // Keep on disk for debugging
/// let dir = TempDirBuilder::new().prefix("my-test-").keep().build();
///
/// // Initialize tracing first
/// let dir = TempDirBuilder::new().with_tracing().build();
/// ```
pub struct TempDirBuilder<'a> {
    prefix: &'a str,
    keep: bool,
    tracing: bool,
}

impl<'a> TempDirBuilder<'a> {
    pub fn new() -> Self {
        Self {
            prefix: "irys-test-",
            keep: false,
            tracing: false,
        }
    }

    /// Set the directory name prefix (default: `"irys-test-"`)
    pub fn prefix(mut self, prefix: &'a str) -> Self {
        self.prefix = prefix;
        self
    }

    /// Keep the directory on disk after the test (default: auto-delete)
    pub fn keep(mut self) -> Self {
        self.keep = true;
        self
    }

    /// Initialize tracing before creating the directory
    pub fn with_tracing(mut self) -> Self {
        self.tracing = true;
        self
    }

    /// Build and return the `TempDir`
    pub fn build(self) -> TempDir {
        if self.tracing {
            initialize_tracing();
        }

        let tmp_path = tmp_base_dir();
        create_dir_all(&tmp_path).unwrap();

        let dir = tempfile::Builder::new()
            .prefix(self.prefix)
            .rand_bytes(8)
            .disable_cleanup(self.keep)
            .tempdir_in(tmp_path)
            .expect("Not able to create a temporary directory.");

        debug!("using random path: {:?} ", &dir);
        dir
    }
}

impl Default for TempDirBuilder<'_> {
    fn default() -> Self {
        Self::new()
    }
}

// simple "generator" that produces an iterator of deterministically random chunk bytes
// this is used to create & verify large txs without having to write them to an intermediary
pub fn chunk_bytes_gen(
    count: u64,
    chunk_size: usize,
    seed: u64,
) -> impl Iterator<Item = eyre::Result<Vec<u8> /* ChunkBytes */>> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    (0..count).map(move |i| {
        debug!("generated chunk {}", &i);
        let mut chunk_bytes = vec![0; chunk_size];
        rng.fill(&mut chunk_bytes[..]);
        Ok(chunk_bytes)
    })
}
