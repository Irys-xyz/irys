use actix_web::{
    HttpRequest, HttpResponse,
    web::{self, Data, Payload},
};
use awc::Client;
use std::time::Instant;
use tracing::{debug, info, warn};

use crate::{API_PROXY_UPSTREAM_TIMEOUT, ApiState};

#[derive(Debug)]
pub enum ProxyError {
    RequestError(awc::error::SendRequestError),
    ParseError(String),
    MethodNotAllowed,
}

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RequestError(e) => write!(f, "Request error: {e}"),
            Self::ParseError(e) => write!(f, "Parse error: {e}"),
            Self::MethodNotAllowed => write!(f, "Method not allowed"),
        }
    }
}

impl actix_web::ResponseError for ProxyError {
    fn error_response(&self) -> HttpResponse {
        match self {
            Self::RequestError(_) => HttpResponse::BadGateway().finish(),
            Self::ParseError(_) => HttpResponse::BadRequest().finish(),
            Self::MethodNotAllowed => HttpResponse::MethodNotAllowed().finish(),
        }
    }
}

pub async fn proxy(
    req: HttpRequest,
    payload: Payload,
    client: Data<Client>,
    state: web::Data<ApiState>,
) -> Result<HttpResponse, ProxyError> {
    let target_uri = &state.reth_http_url;
    let method = req.method().clone();
    let path = req.path().to_owned();
    let start = Instant::now();

    info!(
        http.method = %method,
        http.path = %path,
        target = %target_uri,
        "API proxy request started"
    );

    // Create a new client request
    let mut client_req = match *req.method() {
        actix_web::http::Method::GET => client.get(target_uri),
        actix_web::http::Method::POST => client.post(target_uri),
        actix_web::http::Method::PUT => client.put(target_uri),
        actix_web::http::Method::DELETE => client.delete(target_uri),
        actix_web::http::Method::HEAD => client.head(target_uri),
        actix_web::http::Method::OPTIONS => client.options(target_uri),
        actix_web::http::Method::PATCH => client.patch(target_uri),
        _ => return Err(ProxyError::MethodNotAllowed),
    }
    .no_decompress() // <- very important!
    .timeout(API_PROXY_UPSTREAM_TIMEOUT);

    // Forward relevant headers
    for (header_name, header_value) in req.headers() {
        // Skip hop-by-hop headers
        if !is_hop_by_hop_header(header_name.as_str()) {
            client_req = client_req.insert_header((header_name.clone(), header_value.clone()));
        }
    }

    // Send the request with the payload for methods that support it
    let response = match req.method() {
        &actix_web::http::Method::POST
        | &actix_web::http::Method::PUT
        | &actix_web::http::Method::PATCH => {
            client_req.send_stream(payload).await.map_err(|err| {
                warn!(
                    http.method = %method,
                    http.path = %path,
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    "API proxy upstream request failed: {}",
                    err
                );
                ProxyError::RequestError(err)
            })?
        }
        _ => client_req.send().await.map_err(|err| {
            warn!(
                http.method = %method,
                http.path = %path,
                elapsed_ms = start.elapsed().as_millis() as u64,
                "API proxy upstream request failed: {}",
                err
            );
            ProxyError::RequestError(err)
        })?,
    };

    // Build response
    let mut client_response = HttpResponse::build(response.status());

    // Forward response headers
    for (header_name, header_value) in response.headers() {
        if !is_hop_by_hop_header(header_name.as_str()) {
            client_response.insert_header((header_name.clone(), header_value.clone()));
        }
    }

    // Stream the response body
    debug!(
        http.method = %method,
        http.path = %path,
        status = response.status().as_u16(),
        elapsed_ms = start.elapsed().as_millis() as u64,
        "API proxy upstream response ready for streaming"
    );
    Ok(client_response.streaming(response))
}

const HOP_BY_HOP_HEADERS: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

fn is_hop_by_hop_header(header: &str) -> bool {
    HOP_BY_HOP_HEADERS.contains(&header.to_lowercase().as_str())
}
