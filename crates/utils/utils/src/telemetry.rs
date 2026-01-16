//! OpenTelemetry integration for observability stack (Grafana/Tempo/Prometheus/Elasticsearch)

use eyre::Result;
#[cfg(feature = "telemetry")]
use {
    opentelemetry::{trace::TracerProvider as _, KeyValue},
    opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge,
    opentelemetry_otlp::WithExportConfig as _,
    opentelemetry_sdk::{
        logs::SdkLoggerProvider, metrics::SdkMeterProvider, resource::Resource,
        trace::SdkTracerProvider,
    },
    std::sync::OnceLock,
    tracing::level_filters::LevelFilter,
    tracing_error::ErrorLayer,
    tracing_subscriber::{
        layer::SubscriberExt as _, util::SubscriberInitExt as _, EnvFilter, Layer as _, Registry,
    },
};

#[cfg(feature = "telemetry")]
static LOGGER_PROVIDER: OnceLock<SdkLoggerProvider> = OnceLock::new();

#[cfg(feature = "telemetry")]
static TRACER_PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

#[cfg(feature = "telemetry")]
static METER_PROVIDER: OnceLock<SdkMeterProvider> = OnceLock::new();

#[cfg(feature = "telemetry")]
const EXPORTER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(feature = "telemetry")]
const METRICS_EXPORT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(feature = "telemetry")]
const DEFAULT_OTLP_ENDPOINT: &str = "http://localhost:4317";

#[cfg(feature = "telemetry")]
const DEFAULT_LOGS_ENDPOINT: &str = "http://localhost:4318/v1/logs";

#[cfg(feature = "telemetry")]
const DEFAULT_SERVICE_NAME: &str = "irys-node";

#[cfg(feature = "telemetry")]
struct TelemetryConfig {
    service_name: String,
    traces_endpoint: String,
    logs_endpoint: String,
    axiom_logs_endpoint: Option<String>,
    metrics_endpoint: String,
}

#[cfg(feature = "telemetry")]
impl TelemetryConfig {
    fn from_env() -> Self {
        let otlp_endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
            .unwrap_or_else(|_| DEFAULT_OTLP_ENDPOINT.to_string());

        Self {
            service_name: std::env::var("OTEL_SERVICE_NAME")
                .unwrap_or_else(|_| DEFAULT_SERVICE_NAME.to_string()),
            traces_endpoint: std::env::var("OTEL_TRACES_ENDPOINT")
                .unwrap_or_else(|_| otlp_endpoint.clone()),
            logs_endpoint: std::env::var("OTEL_LOGS_ENDPOINT")
                .unwrap_or_else(|_| DEFAULT_LOGS_ENDPOINT.to_string()),
            axiom_logs_endpoint: std::env::var("AXIOM_LOGS_ENDPOINT").ok(),
            metrics_endpoint: std::env::var("OTEL_METRICS_ENDPOINT").unwrap_or(otlp_endpoint),
        }
    }
}

#[cfg(feature = "telemetry")]
fn build_resource(service_name: &str) -> Resource {
    Resource::builder_empty()
        .with_service_name(service_name.to_string())
        .with_attributes([KeyValue::new(
            opentelemetry_semantic_conventions::resource::SERVICE_VERSION,
            env!("CARGO_PKG_VERSION"),
        )])
        .build()
}

#[cfg(feature = "telemetry")]
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

#[cfg(feature = "telemetry")]
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

#[cfg(feature = "telemetry")]
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

#[cfg(feature = "telemetry")]
fn build_tracer_provider(
    trace_exporter: opentelemetry_otlp::SpanExporter,
    resource: Resource,
) -> SdkTracerProvider {
    let span_processor =
        opentelemetry_sdk::trace::BatchSpanProcessor::builder(trace_exporter).build();

    opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_resource(resource)
        .with_span_processor(span_processor)
        .build()
}

#[cfg(feature = "telemetry")]
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

#[cfg(feature = "telemetry")]
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

#[cfg(feature = "telemetry")]
fn setup_tracing_subscriber(
    tracer_provider: &SdkTracerProvider,
    logger_provider: &SdkLoggerProvider,
) {
    let subscriber = Registry::default();
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    let output_layer = tracing_subscriber::fmt::layer()
        .with_line_number(true)
        .with_ansi(true)
        .with_file(true)
        .with_writer(std::io::stdout);

    let subscriber = subscriber
        .with(filter)
        .with(ErrorLayer::default())
        .with(output_layer.boxed());

    let tracer = tracer_provider.tracer(DEFAULT_SERVICE_NAME);
    let otel_trace_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let otel_log_layer = OpenTelemetryTracingBridge::new(logger_provider);

    let subscriber = subscriber.with(otel_trace_layer).with(otel_log_layer);
    subscriber.init();
}

