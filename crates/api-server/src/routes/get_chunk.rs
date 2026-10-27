use crate::{ApiState, error::ApiError};
use actix_web::{
    HttpResponse,
    http::header::ContentType,
    web::{self},
};

use awc::http::StatusCode;
use irys_domain::AmbiguousDataSize;
use irys_types::{ChunkFormat, DataTreeRoot};
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
pub struct DataRootChunkApiPath {
    ledger_id: u32,
    data_root: DataTreeRoot,
    offset: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataRootChunkApiQuery {
    /// The data size of the tx. With it, the lookup is a point lookup of the
    /// commitment of `(data_root, data_size)`.
    data_size: Option<u64>,
}

/// Chunk `offset` of the data whose tree root is `data_root`.
///
/// Data is keyed by the data commitment, and the root alone does not fix the
/// size. With `?dataSize=`, the lookup serves that size only. Without it, the
/// lookup scans the local placements and cache rows of this root. If chunks of
/// more than one size hold the offset, it returns 400 and asks for `dataSize`.
pub async fn get_chunk_by_data_root_offset(
    state: web::Data<ApiState>,
    path: web::Path<DataRootChunkApiPath>,
    query: web::Query<DataRootChunkApiQuery>,
) -> Result<HttpResponse, ApiError> {
    let ledger = crate::routes::parse_ledger_id(path.ledger_id)?;

    data_root_chunk_response(state.chunk_provider.get_chunk_by_data_root(
        ledger,
        path.data_root,
        query.data_size,
        path.offset.into(),
    ))
}

/// HTTP response for a data-root chunk lookup. An ambiguous root-only lookup
/// is the client's error: it must name the data size.
fn data_root_chunk_response(
    lookup: eyre::Result<Option<ChunkFormat>>,
) -> Result<HttpResponse, ApiError> {
    match lookup {
        Ok(Some(chunk)) => Ok(HttpResponse::Ok()
            .content_type(ContentType::json())
            .json(chunk)),
        Ok(None) => Err(("Chunk not found", StatusCode::NOT_FOUND).into()),
        Err(e) if e.downcast_ref::<AmbiguousDataSize>().is_some() => {
            Err((e.to_string(), StatusCode::BAD_REQUEST).into())
        }
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
    use actix_web::dev::Service as _;
    use actix_web::{App, ResponseError as _, test};
    use irys_types::{H256, TxChunkOffset};

    async fn echo_query(query: web::Query<DataRootChunkApiQuery>) -> HttpResponse {
        HttpResponse::Ok().json(query.data_size)
    }

    async fn extracted_size(uri: &str) -> Option<u64> {
        let app = test::init_service(App::new().route("/chunk", web::get().to(echo_query))).await;
        let response = app
            .call(test::TestRequest::get().uri(uri).to_request())
            .await
            .expect("route call");
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        serde_json::from_slice(&test::read_body(response).await).expect("JSON body")
    }

    #[actix_web::test]
    async fn data_size_query_is_optional_and_camel_case() {
        assert_eq!(extracted_size("/chunk").await, None);
        assert_eq!(extracted_size("/chunk?dataSize=96").await, Some(96));
    }

    #[actix_web::test]
    async fn ambiguous_data_size_is_a_bad_request_that_asks_for_the_size() {
        let ambiguous = AmbiguousDataSize {
            data_tree_root: DataTreeRoot(H256::repeat_byte(1)),
            tx_offset: TxChunkOffset::from(1_u32),
            data_sizes: vec![64, 96],
        };
        let error = data_root_chunk_response(Err(ambiguous.into())).expect_err("error response");
        assert_eq!(error.status_code(), StatusCode::BAD_REQUEST);
        assert!(error.to_string().contains("dataSize"), "{error}");

        let error = data_root_chunk_response(Err(eyre::eyre!("disk"))).expect_err("error response");
        assert_eq!(error.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
        let error = data_root_chunk_response(Ok(None)).expect_err("error response");
        assert_eq!(error.status_code(), StatusCode::NOT_FOUND);
    }
}
