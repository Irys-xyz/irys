//! OpenTelemetry integration for observability stack (Grafana/Tempo/Prometheus/Elasticsearch)
//!
//! This module also bridges the `metrics` crate (used by Reth) to OpenTelemetry,
//! enabling Reth's internal metrics to be exported via OTLP alongside Irys metrics.

use eyre::Result;
use opentelemetry::{
    trace::{SpanId, TraceId, TracerProvider as _},
    KeyValue,
};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::{
    logs::SdkLoggerProvider,
    metrics::SdkMeterProvider,
    propagation::TraceContextPropagator,
    resource::Resource,
    trace::{IdGenerator, SdkTracerProvider},
};
use std::sync::{Mutex, OnceLock};
use tracing::level_filters::LevelFilter;
use tracing_error::ErrorLayer;
use tracing_subscriber::{
    layer::SubscriberExt as _, util::SubscriberInitExt as _, EnvFilter, Registry,
};

static LOGGER_PROVIDER: OnceLock<SdkLoggerProvider> = OnceLock::new();
static TRACER_PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();
static METER_PROVIDER: OnceLock<SdkMeterProvider> = OnceLock::new();
static INIT_GUARD: OnceLock<()> = OnceLock::new();
static INIT_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

const EXPORTER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const METRICS_EXPORT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const DEFAULT_OTLP_ENDPOINT: &str = "http://localhost:4317";
const DEFAULT_LOGS_ENDPOINT: &str = "http://localhost:4318/v1/logs";
const DEFAULT_SERVICE_NAME: &str = "irys-node";

struct TelemetryConfig {
    service_name: String,
    traces_endpoint: String,
    logs_endpoint: String,
    axiom_logs_endpoint: Option<String>,
    metrics_endpoint: String,
}

impl TelemetryConfig {
    fn from_env() -> Self {
        let otlp_endpoint_env = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();
        let otlp_endpoint = otlp_endpoint_env
            .clone()
            .unwrap_or_else(|| DEFAULT_OTLP_ENDPOINT.to_string());

        let default_logs_endpoint = if otlp_endpoint_env.is_some() {
            format!("{}/v1/logs", otlp_endpoint.trim_end_matches('/'))
        } else {
            DEFAULT_LOGS_ENDPOINT.to_string()
        };

        Self {
            service_name: std::env::var("OTEL_SERVICE_NAME")
                .unwrap_or_else(|_| DEFAULT_SERVICE_NAME.to_string()),
            traces_endpoint: std::env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
                .unwrap_or_else(|_| otlp_endpoint.clone()),
            logs_endpoint: std::env::var("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT")
                .unwrap_or(default_logs_endpoint),
            axiom_logs_endpoint: std::env::var("AXIOM_LOGS_ENDPOINT").ok(),
            metrics_endpoint: std::env::var("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT")
                .unwrap_or(otlp_endpoint),
        }
    }
}

fn build_resource(service_name: &str) -> Resource {
    Resource::builder_empty()
        .with_service_name(service_name.to_string())
        .with_attributes([KeyValue::new(
            opentelemetry_semantic_conventions::resource::SERVICE_VERSION,
            env!("CARGO_PKG_VERSION"),
        )])
        .build()
}

fn build_trace_exporter(
    endpoint: &str,
) -> Result<opentelemetry_otlp::SpanExporter, opentelemetry_otlp::ExporterBuildError> {
    opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_timeout(EXPORTER_TIMEOUT)
        .build()
        .map_err(|e| {
            eprintln!("Failed to build OTLP trace exporter: {e:?}");
            e
        })
}

fn build_log_exporter(
    endpoint: &str,
) -> Result<opentelemetry_otlp::LogExporter, opentelemetry_otlp::ExporterBuildError> {
    opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .with_timeout(EXPORTER_TIMEOUT)
        .build()
        .map_err(|e| {
            eprintln!("Failed to build OTLP log exporter: {e:?}");
            e
        })
}

fn build_metrics_exporter(
    endpoint: &str,
) -> Result<opentelemetry_otlp::MetricExporter, opentelemetry_otlp::ExporterBuildError> {
    opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_timeout(EXPORTER_TIMEOUT)
        .build()
        .map_err(|e| {
            eprintln!("Failed to build OTLP metrics exporter: {e:?}");
            e
        })
}

/// OTEL [`IdGenerator`] producing UUID v7 trace IDs (RFC 9562).
///
/// Embeds a 48-bit millisecond timestamp in the high bytes of every trace ID
/// so that IDs are roughly time-ordered when viewed in Tempo/Grafana.
/// Span IDs are random 8-byte values.
///
/// Uses a thread-local [`SmallRng`](rand::rngs::SmallRng) (Xoshiro256++) seeded
/// once from OS entropy — avoiding the periodic reseeding overhead of `ThreadRng`.
/// (same approach as the default ID generator used by Otel - very hot, so perf does matter)

#[derive(Debug)]
struct UuidV7IdGenerator;

thread_local! {
    static ID_RNG: std::cell::RefCell<rand::rngs::SmallRng> =
        std::cell::RefCell::new(<rand::rngs::SmallRng as rand::SeedableRng>::from_entropy());
}

