//! Simple OpenTelemetry integration for sending logs to Axiom
//!
//! This is a minimal proof-of-concept implementation that sends logs to Axiom
//! using OpenTelemetry OTLP protocol.

use eyre::Result;
#[cfg(feature = "telemetry")]
use {
    opentelemetry::{trace::TracerProvider as _, KeyValue},
    opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge,
    opentelemetry_otlp::{Protocol, WithExportConfig as _, WithHttpConfig as _},
    opentelemetry_sdk::{logs::SdkLoggerProvider, resource::Resource, trace::SdkTracerProvider},
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

/// Initialize OpenTelemetry with Axiom backend
///
/// Required environment variables:
/// - `AXIOM_API_TOKEN`: Your Axiom API token (starts with "xaat-")
/// - `AXIOM_DATASET`: The dataset name to send traces to
///
/// Optional environment variables:
/// - `OTEL_EXPORTER_OTLP_ENDPOINT`: OTLP endpoint (default: "https://api.axiom.co")
/// - `OTEL_SERVICE_NAME`: Service name (default: "irys-node")
#[cfg(feature = "telemetry")]
pub fn init_telemetry() -> Result<()> {
    // Get configuration from environment (before any tracing calls)
    let axiom_token = std::env::var("AXIOM_API_TOKEN")
        .map_err(|_| eyre::eyre!("AXIOM_API_TOKEN environment variable not set"))?;
    let axiom_dataset = std::env::var("AXIOM_DATASET")
        .map_err(|_| eyre::eyre!("AXIOM_DATASET environment variable not set"))?;
    let otlp_endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "https://api.axiom.co".to_string());
    let service_name =
        std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "irys-node".to_string());

    // Create a resource with service information
    let resource = Resource::builder_empty()
        .with_service_name(service_name.clone())
        .with_attributes([KeyValue::new(
            opentelemetry_semantic_conventions::resource::SERVICE_VERSION,
            env!("CARGO_PKG_VERSION"),
        )])
        .build();

    // Configure OTLP exporter for traces with Axiom headers
    let trace_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::Grpc)
        .with_endpoint(format!("{}/v1/traces", otlp_endpoint))
        .with_headers(std::collections::HashMap::from([
            (
                "authorization".to_string(),
                format!("Bearer {}", axiom_token),
            ),
            ("x-axiom-dataset".to_string(), axiom_dataset.clone()),
        ]))
        .with_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| {
            eprintln!("Failed to build OTLP trace exporter: {:?}", e);
            e
        })?;

    // Configure OTLP exporter for logs with Axiom headers
    let log_exporter = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_protocol(Protocol::Grpc)
        .with_endpoint(format!("{}/v1/logs", otlp_endpoint))
        .with_headers(std::collections::HashMap::from([
            (
                "authorization".to_string(),
                format!("Bearer {}", axiom_token),
            ),
            ("x-axiom-dataset".to_string(), axiom_dataset.clone()),
        ]))
        .with_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| {
            eprintln!("Failed to build OTLP log exporter: {:?}", e);
            e
        })?;

    // Use BatchSpanProcessor for async, non-blocking span export
    let span_processor =
        opentelemetry_sdk::trace::BatchSpanProcessor::builder(trace_exporter).build();

    let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_span_processor(span_processor)
        .build();

    // Use BatchLogProcessor for async, non-blocking log export
    let log_processor = opentelemetry_sdk::logs::BatchLogProcessor::builder(log_exporter).build();

    let logger_provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_log_processor(log_processor)
        .build();

    // Store the providers for later flushing (e.g., in panic hook)
    if LOGGER_PROVIDER.set(logger_provider.clone()).is_err() {
        tracing::warn!("Logger provider already initialized, skipping duplicate initialization");
    }
    if TRACER_PROVIDER.set(tracer_provider.clone()).is_err() {
        tracing::warn!("Tracer provider already initialized, skipping duplicate initialization");
    }

    // Set up tracing subscriber FIRST - exactly like init_tracing() does
    let subscriber = Registry::default();
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    let output_layer = tracing_subscriber::fmt::layer()
        .with_line_number(true)
        .with_ansi(true)
        .with_file(true)
        .with_writer(std::io::stdout);

    // Build subscriber exactly like init_tracing()
    let subscriber = subscriber
        .with(filter)
        .with(ErrorLayer::default())
        .with(output_layer.boxed());

    let tracer = tracer_provider.tracer("irys-node");
    let otel_trace_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    let otel_log_layer = OpenTelemetryTracingBridge::new(&logger_provider);

    // Add both OTel layers - one for spans, one for log events
    let subscriber = subscriber.with(otel_trace_layer).with(otel_log_layer);

    subscriber.init();

    // Take any other pre-existig panic hook to chain after flushing
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        // Log panic information with current span context
        tracing::error!(
            panic.location = %panic_info.location().unwrap_or_else(|| std::panic::Location::caller()),
            panic.message = %panic_info.payload().downcast_ref::<&str>()
                .or_else(|| panic_info.payload().downcast_ref::<String>().map(|s| s.as_str()))
                .unwrap_or("<non-string panic>"),
            "Process panicked - flushing telemetry and closing spans before exit"
        );

        if let Err(e) = flush_telemetry() {
            eprintln!("Failed to flush telemetry on panic: {}", e);
        }

        original_hook(panic_info);
    }));

    // NOW we can use tracing! All messages from here forward will go to both terminal and Axiom
    tracing::info!(
        telemetry.service_name = %service_name,
        telemetry.dataset = %axiom_dataset,
        telemetry.endpoint = %otlp_endpoint,
        "OpenTelemetry telemetry initialized with Axiom backend - logs and spans will be exported in batches"
    );

    Ok(())
}

/// Flush pending telemetry before process termination (e.g., panic hooks)
#[cfg(feature = "telemetry")]
pub fn flush_telemetry() -> Result<bool> {
    let mut flushed = false;

    // Force flush the logger provider (logs)
    if let Some(logger_provider) = LOGGER_PROVIDER.get() {
        logger_provider
            .force_flush()
            .map_err(|e| eyre::eyre!("Failed to flush log provider: {:?}", e))?;
        flushed = true;
    }

    if let Some(tracer_provider) = TRACER_PROVIDER.get() {
        tracer_provider
            .force_flush()
            .map_err(|e| eyre::eyre!("Failed to flush tracer provider: {:?}", e))?;
        flushed = true;
    }

    if flushed {
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }

    Ok(flushed)
}

// No-op implementations when the telemetry feature is disabled
#[cfg(not(feature = "telemetry"))]
pub fn init_telemetry() -> Result<()> {
    tracing::warn!("Telemetry feature is not enabled, skipping OpenTelemetry initialization");
    Ok(())
}

#[cfg(not(feature = "telemetry"))]
pub fn flush_telemetry() -> Result<bool> {
    Ok(false)
}
