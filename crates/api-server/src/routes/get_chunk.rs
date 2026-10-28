use crate::{ApiState, error::ApiError};
use actix_web::{
    HttpResponse,
    http::header::ContentType,
    web::{self},
};

use awc::http::StatusCode;
use base58::FromBase58 as _;
use irys_types::{ChunkFormat, DataCommitment, H256};
use serde::Deserialize;
use tracing::debug;

#[derive(Deserialize)]
pub struct LedgerChunkApiPath {
    ledger_id: u32,
    ledger_offset: u64,
}

pub async fn get_chunk_by_ledger_offset(
    state: web::Data<ApiState>,
    path: web::Path<LedgerChunkApiPath>,
) -> Result<HttpResponse, ApiError> {
    let ledger = crate::routes::parse_ledger_id(path.ledger_id)?;

    match state
        .chunk_provider
        .get_chunk_by_ledger_offset(ledger, path.ledger_offset.into())
    {
        Ok(Some(chunk)) => Ok(HttpResponse::Ok()
            .content_type(ContentType::json())
            .json(ChunkFormat::Packed(chunk))),
        Ok(None) => Err(("Chunk not found", StatusCode::NOT_FOUND).into()),
        Err(e) => {
            debug!(
                "Error retrieving chunk: ledger_id:{} chunk_offset: {} {}",
                path.ledger_id, path.ledger_offset, e
            );
            Err((
                format!("Error retrieving chunk: {e}"),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
                .into())
        }
    }
}

#[derive(Deserialize)]
pub struct DataCommitmentChunkApiPath {
    ledger_id: u32,
    data_commitment: String,
    offset: u32,
}

/// Parses the base58 text of a data commitment. A malformed one is the
/// client's error.
fn parse_data_commitment(text: &str) -> Result<DataCommitment, ApiError> {
    let invalid = |reason: String| -> ApiError {
        (
            format!("Invalid data commitment: {reason}"),
            StatusCode::BAD_REQUEST,
        )
            .into()
    };
    let bytes = text
        .from_base58()
        .map_err(|_| invalid("not base58".to_owned()))?;
    let hash: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| invalid(format!("expected 32 bytes, got {}", bytes.len())))?;
    Ok(DataCommitment(H256(hash)))
}

/// Chunk `offset` of the data with `data_commitment`.
///
/// Data is keyed by the data commitment, which fixes the tree root and the
/// data size. The lookup is a point lookup: the local placements of the
/// commitment, then the chunk cache row of the commitment.
pub async fn get_chunk_by_data_commitment_offset(
    state: web::Data<ApiState>,
    path: web::Path<DataCommitmentChunkApiPath>,
) -> Result<HttpResponse, ApiError> {
    let ledger = crate::routes::parse_ledger_id(path.ledger_id)?;
    let data_commitment = parse_data_commitment(&path.data_commitment)?;

    data_commitment_chunk_response(state.chunk_provider.get_chunk_by_commitment(
        ledger,
        data_commitment,
        path.offset.into(),
    ))
}

/// HTTP response for a data commitment chunk lookup.
fn data_commitment_chunk_response(
    lookup: eyre::Result<Option<ChunkFormat>>,
) -> Result<HttpResponse, ApiError> {
    match lookup {
        Ok(Some(chunk)) => Ok(HttpResponse::Ok()
            .content_type(ContentType::json())
            .json(chunk)),
        Ok(None) => Err(("Chunk not found", StatusCode::NOT_FOUND).into()),
        Err(e) => Err((
            format!("Error retrieving chunk: {e}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
            .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, ResponseError as _, test};
    use base58::ToBase58 as _;

    async fn commitment_status(uri: &str) -> StatusCode {
        async fn handler(
            path: web::Path<DataCommitmentChunkApiPath>,
        ) -> Result<HttpResponse, ApiError> {
            let data_commitment = parse_data_commitment(&path.data_commitment)?;
            // Only the known commitment has a chunk.
            let known = DataCommitment(H256::repeat_byte(1));
            data_commitment_chunk_response(Ok(
                (data_commitment == known).then(|| ChunkFormat::Packed(Default::default()))
            ))
        }
        let app = test::init_service(App::new().route(
            "/chunk/data-commitment/{ledger_id}/{data_commitment}/{offset}",
            web::get().to(handler),
        ))
        .await;
        test::call_service(&app, test::TestRequest::get().uri(uri).to_request())
            .await
            .status()
    }

    #[actix_web::test]
    async fn commitment_path_is_ok_for_a_known_commitment() {
        let known = H256::repeat_byte(1).0.to_base58();
        let uri = format!("/chunk/data-commitment/1/{known}/0");
        assert_eq!(commitment_status(&uri).await, StatusCode::OK);
    }

    #[actix_web::test]
    async fn unknown_commitment_is_not_found() {
        let unknown = H256::repeat_byte(2).0.to_base58();
        let uri = format!("/chunk/data-commitment/1/{unknown}/0");
        assert_eq!(commitment_status(&uri).await, StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn malformed_commitment_is_a_bad_request() {
        for malformed in ["not-base58-0OIl", "abc", ""] {
            let error = parse_data_commitment(malformed).expect_err("malformed commitment");
            assert_eq!(error.status_code(), StatusCode::BAD_REQUEST, "{malformed}");
        }
        assert_eq!(
            commitment_status("/chunk/data-commitment/1/abc/0").await,
            StatusCode::BAD_REQUEST
        );
    }

    #[actix_web::test]
    async fn storage_error_is_an_internal_error() {
        let error = data_commitment_chunk_response(Err(eyre::eyre!("disk"))).expect_err("error");
        assert_eq!(error.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