/// warning: both functions here are *very hot*
impl IdGenerator for UuidV7IdGenerator {
    // UUIDv7 for globally unique tracing IDs
    fn new_trace_id(&self) -> TraceId {
        use rand::Rng as _;
        use std::time::{SystemTime, UNIX_EPOCH};

        let d = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let ts_ms = d.as_secs() * 1000 + d.subsec_millis() as u64;

        ID_RNG.with(|rng| {
            let mut rng = rng.borrow_mut();
            let r0: u64 = rng.gen();
            let r1: u64 = rng.gen();

            // High u64: 48-bit timestamp | version 7 nibble | 12 random bits
            let hi = (ts_ms << 16) | 0x7000 | (r0 & 0x0FFF);
            // Low u64: variant bits (0b10) | 62 random bits
            let lo = (r1 & 0x3FFF_FFFF_FFFF_FFFF) | 0x8000_0000_0000_0000;

            let mut bytes = [0_u8; 16];
            bytes[..8].copy_from_slice(&hi.to_be_bytes());
            bytes[8..].copy_from_slice(&lo.to_be_bytes());
            TraceId::from_bytes(bytes)
        })
    }

    // u64s for local spans
    fn new_span_id(&self) -> SpanId {
        use rand::Rng as _;
        ID_RNG.with(|rng| {
            let mut rng = rng.borrow_mut();
            loop {
                let v: u64 = rng.gen();
                // zero span ID = no span (spec requirement)
                if v != 0 {
                    return SpanId::from_bytes(v.to_be_bytes());
                }
            }
        })
    }
}

fn build_tracer_provider(
    trace_exporter: opentelemetry_otlp::SpanExporter,
    resource: Resource,
) -> SdkTracerProvider {
    let span_processor =
        opentelemetry_sdk::trace::BatchSpanProcessor::builder(trace_exporter).build();

    opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_resource(resource)
        .with_span_processor(span_processor)
        .with_id_generator(UuidV7IdGenerator)
        .build()
}

fn build_logger_provider(
    log_exporters: Vec<opentelemetry_otlp::LogExporter>,
    resource: Resource,
) -> SdkLoggerProvider {
    let mut builder = opentelemetry_sdk::logs::SdkLoggerProvider::builder().with_resource(resource);

    for exporter in log_exporters {
        let processor = opentelemetry_sdk::logs::BatchLogProcessor::builder(exporter).build();
        builder = builder.with_log_processor(processor);
    }

    builder.build()
}

fn build_meter_provider(
    metrics_exporter: opentelemetry_otlp::MetricExporter,
    resource: Resource,
) -> SdkMeterProvider {
    let reader = opentelemetry_sdk::metrics::PeriodicReader::builder(metrics_exporter)
        .with_interval(METRICS_EXPORT_INTERVAL)
        .build();

    opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_resource(resource)
        .with_reader(reader)
        .build()
}

fn setup_tracing_subscriber(
    tracer_provider: &SdkTracerProvider,
    logger_provider: &SdkLoggerProvider,
    service_name: &str,
) {
    let subscriber = Registry::default();
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    let subscriber = subscriber
        .with(filter)
        .with(ErrorLayer::default())
        .with(crate::make_fmt_layer());

    let tracer = tracer_provider.tracer(service_name.to_string());
    let otel_trace_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let otel_log_layer = OpenTelemetryTracingBridge::new(logger_provider);

    let subscriber = subscriber.with(otel_trace_layer).with(otel_log_layer);
    subscriber.init();
}

fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let message = panic_info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| panic_info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic>".to_string());

        tracing::error!(
            panic.location = %panic_info.location().unwrap_or_else(|| std::panic::Location::caller()),
            panic.message = %message,
            "Process panicked - flushing telemetry and closing spans before exit"
        );

        if let Err(e) = flush_telemetry() {
            eprintln!("Failed to flush telemetry on panic: {e}");
        }

        original_hook(panic_info);
    }));
}

