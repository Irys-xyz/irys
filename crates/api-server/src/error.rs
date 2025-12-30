use actix_web::{body::BoxBody, HttpResponse, ResponseError};
use irys_types::{AddressParseError, DataLedger, IrysAddress};
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
    #[error("Invalid transactions version: {version} minimum version: {minimum}")]
    InvalidTransactionVersion { version: u8, minimum: u8 },
    #[error("{0}")]
    Custom(String),
    #[error("{0}")]
    CustomWithStatus(String, StatusCode),
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
            Self::InvalidAddressFormat { .. } => StatusCode::BAD_REQUEST,
            Self::BalanceUnavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::InvalidBlockParameter { .. } => StatusCode::BAD_REQUEST,
            Self::InvalidTransactionVersion { .. } => StatusCode::BAD_REQUEST,
            Self::Custom(_) => StatusCode::BAD_REQUEST,
            Self::CustomWithStatus(_, sc) => *sc,
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
