use actix_web::{body::BoxBody, HttpResponse, ResponseError};
use serde::{Deserialize, Serialize};

use awc::http::StatusCode;

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub enum ApiError {
    ErrNoId { id: String, err: String },
    Internal { err: String },
}

impl ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        match self {
            ApiError::ErrNoId { .. } => StatusCode::NOT_FOUND,
            ApiError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn error_response(&self) -> HttpResponse<BoxBody> {
        let body = serde_json::to_string(&self).unwrap();
        let res = HttpResponse::new(self.status_code());
        res.set_body(BoxBody::new(body))
    }
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}
