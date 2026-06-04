//! Cross-cutting HTTP conventions that force every response onto the RFC 9457
//! `Problem` shape, regardless of where it originated.
//!
//! There are five places a response is born in actix; these cover the four that
//! are not a handler's own `Err(ApiError)`:
//! - body parse failures ([`json_error_config`]),
//! - path/query parse failures ([`path_error_config`] / [`query_error_config`]),
//! - unmatched routes ([`route_not_found`] as the app's `default_service`),
//! - anything else — framework 405/413, panics, a careless handler — caught by
//!   the [`normalize_problem`] middleware backstop.

use actix_web::{
    HttpRequest, HttpResponse,
    body::{BoxBody, MessageBody},
    dev::{ServiceRequest, ServiceResponse},
    error::Error,
    http::{
        StatusCode,
        header::{CONTENT_TYPE, HeaderName, HeaderValue},
    },
    middleware::Next,
    web::{JsonConfig, PathConfig, QueryConfig},
};
use tracing::warn;

use crate::{
    error::{ApiError, problem_from_status},
    trace::{current_trace_id, current_traceparent},
};

const PROBLEM_JSON: &str = "application/problem+json";

/// JSON body extractor config: 1MB limit, malformed bodies become an
/// `application/problem+json` `MALFORMED_BODY` instead of plain text.
pub fn json_error_config() -> JsonConfig {
    JsonConfig::default()
        .limit(1024 * 1024)
        .error_handler(|err, req| {
            warn!(path = %req.path(), %err, "JSON decode error");
            ApiError::MalformedBody {
                detail: format!("JSON decode/parse error: {err}"),
            }
            .into()
        })
}

/// Path extractor config: unparseable path params become a `Problem`.
pub fn path_error_config() -> PathConfig {
    PathConfig::default().error_handler(|err, _req| {
        ApiError::InvalidPathParameter {
            detail: err.to_string(),
        }
        .into()
    })
}

/// Query extractor config: unparseable query params become a `Problem`.
pub fn query_error_config() -> QueryConfig {
    QueryConfig::default().error_handler(|err, _req| {
        ApiError::InvalidQueryParameter {
            detail: err.to_string(),
        }
        .into()
    })
}

/// App `default_service` handler: any unmatched route becomes a 404 `Problem`.
pub async fn route_not_found(req: HttpRequest) -> Result<HttpResponse, ApiError> {
    Err(ApiError::RouteNotFound {
        method: req.method().to_string(),
        path: req.path().to_string(),
    })
}

/// Response-normalizing middleware (the backstop). For any response that is an
/// error status but not already `application/problem+json` — framework 405/413,
/// panics, a careless handler — it replaces the body with a `Problem`. It also
/// stamps the `traceparent` header on **every** response, success included.
pub async fn normalize_problem<B>(
    req: ServiceRequest,
    next: Next<B>,
) -> Result<ServiceResponse<BoxBody>, Error>
where
    B: MessageBody + 'static,
{
    let res = next.call(req).await?;
    let traceparent = current_traceparent();

    let status = res.status();
    let content_type = res
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());

    if should_synthesize_problem(status, content_type) {
        let (req, _discarded) = res.into_parts();
        let problem = problem_from_status(status, current_trace_id());
        let body = serde_json::to_string(&problem).unwrap_or_default();
        let mut response = HttpResponse::build(status)
            .content_type(PROBLEM_JSON)
            .body(body);
        stamp_traceparent(response.headers_mut(), traceparent);
        return Ok(ServiceResponse::new(req, response));
    }

    let mut res = res.map_into_boxed_body();
    stamp_traceparent(res.headers_mut(), traceparent);
    Ok(res)
}

/// Whether the backstop should replace a response with a synthesized `Problem`.
///
/// True only for error statuses whose body is empty or plain text — the
/// framework defaults (404/405/413, panics). Responses with a structured
/// `application/*` body are left untouched: this protects the `/execution-rpc`
/// proxy's verbatim JSON-RPC passthrough and any response that is already
/// `application/problem+json`.
fn should_synthesize_problem(status: StatusCode, content_type: Option<&str>) -> bool {
    let is_error = status.is_client_error() || status.is_server_error();
    let has_structured_body = content_type.is_some_and(|ct| !ct.starts_with("text/"));
    is_error && !has_structured_body
}

