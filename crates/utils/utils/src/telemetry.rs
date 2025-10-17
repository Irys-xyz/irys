//! Simple OpenTelemetry integration for sending logs to Axiom
//!
//! This is a minimal proof-of-concept implementation that sends logs to Axiom
//! using OpenTelemetry OTLP protocol.

use eyre::Result;

#[cfg(feature = "telemetry")]
use opentelemetry::{trace::Span as _, KeyValue};
#[cfg(feature = "telemetry")]
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
#[cfg(feature = "telemetry")]
use opentelemetry_sdk::resource::Resource;
#[cfg(feature = "telemetry")]
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Registry};
//
#[cfg(feature = "telemetry")]
use tracing_error::ErrorLayer;

// Store the tracer provider globally so it doesn't get dropped
#[cfg(feature = "telemetry")]
static TRACER_PROVIDER: std::sync::OnceLock<opentelemetry_sdk::trace::SdkTracerProvider> =
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
    let service_name =
        std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "irys-node".to_string());

    // Print early messages directly so users see progress before subscriber is installed
    println!(
        "Initializing OpenTelemetry with Axiom backend (service_name={}, dataset={}, endpoint={})",
        service_name, axiom_dataset, otlp_endpoint
    );

        // Create resource with service information
    let resource = Resource::builder_empty()
        .with_service_name(service_name.clone())
        .with_attributes([
            KeyValue::new(
                opentelemetry_semantic_conventions::resource::SERVICE_VERSION,
                env!("CARGO_PKG_VERSION"),
            ),
        ])
        .build();

    // Configure OTLP exporter for traces with Axiom headers
    // Note: using direct prints before subscriber init
    eprintln!("[telemetry] Setting up OTLP exporter with endpoint: {}", otlp_endpoint);
    
    let trace_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(format!("{}/v1/traces", otlp_endpoint))
        .with_headers(std::collections::HashMap::from([
            (
                "authorization".to_string(),
                format!("Bearer {}", axiom_token),
            ),
            ("x-axiom-dataset".to_string(), axiom_dataset.clone()),
        ]))
        .with_timeout(std::time::Duration::from_secs(10))
        .with_http_client(reqwest::Client::new())
        .build()
        .map_err(|e| {
            eprintln!("[telemetry] Failed to build OTLP exporter: {:?}", e);
            e
        })?;

    eprintln!("[telemetry] Installing OpenTelemetry pipeline...");

    // Use SimpleSpanProcessor instead of BatchSpanProcessor
    // BatchSpanProcessor requires a Tokio runtime context which may not be available yet
    // SimpleSpanProcessor exports synchronously which is more reliable during initialization
    let span_processor = opentelemetry_sdk::trace::SimpleSpanProcessor::new(trace_exporter);
    
    let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_resource(resource)
        .with_span_processor(span_processor)
        .build();

    eprintln!("[telemetry] OpenTelemetry pipeline installed successfully");

    // Store the provider in our static only
    // Don't use opentelemetry::global::set_tracer_provider as its Drop can block
    // Instead, we'll keep a reference alive for the program lifetime
    let provider_for_static = tracer_provider.clone();
    let provider_for_use = tracer_provider;
    
    TRACER_PROVIDER.set(provider_for_static).ok();
    
    // Leak the local provider to prevent its Drop from blocking on shutdown
    std::mem::forget(provider_for_use.clone());

    // Now create tracers from our reference
    use opentelemetry::trace::{Status, Tracer, TracerProvider as _};
    
    // Create a test span to verify Axiom connectivity
    let test_tracer = provider_for_use.tracer("irys-test");

    println!("[telemetry] Creating test span...");
    let mut span = test_tracer.start("axiom_connectivity_test");
    span.set_attribute(KeyValue::new("test.message", "Hello from Irys!"));
    span.set_attribute(KeyValue::new("test.version", env!("CARGO_PKG_VERSION")));
    span.set_status(Status::Ok);
    span.end();

    println!("[telemetry] Test span created and ended");

    // Force flush immediately to send the test span right away
    println!("[telemetry] Flushing test span to Axiom...");
    if let Err(e) = provider_for_use.force_flush() {
        eprintln!("[telemetry] Error flushing test span: {:?}", e);
        eprintln!("[telemetry] ⚠️  Test span flush failed - check logs above");
    } else {
        println!("[telemetry] ✅ Test span flushed successfully - should appear in Axiom within seconds!");
    }

    // Create a tracer for the OpenTelemetry layer  
    let tracer = provider_for_use.tracer("irys-node");
    
    // Create the OpenTelemetry layer that converts tracing spans to OTel spans
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    
    // Create the fmt layer for console output (terminal logging)
    // Write to stdout to align with other binaries and typical container logging
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_line_number(true)
        .with_file(true)
        .with_ansi(true)
        .with_target(true)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NEW | tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .with_writer(std::io::stdout);

    // Get filter from environment
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Build and initialize subscriber with BOTH layers
    // Both layers share the same filter at the Registry level
    // This means logs go to both the terminal (fmt_layer) AND Axiom (otel_layer)
    let subscriber = Registry::default()
        .with(filter)
        .with(ErrorLayer::default())
        .with(fmt_layer)
        .with(otel_layer);

    if let Err(_e) = subscriber.try_init() {
        // Don't panic if someone initialized tracing earlier (e.g., a test harness or external lib)
        eprintln!("[telemetry] Global subscriber was already set; skipping re-initialization");
    }

    tracing::info!(
        service_name = %service_name,
        dataset = %axiom_dataset,
        endpoint = %otlp_endpoint,
        "OpenTelemetry initialized: terminal + OTLP configured"
    );

    Ok(())
}

/// Shutdown OpenTelemetry and export remaining traces
///
/// Note: With SimpleSpanProcessor, shutdown may take a moment as it exports
/// all remaining spans synchronously. This is a known limitation.
/// 
/// If Ctrl+C appears to hang, it's because spans are being exported.
/// You can force-kill the process, but some recent spans may be lost.
///
/// For production use, consider switching to BatchSpanProcessor (requires
/// initialization within Tokio runtime context).
#[cfg(feature = "telemetry")]
pub fn shutdown_telemetry() {
    tracing::info!("Shutting down OpenTelemetry...");
    
    // Flush all pending spans
    // Note: SimpleSpanProcessor exports synchronously, so this may block
    if let Some(provider) = TRACER_PROVIDER.get() {
        if let Err(e) = provider.force_flush() {
            tracing::error!("Error flushing traces during shutdown: {:?}", e);
        }
    }
    
    tracing::info!("OpenTelemetry shutdown complete");
}

/// Flush pending traces without shutting down
///
/// Use this if you want to ensure traces are sent but keep the system running.
#[cfg(feature = "telemetry")]
pub fn flush_telemetry() {
    tracing::debug!("Flushing pending OpenTelemetry traces...");
    if let Some(provider) = TRACER_PROVIDER.get() {
        if let Err(e) = provider.force_flush() {
            tracing::error!("Error flushing traces: {:?}", e);
        } else {
            tracing::debug!("Traces flushed successfully");
        }
    }
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

#[cfg(not(feature = "telemetry"))]
pub fn flush_telemetry() {
    // No-op
}
