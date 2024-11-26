use std::{
    fs::create_dir_all,
    path::{absolute, PathBuf},
    str::FromStr as _,
};
use tempfile::TempDir;
use tracing::debug;
use tracing_subscriber::{util::SubscriberInitExt, FmtSubscriber};

/// Configures support for logging `Tracing` macros to console, and creates a temporary directory in ./<project_dir>/.tmp.  
/// The temp directory is prefixed by <name> (default: "irys-test-"), and automatically deletes itself on test completion -
/// unless the `keep` flag is set to `true` - in which case the folder persists indefinitely.
pub fn setup_tracing_and_temp_dir(name: Option<&str>, keep: bool) -> TempDir {
    // tracing-subscriber is so the tracing log macros (i.e info!) work
    FmtSubscriber::new().init();

    temporary_directory(name, keep)
}
/// Creates a temporary directory
pub fn temporary_directory(name: Option<&str>, keep: bool) -> TempDir {
    let abs_tmp_path = absolute(PathBuf::from_str("../../.tmp").unwrap()).unwrap();

    create_dir_all(&abs_tmp_path).unwrap();

    let builder = tempfile::Builder::new()
        .prefix(name.unwrap_or("irys-test-"))
        .rand_bytes(8)
        .keep(keep)
        .tempdir_in(abs_tmp_path);

    let temp_dir = builder.expect("Not able to create a temporary directory.");

    debug!("using random path: {:?} ", &temp_dir);
    temp_dir
}
