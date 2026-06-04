use actix_web::{HttpResponse, ResponseError, body::BoxBody};
use irys_types::{AddressParseError, DataLedger, IrysAddress, Problem, ProblemItem};
use serde::Serialize;

use awc::http::StatusCode;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("No ID found: {id} - {err}")]
    ErrNoId { id: String, err: String },
    #[error("Internal error: {err}")]
    Internal { err: String },
    #[error("Not implemented: {feature}")]
    NotImplemented { feature: String },
    #[error("Miner not found: {miner_address}")]
    MinerNotFound { miner_address: IrysAddress },
    #[error("No {ledger_type} ledger assignments found for miner: {miner_address}")]
    LedgerNotFound {
        miner_address: IrysAddress,
        ledger_type: DataLedger,
    },
    #[error("Invalid address: {0}")]
    InvalidAddress(#[from] AddressParseError),
    #[error("Failed to retrieve canonical chain: {0}")]
    CanonicalChainError(String),
    #[error("Canonical chain is empty - no blocks found")]
    EmptyCanonicalChain,
    #[error("Block not found: {block_hash}")]
    BlockNotFound { block_hash: String },
    #[error("Invalid address format '{address}': {error}")]
    InvalidAddressFormat { address: String, error: String },
    #[error("Balance unavailable: {reason}")]
    BalanceUnavailable { reason: String },
    #[error("Invalid block parameter: {parameter}")]
    InvalidBlockParameter { parameter: String },
    #[error("Invalid transaction version: {version}, minimum required version: {minimum}")]
    InvalidTransactionVersion { version: u8, minimum: u8 },
    #[error("{0}")]
    Custom(String),
    #[error("{0}")]
    CustomWithStatus(String, StatusCode),
    /// Aggregated field-level validation failures, rendered as RFC 9457 `errors[]`.
    #[error("Request validation failed with {} error(s)", errors.len())]
    Validation { errors: Vec<ProblemItem> },
    /// Request body could not be parsed/decoded (e.g. malformed JSON).
    #[error("{detail}")]
    MalformedBody { detail: String },
    /// A path parameter could not be parsed.
    #[error("{detail}")]
    InvalidPathParameter { detail: String },
    /// A query parameter could not be parsed.
    #[error("{detail}")]
    InvalidQueryParameter { detail: String },
    /// No route matched the request.
    #[error("No route matches {method} {path}")]
    RouteNotFound { method: String, path: String },
}

impl ApiError {
    /// Helper method for converting eyre errors to canonical chain errors
    pub fn canonical_chain_error(err: impl std::fmt::Display) -> Self {
        Self::CanonicalChainError(err.to_string())
    }

    /// Stable machine-readable code clients switch on (RFC 9457 extension).
    fn code(&self) -> String {
        match self {
            Self::ErrNoId { .. } => "ID_NOT_FOUND".to_string(),
            Self::Internal { .. } => "INTERNAL".to_string(),
            Self::NotImplemented { .. } => "NOT_IMPLEMENTED".to_string(),
            Self::MinerNotFound { .. } => "MINER_NOT_FOUND".to_string(),
            Self::LedgerNotFound { .. } => "LEDGER_NOT_FOUND".to_string(),
            Self::InvalidAddress(_) => "INVALID_ADDRESS".to_string(),
            Self::CanonicalChainError(_) => "CANONICAL_CHAIN_ERROR".to_string(),
            Self::EmptyCanonicalChain => "EMPTY_CANONICAL_CHAIN".to_string(),
            Self::BlockNotFound { .. } => "BLOCK_NOT_FOUND".to_string(),
            Self::InvalidAddressFormat { .. } => "INVALID_ADDRESS_FORMAT".to_string(),
            Self::BalanceUnavailable { .. } => "BALANCE_UNAVAILABLE".to_string(),
            Self::InvalidBlockParameter { .. } => "INVALID_BLOCK_PARAMETER".to_string(),
            Self::InvalidTransactionVersion { .. } => "INVALID_TRANSACTION_VERSION".to_string(),
            Self::Custom(_) => "BAD_REQUEST".to_string(),
            // Ad-hoc tuple errors (e.g. from tx.rs) inherit a code from their status.
            Self::CustomWithStatus(_, sc) => status_code_token(*sc),
            Self::Validation { .. } => "VALIDATION_FAILED".to_string(),
            Self::MalformedBody { .. } => "MALFORMED_BODY".to_string(),
            Self::InvalidPathParameter { .. } => "INVALID_PATH_PARAMETER".to_string(),
            Self::InvalidQueryParameter { .. } => "INVALID_QUERY_PARAMETER".to_string(),
            Self::RouteNotFound { .. } => "ROUTE_NOT_FOUND".to_string(),
        }
    }

