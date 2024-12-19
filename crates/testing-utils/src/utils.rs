use std::{
    fs::create_dir_all,
    path::PathBuf,
    str::FromStr as _,
};
use tempfile::TempDir;
use tracing::{debug, level_filters::LevelFilter};
use tracing_subscriber::{fmt::SubscriberBuilder, util::SubscriberInitExt};

/// Configures support for logging `Tracing` macros to console, and creates a temporary directory in ./<project_dir>/.tmp.  
/// The temp directory is prefixed by <name> (default: "irys-test-"), and automatically deletes itself on test completion -
/// unless the `keep` flag is set to `true` - in which case the folder persists indefinitely.
pub fn setup_tracing_and_temp_dir(name: Option<&str>, keep: bool) -> TempDir {
    // tracing-subscriber is so the tracing log macros (i.e info!) work
    // TODO: expose tracing configuration
    let _ = SubscriberBuilder::default()
        .with_max_level(LevelFilter::DEBUG)
        .finish()
        .try_init();

    temporary_directory(name, keep)
}

/// Constant used to make sure .tmp shows up in the right place all the time
pub const CARGO_MANIFEST_DIR: &'static str = env!("CARGO_MANIFEST_DIR");

/// Creates a temporary directory
pub fn temporary_directory(name: Option<&str>, keep: bool) -> TempDir {
    let tmp_path = PathBuf::from_str(CARGO_MANIFEST_DIR)
        .unwrap()
        .join("../../.tmp");

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
