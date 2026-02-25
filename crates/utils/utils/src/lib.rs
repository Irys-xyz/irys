mod metric_macros;

pub mod circuit_breaker;
pub mod listener;
pub mod shutdown;
pub mod signal;

/// Extension trait for converting [`std::time::Instant`] elapsed time to milliseconds.
pub trait ElapsedMs {
    fn elapsed_ms(&self) -> f64;
}

impl ElapsedMs for std::time::Instant {
    #[inline]
    fn elapsed_ms(&self) -> f64 {
        self.elapsed().as_secs_f64() * 1000.0
    }
}

/// Builds a boxed fmt layer configured via the `IRYS_LOG_FORMAT` env var.
/// When `IRYS_LOG_FORMAT=json`, produces structured JSON output; otherwise plain text.
pub fn make_fmt_layer<S>() -> Box<dyn tracing_subscriber::Layer<S> + Send + Sync>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use tracing_subscriber::{fmt::format::FmtSpan, Layer as _};

    let use_json = std::env::var("IRYS_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    if use_json {
        tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .with_file(true)
            .with_line_number(true)
            .with_writer(std::io::stdout)
            .with_span_events(FmtSpan::NONE)
            .boxed()
    } else {
        tracing_subscriber::fmt::layer()
            .with_line_number(true)
            .with_file(true)
            .with_writer(std::io::stdout)
            .with_span_events(FmtSpan::NONE)
            .boxed()
    }
}

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
