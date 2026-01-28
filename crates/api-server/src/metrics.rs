use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use futures::future::{ok, Ready};
use opentelemetry::{global, KeyValue};
use std::{future::Future, pin::Pin, time::Instant};

fn meter() -> opentelemetry::metrics::Meter {
    global::meter("irys-api-server")
}

pub fn record_chunk_received(bytes: u64) {
    let m = meter();
    m.u64_counter("irys.api.chunks.received_total")
        .with_description("Total chunks received via API")
        .build()
        .add(1, &[]);
    m.u64_counter("irys.api.chunks.bytes_received_total")
        .with_description("Total bytes received in chunk payloads")
        .build()
        .add(bytes, &[]);
}

pub fn record_chunk_error(error_type: &'static str, is_advisory: bool) {
    meter()
        .u64_counter("irys.api.chunks.errors_total")
        .with_description("Chunk processing errors by type")
        .build()
        .add(
            1,
            &[
                KeyValue::new("error_type", error_type),
                KeyValue::new("advisory", is_advisory),
            ],
        );
}

/// Actix-web middleware that records request duration and status for every route.
///
/// Emits:
/// - `irys.api.http.request_duration_ms` histogram with `method`, `path`, `status` attributes
/// - `irys.api.http.requests_total` counter with `method`, `path`, `status` attributes
pub struct RequestMetrics;

impl<S, B> Transform<S, ServiceRequest> for RequestMetrics
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = actix_web::Error>,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = actix_web::Error;
    type Transform = RequestMetricsMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(RequestMetricsMiddleware { service })
    }
}

pub struct RequestMetricsMiddleware<S> {
    service: S,
}

impl<S, B> Service<ServiceRequest> for RequestMetricsMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = actix_web::Error>,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = actix_web::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let method = req.method().to_string();
        // Use the matched route pattern (e.g. "/v1/tx/{tx_id}") for low-cardinality labels,
        // falling back to the raw path only if no pattern is matched.
        let path = req
            .match_pattern()
            .unwrap_or_else(|| req.path().to_string());
        let start = Instant::now();
        let fut = self.service.call(req);

        Box::pin(async move {
            let res = fut.await?;
            let status = res.status().as_u16().to_string();
            let duration_ms = start.elapsed().as_secs_f64() * 1000.0;

            let attrs = [
                KeyValue::new("method", method),
                KeyValue::new("path", path),
                KeyValue::new("status", status),
            ];

            let m = meter();
            m.f64_histogram("irys.api.http.request_duration_ms")
                .with_description("HTTP request processing latency in milliseconds")
                .build()
                .record(duration_ms, &attrs);
            m.u64_counter("irys.api.http.requests_total")
                .with_description("Total HTTP requests by method, path, and status")
                .build()
                .add(1, &attrs);

            Ok(res)
        })
    }
}