fn stamp_traceparent(
    headers: &mut actix_web::http::header::HeaderMap,
    traceparent: Option<String>,
) {
    if let Some(tp) = traceparent
        && let Ok(value) = HeaderValue::from_str(&tp)
    {
        headers.insert(HeaderName::from_static("traceparent"), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{
        App, HttpResponse, dev::ServiceResponse, http::header::CONTENT_TYPE, test, web,
    };

    async fn takes_json(_b: web::Json<serde_json::Value>) -> HttpResponse {
        HttpResponse::Ok().finish()
    }

    async fn raw_500() -> HttpResponse {
        HttpResponse::InternalServerError().body("oops")
    }

    fn content_type<B>(resp: &ServiceResponse<B>) -> String {
        resp.headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }

    #[actix_web::test]
    async fn malformed_json_body_renders_problem() {
        let app = test::init_service(
            App::new()
                .app_data(json_error_config())
                .route("/j", web::post().to(takes_json)),
        )
        .await;
        let req = test::TestRequest::post()
            .uri("/j")
            .insert_header(("content-type", "application/json"))
            .set_payload("{ not valid")
            .to_request();
        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status().as_u16(), 400);
        assert!(
            content_type(&resp).starts_with("application/problem+json"),
            "ct={}",
            content_type(&resp)
        );
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["code"], "MALFORMED_BODY");
    }

    #[actix_web::test]
    async fn unknown_route_renders_problem() {
        let app =
            test::init_service(App::new().default_service(web::route().to(route_not_found))).await;
        let resp =
            test::call_service(&app, test::TestRequest::get().uri("/nope").to_request()).await;

        assert_eq!(resp.status().as_u16(), 404);
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["code"], "ROUTE_NOT_FOUND");
    }

    #[actix_web::test]
    async fn middleware_normalizes_non_problem_error() {
        let app = test::init_service(
            App::new()
                .wrap(actix_web::middleware::from_fn(normalize_problem))
                .route("/boom", web::get().to(raw_500)),
        )
        .await;
        let resp =
            test::call_service(&app, test::TestRequest::get().uri("/boom").to_request()).await;

        assert_eq!(resp.status().as_u16(), 500);
        assert!(
            content_type(&resp).starts_with("application/problem+json"),
            "ct={}",
            content_type(&resp)
        );
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["code"], "INTERNAL_SERVER_ERROR");
        assert_eq!(body["status"], 500);
    }

    #[actix_web::test]
    async fn middleware_leaves_existing_problem_untouched() {
        async fn already_problem() -> Result<HttpResponse, crate::error::ApiError> {
            Err(crate::error::ApiError::BlockNotFound {
                block_hash: "h".into(),
            })
        }
        let app = test::init_service(
            App::new()
                .wrap(actix_web::middleware::from_fn(normalize_problem))
                .route("/np", web::get().to(already_problem)),
        )
        .await;
        let resp = test::call_service(&app, test::TestRequest::get().uri("/np").to_request()).await;

        let body: serde_json::Value = test::read_body_json(resp).await;
        // The handler's rich code must survive, not be flattened to NOT_FOUND.
        assert_eq!(body["code"], "BLOCK_NOT_FOUND");
    }

    #[actix_web::test]
    async fn middleware_preserves_structured_error_body() {
        // The /execution-rpc proxy forwards reth's verbatim JSON-RPC response,
        // including non-2xx statuses with an application/json body. The backstop
        // must NOT clobber a structured body — only empty/text framework errors.
        async fn proxied_jsonrpc_error() -> HttpResponse {
            HttpResponse::BadRequest()
                .content_type("application/json")
                .body(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"Invalid Request"}}"#)
        }
        let app = test::init_service(
            App::new()
                .wrap(actix_web::middleware::from_fn(normalize_problem))
                .route("/rpc", web::post().to(proxied_jsonrpc_error)),
        )
        .await;
        let resp =
            test::call_service(&app, test::TestRequest::post().uri("/rpc").to_request()).await;

        assert_eq!(resp.status().as_u16(), 400);
        assert!(
            content_type(&resp).starts_with("application/json"),
            "proxy content-type must be preserved, got {}",
            content_type(&resp)
        );
        let body: serde_json::Value = test::read_body_json(resp).await;
        // The reth JSON-RPC error must survive verbatim, not be replaced.
        assert_eq!(body["error"]["code"], -32600);
        assert!(body.get("type").is_none(), "must not be a Problem");
    }
}

// Pure-function tests live in their own module: importing `actix_web::test`
// above shadows the built-in `#[test]` attribute with actix's async one.
#[cfg(test)]
mod backstop_unit_tests {
    use super::*;

    #[test]
    fn synthesize_only_for_empty_or_text_error_bodies() {
        // Framework errors (no body / text) get a synthesized Problem...
        assert!(should_synthesize_problem(StatusCode::NOT_FOUND, None));
        assert!(should_synthesize_problem(
            StatusCode::BAD_REQUEST,
            Some("text/plain")
        ));
        // ...but structured bodies (proxy JSON, existing problem+json) are left alone.
        assert!(!should_synthesize_problem(
            StatusCode::BAD_REQUEST,
            Some("application/json")
        ));
        assert!(!should_synthesize_problem(
            StatusCode::NOT_FOUND,
            Some("application/problem+json")
        ));
        // Success responses are never touched.
        assert!(!should_synthesize_problem(StatusCode::OK, None));
    }
}
