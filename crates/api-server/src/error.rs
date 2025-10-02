use actix_web::{body::BoxBody, HttpResponse, ResponseError};
use irys_types::{Address, AddressParseError, DataLedger};
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
    MinerNotFound { miner_address: Address },
    #[error("No {ledger_type} ledger assignments found for miner: {miner_address}")]
    LedgerNotFound {
        miner_address: Address,
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
}

impl ApiError {
    /// Helper method for converting eyre errors to canonical chain errors
    pub fn canonical_chain_error(err: impl std::fmt::Display) -> Self {
        Self::CanonicalChainError(err.to_string())
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
        }
    }

    fn error_response(&self) -> HttpResponse<BoxBody> {
        #[derive(Serialize)]
        struct ErrorResponse {
            error: String,
        }

        let error_response = ErrorResponse {
            error: self.to_string(),
        };

        let body = serde_json::to_string(&error_response).unwrap();
        let res = HttpResponse::new(self.status_code());
        res.set_body(BoxBody::new(body))
    }
}
