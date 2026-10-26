use chrono::{SecondsFormat, Utc};
use irys_types::ShutdownReason;
use std::panic;
use std::time::Duration;
use tracing::warn;

#[cfg(feature = "telemetry")]
use std::backtrace::Backtrace;

/// Maximum time allowed for graceful shutdown before forcing process abort.
pub const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(45);

/// Spawns a watchdog thread that aborts the process after `GRACEFUL_SHUTDOWN_TIMEOUT`.
pub fn spawn_shutdown_watchdog(reason: ShutdownReason) -> std::thread::JoinHandle<()> {
    warn!(
        timeout_secs = GRACEFUL_SHUTDOWN_TIMEOUT.as_secs(),
        %reason,
        "Shutdown initiated, watchdog will force exit if not complete in time"
    );
    std::thread::spawn(move || {
        std::thread::sleep(GRACEFUL_SHUTDOWN_TIMEOUT);
        eprintln!(
            "\x1b[1;31mGraceful shutdown timeout after {:?} (reason: {}), forcing exit\x1b[0m",
            GRACEFUL_SHUTDOWN_TIMEOUT, reason
        );
        std::process::abort();
    })
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
            if crate::flush_telemetry().is_ok() {
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
        let _ = nix::sys::signal::kill(nix::unistd::Pid::this(), nix::sys::signal::Signal::SIGINT);

        // Spawn watchdog thread to force exit if graceful shutdown hangs
        spawn_shutdown_watchdog(ShutdownReason::FatalError("panic".to_string()));
    }));

    Ok(())
}
