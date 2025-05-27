use std::{fs::create_dir_all, path::PathBuf, str::FromStr as _, time::Duration};
pub use tempfile;
use tempfile::TempDir;
use tokio::time::Sleep;
use tracing::debug;
use tracing_subscriber::{
    fmt::{self, SubscriberBuilder},
    util::SubscriberInitExt,
    EnvFilter,
};

pub fn initialize_tracing() {
    let _ = SubscriberBuilder::default()
        .with_env_filter(EnvFilter::from_default_env())
        .with_span_events(fmt::format::FmtSpan::NONE)
        .finish()
        .try_init();
}

pub async fn tokio_sleep(seconds: u64) -> Sleep {
    tokio::time::sleep(Duration::from_secs(seconds))
}

/// Configures support for logging `Tracing` macros to console, and creates a temporary directory in ./<`project_dir>/.tmp`.  
/// The temp directory is prefixed by <name> (default: "irys-test-"), and automatically deletes itself on test completion -
/// unless the `keep` flag is set to `true` - in which case the folder persists indefinitely.
pub fn setup_tracing_and_temp_dir(name: Option<&str>, keep: bool) -> TempDir {
    // tracing-subscriber is so the tracing log macros (i.e info!) work
    // TODO: expose tracing configuration
    let _ = SubscriberBuilder::default()
        .with_env_filter(EnvFilter::from_default_env())
        .with_span_events(fmt::format::FmtSpan::NONE)
        .finish()
        .try_init();

    temporary_directory(name, keep)
}

/// Constant used to make sure .tmp shows up in the right place all the time
pub const CARGO_MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

pub fn tmp_base_dir() -> PathBuf {
    PathBuf::from_str(CARGO_MANIFEST_DIR)
        .unwrap()
        .join("../../.tmp")
}

/// Creates a temporary directory
pub fn temporary_directory(name: Option<&str>, keep: bool) -> TempDir {
    let tmp_path = tmp_base_dir();

    create_dir_all(&tmp_path).unwrap();

    let builder = tempfile::Builder::new()
        .prefix(name.unwrap_or("irys-test-"))
        .rand_bytes(8)
        .keep(keep)
        .tempdir_in(tmp_path);

    let temp_dir = builder.expect("Not able to create a temporary directory.");

    debug!("using random path: {:?} ", &temp_dir);
    temp_dir
}
