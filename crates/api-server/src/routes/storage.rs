use crate::ApiState;
use crate::error::ApiError;
use actix_web::web::{Data, Json, Path};
use irys_domain::ChunkType;
use irys_types::DataLedger;
use serde::{Deserialize, Serialize};

/// Route path templates registered in [`crate::routes()`]. actix-web matches placeholders to
/// `Path`-extracted struct fields by their serde-visible names; a mismatch rejects every
/// request with a 404 ("missing field" in the body) before it reaches the handler. Keep the
/// params structs below free of `#[serde(rename_all)]` so these snake_case placeholders stay
/// correct — the camelCase JSON contract lives on the `*Response` structs.
pub const INTERVALS_ROUTE: &str = "/storage/intervals/{ledger}/{slot_index}/{chunk_type}";
pub const COUNTS_ROUTE: &str = "/storage/counts/{ledger}/{slot_index}";

#[derive(Serialize, Deserialize, Debug)]
pub struct StorageIntervalsParams {
    ledger: DataLedger,
    slot_index: usize,
    chunk_type: ChunkType,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ChunkInterval {
    start: u32,
    end: u32,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageIntervalsResponse {
    ledger: DataLedger,
    slot_index: usize,
    chunk_type: ChunkType,
    intervals: Vec<ChunkInterval>,
}

impl StorageIntervalsResponse {
    fn new(params: &StorageIntervalsParams, intervals: Vec<ChunkInterval>) -> Self {
        Self {
            ledger: params.ledger,
            slot_index: params.slot_index,
            chunk_type: params.chunk_type,
            intervals,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ChunkCountsParams {
    ledger: DataLedger,
    slot_index: usize,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ChunkCountsResponse {
    ledger: DataLedger,
    slot_index: usize,
    data_chunks: usize,
    packed_chunks: usize,
}

impl ChunkCountsResponse {
    fn new(params: &ChunkCountsParams, data_chunks: usize, packed_chunks: usize) -> Self {
        Self {
            ledger: params.ledger,
            slot_index: params.slot_index,
            data_chunks,
            packed_chunks,
        }
    }
}

pub async fn get_intervals(
    params: Path<StorageIntervalsParams>,
    app_state: Data<ApiState>,
) -> Result<Json<StorageIntervalsResponse>, ApiError> {
    let storage_modules = app_state.chunk_provider.storage_modules_guard.read();

    let intervals = storage_modules
        .iter()
        .find(|sm| {
            sm.partition_assignment()
                .map(|pa| {
                    pa.ledger_id == Some(params.ledger.into())
                        && pa.slot_index == Some(params.slot_index)
                })
                .unwrap_or(false)
        })
        .map(|sm| {
            sm.get_intervals(params.chunk_type)
                .into_iter()
                .map(|interval| ChunkInterval {
                    start: interval.start().into(),
                    end: interval.end().into(),
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(Json(StorageIntervalsResponse::new(&params, intervals)))
}

pub async fn get_chunk_counts(
    params: Path<ChunkCountsParams>,
    app_state: Data<ApiState>,
) -> Result<Json<ChunkCountsResponse>, ApiError> {
    let storage_modules = app_state.chunk_provider.storage_modules_guard.read();

    let storage_module = storage_modules.iter().find(|sm| {
        sm.partition_assignment()
            .map(|pa| {
                pa.ledger_id == Some(params.ledger.into())
                    && pa.slot_index == Some(params.slot_index)
            })
            .unwrap_or(false)
    });

    let (data_chunks, packed_chunks) = if let Some(sm) = storage_module {
        let data_intervals = sm.get_intervals(ChunkType::Data);
        let packed_intervals = sm.get_intervals(ChunkType::Entropy);

        let data_count: usize = data_intervals
            .iter()
            .map(|interval| {
                let start: u32 = interval.start().into();
                let end: u32 = interval.end().into();
                (end - start + 1) as usize
            })
            .sum();

        let packed_count: usize = packed_intervals
            .iter()
            .map(|interval| {
                let start: u32 = interval.start().into();
                let end: u32 = interval.end().into();
                (end - start + 1) as usize
            })
            .sum();

        (data_count, packed_count)
    } else {
        (0, 0)
    };

    Ok(Json(ChunkCountsResponse::new(
        &params,
        data_chunks,
        packed_chunks,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::dev::{Service as _, ServiceResponse};
    use actix_web::http::StatusCode;
    use actix_web::{App, test, web};

    // The real handlers need a full `ApiState`, which is too heavy to construct in a unit
    // test. Mounting the shared route templates with stub handlers that use the same
    // `Path` extractors still exercises what regressed: placeholder names resolving to the
    // serde-visible field names of the param structs.
    async fn echo_intervals(params: Path<StorageIntervalsParams>) -> Json<StorageIntervalsParams> {
        Json(params.into_inner())
    }

    async fn echo_counts(params: Path<ChunkCountsParams>) -> Json<ChunkCountsParams> {
        Json(params.into_inner())
    }

    async fn call(uri: &str) -> ServiceResponse {
        let app = test::init_service(
            App::new().service(
                web::scope(crate::API_VERSION)
                    .route(INTERVALS_ROUTE, web::get().to(echo_intervals))
                    .route(COUNTS_ROUTE, web::get().to(echo_counts)),
            ),
        )
        .await;
        let req = test::TestRequest::get().uri(uri).to_request();
        app.call(req).await.expect("route call failed")
    }

    async fn assert_extracted(uri: &str, expected: serde_json::Value) {
        let resp = call(uri).await;
        let status = resp.status();
        let body = test::read_body(resp).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "path extraction failed for {uri}: {}",
            String::from_utf8_lossy(&body)
        );
        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("JSON body");
        assert_eq!(parsed, expected, "extracted params drifted for {uri}");
    }

    #[actix_web::test]
    async fn data_intervals_path_reaches_handler() {
        assert_extracted(
            "/v1/storage/intervals/OneYear/0/Data",
            serde_json::json!({"ledger": "OneYear", "slot_index": 0, "chunk_type": "Data"}),
        )
        .await;
    }

    #[actix_web::test]
    async fn entropy_intervals_path_reaches_handler() {
        assert_extracted(
            "/v1/storage/intervals/Submit/3/Entropy",
            serde_json::json!({"ledger": "Submit", "slot_index": 3, "chunk_type": "Entropy"}),
        )
        .await;
    }

    #[actix_web::test]
    async fn counts_path_reaches_handler() {
        assert_extracted(
            "/v1/storage/counts/OneYear/7",
            serde_json::json!({"ledger": "OneYear", "slot_index": 7}),
        )
        .await;
    }

    /// Wire JSON for storage responses must stay camelCase even though path params
    /// are snake_case (no `rename_all` on the Path structs).
    #[actix_web::test]
    async fn intervals_response_serialises_camel_case_keys() {
        let params: StorageIntervalsParams = serde_json::from_value(serde_json::json!({
            "ledger": "OneYear",
            "slot_index": 2,
            "chunk_type": "Data",
        }))
        .expect("params");
        let intervals: Vec<ChunkInterval> = serde_json::from_value(serde_json::json!([
            {"start": 0, "end": 3}
        ]))
        .expect("intervals");
        let body = StorageIntervalsResponse::new(&params, intervals);
        let value = serde_json::to_value(&body).expect("serialise");
        let object = value.as_object().expect("object");
        assert!(
            object.contains_key("slotIndex") && !object.contains_key("slot_index"),
            "expected camelCase slotIndex, got keys {:?}",
            object.keys().collect::<Vec<_>>()
        );
        assert!(
            object.contains_key("chunkType") && !object.contains_key("chunk_type"),
            "expected camelCase chunkType, got keys {:?}",
            object.keys().collect::<Vec<_>>()
        );
        assert_eq!(object["slotIndex"], 2);
        assert_eq!(object["chunkType"], "Data");
        assert_eq!(object["intervals"][0]["start"], 0);
        assert_eq!(object["intervals"][0]["end"], 3);
    }

    #[actix_web::test]
    async fn counts_response_serialises_camel_case_keys() {
        let params: ChunkCountsParams = serde_json::from_value(serde_json::json!({
            "ledger": "Submit",
            "slot_index": 5,
        }))
        .expect("params");
        let body = ChunkCountsResponse::new(&params, 10, 20);
        let value = serde_json::to_value(&body).expect("serialise");
        let object = value.as_object().expect("object");
        let keys: Vec<&str> = object.keys().map(String::as_str).collect();
        for snake in ["slot_index", "data_chunks", "packed_chunks"] {
            assert!(
                !keys.contains(&snake),
                "response must not emit snake_case {snake}, got {keys:?}"
            );
        }
        assert_eq!(object["slotIndex"], 5);
        assert_eq!(object["dataChunks"], 10);
        assert_eq!(object["packedChunks"], 20);
        assert_eq!(object["ledger"], "Submit");
    }
}
