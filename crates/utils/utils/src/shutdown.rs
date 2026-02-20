use irys_types::ShutdownReason;
use std::time::Duration;
use tracing::warn;

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
