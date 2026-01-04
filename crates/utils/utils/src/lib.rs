pub mod circuit_breaker;
pub mod listener;
pub mod shutdown;
pub mod signal;

#[cfg(feature = "telemetry")]
mod telemetry;
#[cfg(feature = "telemetry")]
pub use telemetry::{flush_telemetry, init_telemetry};

#[cfg(not(feature = "telemetry"))]
mod telemetry_stubs {
    use eyre::Result;

    #[must_use = "telemetry initialization result should be checked"]
    pub fn init_telemetry() -> Result<()> {
        eprintln!("Telemetry feature is not enabled, skipping OpenTelemetry initialization");
        Ok(())
    }

    pub fn flush_telemetry() -> Result<()> {
        Ok(())
    }
}
#[cfg(not(feature = "telemetry"))]
pub use telemetry_stubs::{flush_telemetry, init_telemetry};