    /// Short, human-readable summary, stable per error class (RFC 9457 `title`).
    fn title(&self) -> String {
        match self {
            Self::ErrNoId { .. } => "Resource not found".to_string(),
            Self::Internal { .. } => "Internal server error".to_string(),
            Self::NotImplemented { .. } => "Not implemented".to_string(),
            Self::MinerNotFound { .. } => "Miner not found".to_string(),
            Self::LedgerNotFound { .. } => "Ledger assignments not found".to_string(),
            Self::InvalidAddress(_) => "Invalid address".to_string(),
            Self::CanonicalChainError(_) => "Canonical chain error".to_string(),
            Self::EmptyCanonicalChain => "Canonical chain empty".to_string(),
            Self::BlockNotFound { .. } => "Block not found".to_string(),
            Self::InvalidAddressFormat { .. } => "Invalid address format".to_string(),
            Self::BalanceUnavailable { .. } => "Balance unavailable".to_string(),
            Self::InvalidBlockParameter { .. } => "Invalid block parameter".to_string(),
            Self::InvalidTransactionVersion { .. } => "Invalid transaction version".to_string(),
            Self::Custom(_) => "Bad request".to_string(),
            Self::CustomWithStatus(_, sc) => sc.canonical_reason().unwrap_or("Error").to_string(),
            Self::Validation { .. } => "Validation failed".to_string(),
            Self::MalformedBody { .. } => "Malformed request body".to_string(),
            Self::InvalidPathParameter { .. } => "Invalid path parameter".to_string(),
            Self::InvalidQueryParameter { .. } => "Invalid query parameter".to_string(),
            Self::RouteNotFound { .. } => "Route not found".to_string(),
        }
    }

    /// Build the RFC 9457 [`Problem`] for this error, stamped with `trace_id`.
    pub fn to_problem(&self, trace_id: String) -> Problem {
        let status = self.status_code();
        // Never expose internal 5xx specifics to clients: `code`/`title` convey
        // the class, and the traceId is the handle for the server-side log.
        let detail = if status.is_server_error() {
            "An unexpected internal error occurred. Quote the traceId when reporting this."
                .to_string()
        } else {
            self.to_string()
        };
        let errors = match self {
            Self::Validation { errors } => errors.clone(),
            _ => Vec::new(),
        };
        let code = self.code();
        Problem {
            r#type: problem_type(&code),
            title: self.title(),
            status: status.as_u16(),
            detail: Some(detail.clone()),
            code,
            trace_id,
            errors,
            // Deprecated migration mirror of `detail` for legacy clients.
            error: Some(detail),
        }
    }
}

/// Canonical problem `type` URI derived from the machine code, e.g.
/// `BLOCK_NOT_FOUND` -> `https://docs.irys.xyz/api/errors/block-not-found`.
pub(crate) fn problem_type(code: &str) -> String {
    format!(
        "https://docs.irys.xyz/api/errors/{}",
        code.to_lowercase().replace('_', "-")
    )
}

/// SCREAMING_SNAKE code from a status' canonical reason, e.g.
/// `402 Payment Required` -> `PAYMENT_REQUIRED`.
pub(crate) fn status_code_token(status: StatusCode) -> String {
    status
        .canonical_reason()
        .unwrap_or("ERROR")
        .to_uppercase()
        .replace(' ', "_")
}

/// Synthesize a [`Problem`] from a bare HTTP status, for framework-level
/// responses (404/405/413/panics) that never carried an [`ApiError`]. Used by
/// the response-normalizing middleware as the last-resort backstop.
pub(crate) fn problem_from_status(status: StatusCode, trace_id: String) -> Problem {
    let code = status_code_token(status);
    let title = status.canonical_reason().unwrap_or("Error").to_string();
    Problem {
        r#type: problem_type(&code),
        title: title.clone(),
        status: status.as_u16(),
        detail: Some(title.clone()),
        code,
        trace_id,
        errors: Vec::new(),
        error: Some(title),
    }
}

