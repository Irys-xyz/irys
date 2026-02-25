use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use irys_utils::ElapsedMs as _;
use opentelemetry::KeyValue;
use std::{
    future::{ready, Future, Ready},
    pin::Pin,
    time::Instant,
};

irys_utils::define_metrics! {
    meter: "irys-api-server";

    counter CHUNKS_RECEIVED("irys.api.chunks.received_total", "Total chunks received via API");
    counter BYTES_RECEIVED("irys.api.chunks.bytes_received_total", "Total bytes received in chunk payloads");
    counter CHUNK_ERRORS("irys.api.chunks.errors_total", "Chunk processing errors by type");
    histogram CHUNK_PROCESSING_MS("irys.api.chunks.processing_duration_ms", "Chunk processing latency in milliseconds", vec![0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0]);
    histogram REQUEST_DURATION_MS("irys.api.http.request_duration_ms", "HTTP request processing latency in milliseconds", vec![0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0]);
    counter REQUESTS_TOTAL("irys.api.http.requests_total", "Total HTTP requests by method, path, and status");
}

pub fn record_chunk_received(bytes: u64) {
    CHUNKS_RECEIVED.add(1, &[]);
    BYTES_RECEIVED.add(bytes, &[]);
}

pub fn record_chunk_processing_duration(ms: f64) {
    CHUNK_PROCESSING_MS.record(ms, &[]);
}

pub fn record_chunk_error(error_type: &'static str, is_advisory: bool) {
    CHUNK_ERRORS.add(
        1,
        &[
            KeyValue::new("error_type", error_type),
            KeyValue::new("advisory", is_advisory),
        ],
    );
}

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
        ready(Ok(RequestMetricsMiddleware { service }))
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
        let path = req
            .match_pattern()
            .unwrap_or_else(|| "unmatched".to_string());
        let start = Instant::now();
        let fut = self.service.call(req);

        Box::pin(async move {
            let res = fut.await?;
            let status = res.status().as_u16().to_string();
            let duration_ms = start.elapsed_ms();

            let attrs = [
                KeyValue::new("method", method),
                KeyValue::new("path", path),
                KeyValue::new("status", status),
            ];

            REQUEST_DURATION_MS.record(duration_ms, &attrs);
            REQUESTS_TOTAL.add(1, &attrs);

            Ok(res)
        })
    }
}
