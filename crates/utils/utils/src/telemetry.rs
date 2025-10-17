//! Simple OpenTelemetry integration for sending logs to Axiom
//!
//! This is a minimal proof-of-concept implementation that sends logs to Axiom
//! using OpenTelemetry OTLP protocol.

use eyre::Result;

#[cfg(feature = "telemetry")]
use opentelemetry::{KeyValue, trace::Span as _};
#[cfg(feature = "telemetry")]
use opentelemetry_otlp::WithExportConfig;
#[cfg(feature = "telemetry")]
use opentelemetry_sdk::{
    resource::{EnvResourceDetector, SdkProvidedResourceDetector},
    runtime, Resource,
};
#[cfg(feature = "telemetry")]
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Registry};

// Store the tracer provider globally so it doesn't get dropped
#[cfg(feature = "telemetry")]
static TRACER_PROVIDER: std::sync::OnceLock<opentelemetry_sdk::trace::TracerProvider> =
    std::sync::OnceLock::new();

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
    // Get configuration from environment
    let axiom_token = std::env::var("AXIOM_API_TOKEN")
        .map_err(|_| eyre::eyre!("AXIOM_API_TOKEN environment variable not set"))?;
    let axiom_dataset = std::env::var("AXIOM_DATASET")
        .map_err(|_| eyre::eyre!("AXIOM_DATASET environment variable not set"))?;
    let otlp_endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "https://api.axiom.co".to_string());
    let service_name = std::env::var("OTEL_SERVICE_NAME")
        .unwrap_or_else(|_| "irys-node".to_string());

    tracing::info!(
        service_name = %service_name,
        dataset = %axiom_dataset,
        endpoint = %otlp_endpoint,
        "Initializing OpenTelemetry with Axiom backend"
    );

    // Create resource with service information
    let resource = Resource::from_detectors(
        std::time::Duration::from_secs(3),
        vec![
            Box::new(SdkProvidedResourceDetector),
            Box::new(EnvResourceDetector::new()),
        ],
    )
    .merge(&Resource::new(vec![
        KeyValue::new("service.name", service_name),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
    ]));

    // Configure OTLP exporter for traces with Axiom headers
    tracing::debug!("Setting up OTLP exporter with endpoint: {}", otlp_endpoint);
    
    // Try HTTP protocol instead of gRPC - Axiom might work better with HTTP
    let trace_exporter = opentelemetry_otlp::new_exporter()
        .http()
        .with_endpoint(format!("{}/v1/traces", otlp_endpoint))
        .with_headers(std::collections::HashMap::from([
            ("authorization".to_string(), format!("Bearer {}", axiom_token)),
            ("x-axiom-dataset".to_string(), axiom_dataset.clone()),
        ]))
        .with_timeout(std::time::Duration::from_secs(10))
        .with_http_client(reqwest::Client::new());

    tracing::debug!("Installing OpenTelemetry pipeline...");
    
    // Create tracer provider with batch processing  
    let tracer_provider = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(trace_exporter)
        .with_trace_config(
            opentelemetry_sdk::trace::Config::default().with_resource(resource),
        )
        .install_batch(runtime::Tokio)
        .map_err(|e| {
            tracing::error!("Failed to install OpenTelemetry pipeline: {:?}", e);
            e
        })?;
    
    tracing::debug!("OpenTelemetry pipeline installed successfully");

    // Create a test span to verify Axiom connectivity
    use opentelemetry::trace::{Tracer, TracerProvider as _, Status};
    let tracer = tracer_provider.tracer("irys-test");
    
    tracing::info!("Creating test span...");
    let mut span = tracer.start("axiom_connectivity_test");
    span.set_attribute(KeyValue::new("test.message", "Hello from Irys!"));
    span.set_attribute(KeyValue::new("test.version", env!("CARGO_PKG_VERSION")));
    span.set_status(Status::Ok);
    span.end();
    
    tracing::info!("Test span created and ended");
    
    // Force flush immediately to send the test span right away
    tracing::info!("Flushing test span to Axiom...");
    let flush_results = tracer_provider.force_flush();
    let mut flush_success = true;
    for result in flush_results {
        if let Err(e) = result {
            tracing::error!("Error flushing test span: {:?}", e);
            flush_success = false;
        }
    }
    
    if flush_success {
        tracing::info!("✅ Test span flushed successfully - should appear in Axiom within seconds!");
    } else {
        tracing::warn!("⚠️  Test span flush had errors - check logs above");
    }
    
    // Store the provider globally so it doesn't get dropped
    TRACER_PROVIDER.set(tracer_provider).ok();
    
    // Just use the standard console logging for now
    // The traces will be sent via the global provider
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_line_number(true)
        .with_file(true)
        .with_ansi(true)
        .with_target(true);

    // Get filter from environment
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    // Build and initialize subscriber
    Registry::default()
        .with(filter)
        .with(fmt_layer)
        .init();

    Ok(())
}

/// Shutdown OpenTelemetry gracefully
/// Call this before your application exits to ensure all pending traces are sent
#[cfg(feature = "telemetry")]
pub fn shutdown_telemetry() {
    tracing::info!("Shutting down OpenTelemetry, flushing pending traces...");
    if let Some(provider) = TRACER_PROVIDER.get() {
        // Force flush before shutdown
        let results = provider.force_flush();
        for result in results {
            if let Err(e) = result {
                tracing::error!("Error flushing traces: {:?}", e);
            }
        }
    }
    tracing::info!("OpenTelemetry shutdown complete");
}

// No-op implementations when telemetry feature is disabled
#[cfg(not(feature = "telemetry"))]
pub fn init_telemetry() -> Result<()> {
    tracing::warn!("Telemetry feature is not enabled, skipping OpenTelemetry initialization");
    Ok(())
}

#[cfg(not(feature = "telemetry"))]
pub fn shutdown_telemetry() {
    // No-op
}