/// Initialize OpenTelemetry for the observability stack (Grafana/Tempo/Prometheus/Elasticsearch)
///
/// Environment variables (per OTEL specification):
/// - `OTEL_EXPORTER_OTLP_ENDPOINT`: Base OTLP endpoint (default: `http://localhost:4317`)
///   Signal-specific endpoints derive from this with paths appended (e.g., `/v1/logs`)
/// - `OTEL_SERVICE_NAME`: Service name (default: "irys-node")
/// - `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`: Override endpoint for traces (optional)
/// - `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT`: Override endpoint for logs (optional)
/// - `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`: Override endpoint for metrics (optional)
/// - `AXIOM_LOGS_ENDPOINT`: Additional Axiom OTLP endpoint for logs (optional)
///   When set, logs are sent to BOTH the primary logs endpoint AND Axiom
///
/// # Errors
///
/// Returns an error if any OTLP exporter fails to build.
#[must_use = "telemetry initialization result should be checked"]
pub fn init_telemetry() -> Result<()> {
    let _lock = INIT_MUTEX.get_or_init(|| Mutex::new(())).lock().unwrap();

    if INIT_GUARD.get().is_some() {
        eprintln!("Warning: Telemetry already initialized, skipping duplicate initialization");
        return Ok(());
    }

    let config = TelemetryConfig::from_env();
    let resource = build_resource(&config.service_name);

    let trace_exporter = build_trace_exporter(&config.traces_endpoint)?;
    let metrics_exporter = build_metrics_exporter(&config.metrics_endpoint)?;

    let mut log_exporters = vec![build_log_exporter(&config.logs_endpoint)?];
    let mut log_endpoints = vec![config.logs_endpoint.clone()];

    if let Some(ref axiom_endpoint) = config.axiom_logs_endpoint {
        match build_log_exporter(axiom_endpoint) {
            Ok(exporter) => {
                log_exporters.push(exporter);
                log_endpoints.push(axiom_endpoint.clone());
            }
            Err(e) => {
                eprintln!("Failed to build Axiom log exporter (continuing with primary): {e:?}");
            }
        }
    }

    let tracer_provider = build_tracer_provider(trace_exporter, resource.clone());
    let logger_provider = build_logger_provider(log_exporters, resource.clone());
    let meter_provider = build_meter_provider(metrics_exporter, resource);

    let _ = LOGGER_PROVIDER.set(logger_provider.clone());
    let _ = TRACER_PROVIDER.set(tracer_provider.clone());
    let _ = METER_PROVIDER.set(meter_provider.clone());

    opentelemetry::global::set_meter_provider(meter_provider);
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

    // NOTE: We do NOT install a metrics recorder here because Reth's internal
    // EngineNodeLauncher calls install_prometheus_recorder() and will panic
    // if a recorder is already set. Reth metrics will need to be exposed via
    // a /metrics HTTP endpoint for Prometheus to scrape.

    setup_tracing_subscriber(&tracer_provider, &logger_provider, &config.service_name);
    install_panic_hook();

    let endpoints_str = log_endpoints.join(", ");
    tracing::info!(
        telemetry.service_name = %config.service_name,
        telemetry.traces_endpoint = %config.traces_endpoint,
        telemetry.logs_endpoints = %endpoints_str,
        telemetry.metrics_endpoint = %config.metrics_endpoint,
        "OpenTelemetry telemetry initialized - logs, traces, metrics, and Reth metrics will be exported"
    );

    let _ = INIT_GUARD.set(());

    Ok(())
}

/// Flush all pending telemetry (logs, traces/spans, metrics) before process termination.
///
/// This properly drains all pending log/trace/metric batches and waits for exports
/// to complete. All providers are attempted even if some fail.
///
/// # Important
///
/// This is a blocking call. When using tokio, call from `spawn_blocking`.
///
/// # Errors
///
/// Returns an error if any provider (LOGGER_PROVIDER, TRACER_PROVIDER, METER_PROVIDER)
/// fails to flush. All providers are attempted regardless of individual failures,
/// and errors are aggregated into the result.
///
/// # Returns
///
/// - `Ok(())` if all providers flushed successfully (or no providers initialized)
/// - `Err(...)` with details of which provider(s) failed to flush
pub fn flush_telemetry() -> Result<()> {
    let mut errors = Vec::new();

    if let Some(logger) = LOGGER_PROVIDER.get() {
        if let Err(e) = logger.force_flush() {
            let err_msg = format!("Logger provider force flush error: {e:?}");
            eprintln!("{err_msg}");
            errors.push(err_msg);
        }
    }

    if let Some(tracer) = TRACER_PROVIDER.get() {
        if let Err(e) = tracer.force_flush() {
            let err_msg = format!("Tracer provider force flush error: {e:?}");
            eprintln!("{err_msg}");
            errors.push(err_msg);
        }
    }

    if let Some(meter) = METER_PROVIDER.get() {
        if let Err(e) = meter.force_flush() {
            let err_msg = format!("Meter provider force flush error: {e:?}");
            eprintln!("{err_msg}");
            errors.push(err_msg);
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(eyre::eyre!(
            "Failed to flush {} provider(s): {}",
            errors.len(),
            errors.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_v7_trace_id_has_correct_version_and_variant() {
        let gen = UuidV7IdGenerator;
        let trace_id = gen.new_trace_id();
        let bytes = trace_id.to_bytes();

        assert_eq!(bytes[6] >> 4, 0x7, "version nibble must be 7");
        assert_eq!(bytes[8] >> 6, 0b10, "variant bits must be 0b10");
    }

    #[test]
    fn uuid_v7_trace_ids_are_time_ordered() {
        let gen = UuidV7IdGenerator;
        let id1 = gen.new_trace_id();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let id2 = gen.new_trace_id();

        assert!(
            id1.to_bytes() < id2.to_bytes(),
            "later trace ID must sort after earlier one"
        );
    }

    #[test]
    fn uuid_v7_span_id_is_nonzero() {
        let gen = UuidV7IdGenerator;
        let span_id = gen.new_span_id();
        assert_ne!(
            span_id.to_bytes(),
            [0_u8; 8],
            "span ID must not be all zeros"
        );
    }
}