impl ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        match self {
            Self::ErrNoId { .. } => StatusCode::NOT_FOUND,
            Self::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            Self::NotImplemented { .. } => StatusCode::FORBIDDEN,
            Self::MinerNotFound { .. } => StatusCode::NOT_FOUND,
            Self::LedgerNotFound { .. } => StatusCode::NOT_FOUND,
            Self::InvalidAddress(_) => StatusCode::BAD_REQUEST,
            Self::CanonicalChainError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::EmptyCanonicalChain => StatusCode::SERVICE_UNAVAILABLE,
            Self::BlockNotFound { .. } => StatusCode::NOT_FOUND,
            Self::InvalidAddressFormat { .. } => StatusCode::BAD_REQUEST,
            Self::BalanceUnavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::InvalidBlockParameter { .. } => StatusCode::BAD_REQUEST,
            Self::InvalidTransactionVersion { .. } => StatusCode::BAD_REQUEST,
            Self::Custom(_) => StatusCode::BAD_REQUEST,
            Self::CustomWithStatus(_, sc) => *sc,
            Self::Validation { .. } => StatusCode::BAD_REQUEST,
            Self::MalformedBody { .. } => StatusCode::BAD_REQUEST,
            Self::InvalidPathParameter { .. } => StatusCode::BAD_REQUEST,
            Self::InvalidQueryParameter { .. } => StatusCode::BAD_REQUEST,
            Self::RouteNotFound { .. } => StatusCode::NOT_FOUND,
        }
    }

    fn error_response(&self) -> HttpResponse<BoxBody> {
        let trace_id = crate::trace::current_trace_id();
        // The genericized 5xx body hides internals from clients; capture the real
        // error here, correlated by trace_id, so operators can still diagnose it.
        if self.status_code().is_server_error() {
            tracing::error!(%trace_id, error = %self, "internal API error");
        }
        let problem = self.to_problem(trace_id);
        // `Problem` is plain serde data; serialization cannot realistically fail.
        let body = serde_json::to_string(&problem).unwrap_or_else(|_| {
            r#"{"type":"https://docs.irys.xyz/api/errors/internal","title":"Internal server error","status":500,"code":"INTERNAL","traceId":""}"#.to_string()
        });
        HttpResponse::build(self.status_code())
            .content_type("application/problem+json")
            .body(body)
    }
}

impl From<(&str, StatusCode)> for ApiError {
    fn from(value: (&str, StatusCode)) -> Self {
        Self::CustomWithStatus(value.0.to_string(), value.1)
    }
}

impl From<(String, StatusCode)> for ApiError {
    fn from(value: (String, StatusCode)) -> Self {
        Self::CustomWithStatus(value.0, value.1)
    }
}

// TODO: move this somewhere smarter

pub struct ApiStatusResponse(pub String, pub StatusCode);

impl From<(String, StatusCode)> for ApiStatusResponse {
    fn from(value: (String, StatusCode)) -> Self {
        Self(value.0, value.1)
    }
}

impl From<(&str, StatusCode)> for ApiStatusResponse {
    fn from(value: (&str, StatusCode)) -> Self {
        Self(value.0.to_string(), value.1)
    }
}

