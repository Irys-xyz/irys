//! Correlation-id helpers sourced from the active OpenTelemetry span.
//!
//! `tracing-actix-web`'s `TracingLogger` (with the `opentelemetry_0_31`
//! feature) wraps each request in a span bridged to an OTEL context, so the
//! W3C Trace Context id is available wherever a response is built.

use opentelemetry::trace::{TraceContextExt as _, TraceId};
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

/// The active request's W3C trace id as 32 lowercase hex chars.
///
/// Returns the all-zero invalid id (`"00000000000000000000000000000000"`) when
/// no OTEL trace is active — e.g. in tests with no telemetry pipeline.
pub(crate) fn current_trace_id() -> String {
    let context = Span::current().context();
    let span = context.span();
    let span_context = span.span_context();
    if span_context.is_valid() {
        span_context.trace_id().to_string()
    } else {
        TraceId::INVALID.to_string()
    }
}

/// The active request's `traceparent` header value (W3C Trace Context), if a
/// valid trace is active. `None` when there is no telemetry pipeline.
pub(crate) fn current_traceparent() -> Option<String> {
    let context = Span::current().context();
    let span = context.span();
    let span_context = span.span_context();
    if span_context.is_valid() {
        Some(format!(
            "00-{}-{}-{:02x}",
            span_context.trace_id(),
            span_context.span_id(),
            span_context.trace_flags().to_u8()
        ))
    } else {
        None
    }
}
