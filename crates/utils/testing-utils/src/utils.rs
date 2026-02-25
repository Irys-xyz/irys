use chrono::{SecondsFormat, Utc};
use color_eyre::eyre;
use irys_utils::shutdown::spawn_shutdown_watchdog;
use rand::{Rng as _, SeedableRng as _};
use std::panic;
use std::{fs::create_dir_all, path::PathBuf, str::FromStr as _};
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

#[cfg(feature = "telemetry")]
use std::backtrace::Backtrace;

pub fn initialize_tracing() {
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
/// The temp directory is prefixed by `<name>` (default: "irys-test-"), and automatically deletes itself on test completion -
/// unless the `keep` flag is set to `true` - in which case the folder persists indefinitely.
pub fn setup_tracing_and_temp_dir(name: Option<&str>, keep: bool) -> TempDir {
    initialize_tracing();
    temporary_directory(name, keep)
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

    // Default fallback
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

    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        // get current timestamp in RFC3339 format with microseconds and Z suffix to match `tracing`
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
        let panic_message = panic_info.to_string();
        let location = panic_info.location();

        // Log panic to OpenTelemetry with structured fields
        let panic_message_clean = panic_message.replace('\n', " | ");

        // Capture backtrace if telemetry is enabled, otherwise empty string
        #[cfg(feature = "telemetry")]
        let backtrace_str = Backtrace::force_capture().to_string().replace('\n', " | ");
        #[cfg(not(feature = "telemetry"))]
        let backtrace_str = String::new();

        if let Some(loc) = location {
            tracing::error!(
                timestamp = %timestamp,
                panic.message = %panic_message_clean,
                panic.file = %loc.file(),
                panic.line = loc.line(),
                panic.column = loc.column(),
                panic.backtrace = %backtrace_str,
                "PANIC OCCURRED - Process will abort"
            );
        } else {
            tracing::error!(
                timestamp = %timestamp,
                panic.message = %panic_message_clean,
                panic.backtrace = %backtrace_str,
                "PANIC OCCURRED (no location) - Process will abort"
            );
        }

        // Flush OpenTelemetry before calling original hook
        #[cfg(feature = "telemetry")]
        {
            if irys_utils::flush_telemetry().is_ok() {
                // Give batch processor time to send data
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        }

        // Print to stderr for immediate console visibility
        eprintln!("\x1b[1;31m[{timestamp}] Panic occurred:\x1b[0m");

        // Call the original panic hook for full backtrace
        original_hook(panic_info);

        eprintln!("\x1b[1;31mPanic occurred, Aborting process\x1b[0m");

        // Trigger SIGINT for orderly shutdown
        let pid = unsafe { libc::getpid() };
        unsafe {
            libc::kill(pid, libc::SIGINT);
        }

        // Spawn watchdog thread to force exit if graceful shutdown hangs
        spawn_shutdown_watchdog(irys_types::ShutdownReason::FatalError("panic".to_string()));
    }));

    Ok(())
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