#[cfg(feature = "telemetry")]
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
/// Environment variables:
/// - `OTEL_EXPORTER_OTLP_ENDPOINT`: Base OTLP endpoint (default: `http://localhost:4317`)
/// - `OTEL_SERVICE_NAME`: Service name (default: "irys-node")
/// - `OTEL_TRACES_ENDPOINT`: Override endpoint for traces (optional)
/// - `OTEL_LOGS_ENDPOINT`: Override endpoint for logs (optional)
/// - `OTEL_METRICS_ENDPOINT`: Override endpoint for metrics (optional)
/// - `AXIOM_LOGS_ENDPOINT`: Additional Axiom OTLP endpoint for logs (optional)
///   When set, logs are sent to BOTH `OTEL_LOGS_ENDPOINT` AND `AXIOM_LOGS_ENDPOINT`
/// - `ENABLE_TELEMETRY`: Set to "true" to enable telemetry
///
/// # Errors
///
/// Returns an error if any OTLP exporter fails to build.
#[must_use = "telemetry initialization result should be checked"]
#[cfg(feature = "telemetry")]
pub fn init_telemetry() -> Result<()> {
    let config = TelemetryConfig::from_env();
    let resource = build_resource(&config.service_name);

    let trace_exporter = build_trace_exporter(&config.traces_endpoint)?;
    let metrics_exporter = build_metrics_exporter(&config.metrics_endpoint)?;

    // Build log exporters - primary OTLP endpoint plus optional Axiom
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

    opentelemetry::global::set_meter_provider(meter_provider.clone());

    if LOGGER_PROVIDER.set(logger_provider.clone()).is_err() {
        eprintln!(
            "Warning: Logger provider already initialized, skipping duplicate initialization"
        );
    }
    if TRACER_PROVIDER.set(tracer_provider.clone()).is_err() {
        eprintln!(
            "Warning: Tracer provider already initialized, skipping duplicate initialization"
        );
    }
    if METER_PROVIDER.set(meter_provider).is_err() {
        eprintln!("Warning: Meter provider already initialized, skipping duplicate initialization");
    }

    setup_tracing_subscriber(&tracer_provider, &logger_provider);
    install_panic_hook();

    let endpoints_str = log_endpoints.join(", ");
    tracing::info!(
        telemetry.service_name = %config.service_name,
        telemetry.traces_endpoint = %config.traces_endpoint,
        telemetry.logs_endpoints = %endpoints_str,
        telemetry.metrics_endpoint = %config.metrics_endpoint,
        "OpenTelemetry telemetry initialized - logs, traces, and metrics will be exported"
    );

    Ok(())
}

/// Flush all pending telemetry (logs, traces/spans, metrics) before process termination.
///
/// This properly drains all pending log/trace/metric batches and waits for exports
/// to complete.
///
/// # Important
///
/// This is a blocking call. When using tokio, call from `spawn_blocking`.
///
/// # Errors
///
/// Returns `Ok(true)` if any provider was flushed, `Ok(false)` if none were initialized.
#[must_use = "flush result indicates whether telemetry was exported"]
#[cfg(feature = "telemetry")]
pub fn flush_telemetry() -> Result<bool> {
    let mut flushed = false;

    if let Some(logger) = LOGGER_PROVIDER.get() {
        if let Err(e) = logger.force_flush() {
            eprintln!("Logger provider force flush error: {e:?}");
        } else {
            flushed = true;
        }
    }

    if let Some(tracer) = TRACER_PROVIDER.get() {
        if let Err(e) = tracer.force_flush() {
            eprintln!("Tracer provider force flush error: {e:?}");
        } else {
            flushed = true;
        }
    }

    if let Some(meter) = METER_PROVIDER.get() {
        if let Err(e) = meter.force_flush() {
            eprintln!("Meter provider force flush error: {e:?}");
        } else {
            flushed = true;
        }
    }

    Ok(flushed)
}

#[must_use = "telemetry initialization result should be checked"]
#[cfg(not(feature = "telemetry"))]
pub fn init_telemetry() -> Result<()> {
    tracing::warn!("Telemetry feature is not enabled, skipping OpenTelemetry initialization");
    Ok(())
}

#[must_use = "flush result indicates whether telemetry was exported"]
#[cfg(not(feature = "telemetry"))]
pub fn flush_telemetry() -> Result<bool> {
    Ok(false)
}