impl From<ApiStatusResponse> for HttpResponse {
    fn from(val: ApiStatusResponse) -> Self {
        #[derive(Serialize)]
        struct Status {
            // informative status message
            // we use JSON so we can extend it etc later
            status: String,
        }

        let response = Status { status: val.0 };

        let body = serde_json::to_string(&response).unwrap();
        let res = Self::new(val.1);
        res.set_body(BoxBody::new(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case(ApiError::ErrNoId { id: "x".into(), err: "y".into() }, StatusCode::NOT_FOUND)]
    #[case(ApiError::Internal { err: "x".into() }, StatusCode::INTERNAL_SERVER_ERROR)]
    #[case(ApiError::NotImplemented { feature: "x".into() }, StatusCode::FORBIDDEN)]
    #[case(ApiError::MinerNotFound { miner_address: IrysAddress::from([0_u8; 20]) }, StatusCode::NOT_FOUND)]
    #[case(ApiError::LedgerNotFound { miner_address: IrysAddress::from([0_u8; 20]), ledger_type: DataLedger::Publish }, StatusCode::NOT_FOUND)]
    #[case(ApiError::CanonicalChainError("x".into()), StatusCode::INTERNAL_SERVER_ERROR)]
    #[case(ApiError::EmptyCanonicalChain, StatusCode::SERVICE_UNAVAILABLE)]
    #[case(ApiError::BlockNotFound { block_hash: "x".into() }, StatusCode::NOT_FOUND)]
    #[case(ApiError::InvalidAddressFormat { address: "x".into(), error: "y".into() }, StatusCode::BAD_REQUEST)]
    #[case(ApiError::BalanceUnavailable { reason: "x".into() }, StatusCode::SERVICE_UNAVAILABLE)]
    #[case(ApiError::InvalidBlockParameter { parameter: "x".into() }, StatusCode::BAD_REQUEST)]
    #[case(ApiError::InvalidTransactionVersion { version: 0, minimum: 1 }, StatusCode::BAD_REQUEST)]
    #[case(ApiError::InvalidAddress(AddressParseError::InvalidFormat("bad".into())), StatusCode::BAD_REQUEST)]
    #[case(ApiError::Custom("x".into()), StatusCode::BAD_REQUEST)]
    #[case(ApiError::CustomWithStatus("x".into(), StatusCode::CONFLICT), StatusCode::CONFLICT)]
    fn status_code_mapping(#[case] error: ApiError, #[case] expected: StatusCode) {
        assert_eq!(ResponseError::status_code(&error), expected);
    }

    #[test]
    fn from_str_status_tuple() {
        let error = ApiError::from(("msg", StatusCode::CONFLICT));
        match error {
            ApiError::CustomWithStatus(msg, sc) => {
                assert_eq!(msg, "msg");
                assert_eq!(sc, StatusCode::CONFLICT);
            }
            other => panic!("Expected CustomWithStatus, got {other:?}"),
        }
    }

    #[test]
    fn maps_single_error_to_rfc9457_problem() {
        let err = ApiError::BlockNotFound {
            block_hash: "0xabc".into(),
        };
        let p = err.to_problem("trace-x".into());

        assert_eq!(p.status, 404);
        assert_eq!(p.code, "BLOCK_NOT_FOUND");
        assert_eq!(p.r#type, "https://docs.irys.xyz/api/errors/block-not-found");
        assert_eq!(p.title, "Block not found");
        assert_eq!(p.detail.as_deref(), Some("Block not found: 0xabc"));
        assert_eq!(p.trace_id, "trace-x");
        // deprecated migration mirror duplicates `detail`
        assert_eq!(p.error.as_deref(), Some("Block not found: 0xabc"));
        assert!(p.errors.is_empty());
    }

    #[test]
    fn validation_variant_carries_errors_array() {
        let err = ApiError::Validation {
            errors: vec![ProblemItem {
                code: "INVALID_SIGNATURE".into(),
                detail: "signature does not match signer".into(),
                pointer: Some("/signature".into()),
            }],
        };
        assert_eq!(ResponseError::status_code(&err), StatusCode::BAD_REQUEST);

        let p = err.to_problem("t".into());
        assert_eq!(p.code, "VALIDATION_FAILED");
        assert_eq!(
            p.r#type,
            "https://docs.irys.xyz/api/errors/validation-failed"
        );
        assert_eq!(p.errors.len(), 1);
        assert_eq!(p.errors[0].code, "INVALID_SIGNATURE");
        assert_eq!(p.errors[0].pointer.as_deref(), Some("/signature"));
    }

    #[test]
    fn custom_with_status_derives_code_from_status() {
        // tx.rs's ad-hoc tuple errors inherit a code/type for free.
        let err = ApiError::CustomWithStatus("nope".into(), StatusCode::PAYMENT_REQUIRED);
        let p = err.to_problem("t".into());
        assert_eq!(p.status, 402);
        assert_eq!(p.code, "PAYMENT_REQUIRED");
        assert_eq!(p.detail.as_deref(), Some("nope"));
    }

    #[test]
    fn server_errors_do_not_leak_internal_detail() {
        // 5xx must not expose internal messages; code/title still convey the class
        // and the traceId is the correlation handle.
        let err = ApiError::Internal {
            err: "postgres://secret@host connection refused".into(),
        };
        let p = err.to_problem("t".into());

        assert_eq!(p.status, 500);
        assert_eq!(p.code, "INTERNAL");
        let detail = p.detail.as_deref().unwrap_or_default();
        assert!(
            !detail.contains("secret"),
            "5xx detail leaked internals: {detail}"
        );
        let mirror = p.error.as_deref().unwrap_or_default();
        assert!(
            !mirror.contains("secret"),
            "5xx error mirror leaked internals: {mirror}"
        );
    }
}

#[cfg(test)]
mod response_tests {
    use super::*;
    use actix_web::{App, HttpResponse, test, web};

    async fn boom() -> Result<HttpResponse, ApiError> {
        Err(ApiError::BlockNotFound {
            block_hash: "0xabc".into(),
        })
    }

    #[actix_web::test]
    async fn handler_error_renders_problem_json() {
        let app = test::init_service(App::new().route("/boom", web::get().to(boom))).await;
        let resp =
            test::call_service(&app, test::TestRequest::get().uri("/boom").to_request()).await;

        assert_eq!(resp.status().as_u16(), 404);
        let content_type = resp
            .headers()
            .get(actix_web::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            content_type.starts_with("application/problem+json"),
            "expected problem+json, got {content_type:?}"
        );

        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["status"], 404);
        assert_eq!(body["code"], "BLOCK_NOT_FOUND");
        assert_eq!(body["title"], "Block not found");
        assert!(body.get("traceId").is_some(), "traceId must be present");
    }
}
