use actix_web::{
    web::{self, Data, Payload},
    HttpRequest, HttpResponse,
};
use awc::Client;

use crate::ApiState;

#[derive(Debug)]
pub enum ProxyError {
    RequestError(awc::error::SendRequestError),
    ParseError(String),
    MethodNotAllowed,
}

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RequestError(e) => write!(f, "Request error: {}", e),
            Self::ParseError(e) => write!(f, "Parse error: {}", e),
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
    .no_decompress(); // <- very important!

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
        | &actix_web::http::Method::PATCH => client_req
            .send_stream(payload)
            .await
            .map_err(ProxyError::RequestError)?,
        _ => client_req.send().await.map_err(ProxyError::RequestError)?,
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
