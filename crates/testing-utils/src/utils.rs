use chrono::{SecondsFormat, Utc};
use color_eyre::eyre;
use std::panic;
use std::{fs::create_dir_all, path::PathBuf, str::FromStr as _};
pub use tempfile;
use tempfile::TempDir;
use tracing::debug;
use tracing_error::ErrorLayer;
use tracing_subscriber::{
    fmt::{self, SubscriberBuilder},
    layer::SubscriberExt as _,
    util::SubscriberInitExt as _,
    EnvFilter,
};

pub fn initialize_tracing() {
    let _ = SubscriberBuilder::default()
        .with_env_filter(EnvFilter::from_default_env())
        .with_span_events(fmt::format::FmtSpan::NONE)
        .finish()
        .try_init();
}

pub fn initialize_tracing_with_backtrace() {
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
        .disable_cleanup(keep)
        .tempdir_in(tmp_path);

    let temp_dir = builder.expect("Not able to create a temporary directory.");

    debug!("using random path: {:?} ", &temp_dir);
    temp_dir
}

pub fn setup_panic_hook() -> eyre::Result<()> {
    color_eyre::install()?;

    // wrap the color_eyre panic hook & log the timestamp before it runs
    // not perfect, but probably good enough
    // easier than hook_builder.panic_message
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        // get current timestamp in RFC3339 format with microseconds and Z suffix to match `tracing`
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);

        // print timestamp before the panic message
        eprintln!("\x1b[1;31m[{}] Panic occurred:\x1b[0m", timestamp);

        // call the original panic hook
        original_hook(panic_info);

        // abort the process
        eprintln!("\x1b[1;31mPanic occurred, Aborting process\x1b[0m");
        // TODO: maybe change this so that the panic hook can trigger an orderly shutdown
        std::process::abort()
    }));

    Ok(())
}
