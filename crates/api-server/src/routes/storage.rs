use crate::ApiState;
use crate::error::ApiError;
use actix_web::web::{Data, Json, Path};
use awc::http::StatusCode;
use irys_domain::{ChunkType, StorageModule};
use irys_types::{DataLedger, H256};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Route path templates registered in [`crate::routes()`]. actix-web matches placeholders to
/// `Path`-extracted struct fields by their serde-visible names; a mismatch rejects every
/// request with a 404 ("missing field" in the body) before it reaches the handler. Keep the
/// params structs below free of `#[serde(rename_all)]` so these snake_case placeholders stay
/// correct — the camelCase JSON contract lives on the `*Response` structs.
pub const INTERVALS_ROUTE: &str = "/storage/intervals/{ledger}/{slot_index}/{chunk_type}";
pub const COUNTS_ROUTE: &str = "/storage/counts/{ledger}/{slot_index}";
pub const PARTITION_INTERVALS_ROUTE: &str =
    "/storage/partition/{partition_hash}/intervals/{chunk_type}";

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

/// Maps a storage module's coalesced interval set for one [`ChunkType`] to
/// the wire shape. Bounds are partition-relative chunk offsets with an
/// INCLUSIVE `end` (a fully covered 10-chunk partition reads `[0, 9]`);
/// touching/overlapping runs arrive already merged and sorted from
/// [`StorageModule::get_intervals`]. Both interval endpoints go through
/// here so their semantics cannot drift apart.
fn module_intervals(sm: &StorageModule, chunk_type: ChunkType) -> Vec<ChunkInterval> {
    sm.get_intervals(chunk_type)
        .into_iter()
        .map(|interval| ChunkInterval {
            start: interval.start().into(),
            end: interval.end().into(),
        })
        .collect()
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
        .map(|sm| module_intervals(sm, params.chunk_type))
        .unwrap_or_default();

    Ok(Json(StorageIntervalsResponse::new(&params, intervals)))
}

#[derive(Serialize, Deserialize, Debug)]
pub struct PartitionIntervalsParams {
    // Kept as a String so a malformed hash maps to an explicit 400 instead
    // of the extractor's opaque 404
    partition_hash: String,
    chunk_type: ChunkType,
}

/// Response for `/storage/partition/{partition_hash}/intervals/{chunk_type}`:
/// the intervals this node has locally recorded for the partition. Works for
/// any locally hosted partition — including capacity partitions, which have
/// no ledger/slot coordinates and so cannot be addressed by
/// [`INTERVALS_ROUTE`].
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PartitionIntervalsResponse {
    partition_hash: H256,
    chunk_type: ChunkType,
    intervals: Vec<ChunkInterval>,
}

fn parse_partition_hash(raw: &str) -> Result<H256, ApiError> {
    H256::from_base58_result(raw).map_err(|e| {
        ApiError::CustomWithStatus(
            format!("Invalid partition hash '{raw}': {e}"),
            StatusCode::BAD_REQUEST,
        )
    })
}

/// Resolves the one local storage module hosting `partition_hash` from the
/// node's authoritative in-memory module registry.
///
/// `Ok(None)` means the partition is not hosted by this node. More than one
/// module claiming the same hash is a storage-registry invariant violation
/// and surfaces as an error rather than silently answering from an
/// arbitrary module.
fn find_module_by_partition_hash(
    modules: &[Arc<StorageModule>],
    partition_hash: H256,
) -> Result<Option<Arc<StorageModule>>, ApiError> {
    let mut matches = modules.iter().filter(|sm| {
        sm.partition_assignment()
            .is_some_and(|pa| pa.partition_hash == partition_hash)
    });
    let first = matches.next().cloned();
    let extra = matches.count();
    if extra > 0 {
        return Err(ApiError::CustomWithStatus(
            format!(
                "{} local storage modules claim partition hash {partition_hash}; \
                 the storage-module registry violates its one-module-per-partition invariant",
                extra + 1
            ),
            StatusCode::INTERNAL_SERVER_ERROR,
        ));
    }
    Ok(first)
}

pub async fn get_partition_intervals(
    params: Path<PartitionIntervalsParams>,
    app_state: Data<ApiState>,
) -> Result<Json<PartitionIntervalsResponse>, ApiError> {
    let partition_hash = parse_partition_hash(&params.partition_hash)?;

    // Clone the module Arc out of the registry lock; the interval read below
    // only takes the module's own intervals read lock
    let module = {
        let storage_modules = app_state.chunk_provider.storage_modules_guard.read();
        find_module_by_partition_hash(&storage_modules, partition_hash)?
    };
    let Some(module) = module else {
        return Err(ApiError::CustomWithStatus(
            format!("Partition {partition_hash} is not hosted by this node"),
            StatusCode::NOT_FOUND,
        ));
    };

    Ok(Json(PartitionIntervalsResponse {
        partition_hash,
        chunk_type: params.chunk_type,
        intervals: module_intervals(&module, params.chunk_type),
    }))
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

    async fn echo_partition_intervals(
        params: Path<PartitionIntervalsParams>,
    ) -> Json<PartitionIntervalsParams> {
        Json(params.into_inner())
    }

    async fn call(uri: &str) -> ServiceResponse {
        let app = test::init_service(
            App::new().service(
                web::scope(crate::API_VERSION)
                    .route(INTERVALS_ROUTE, web::get().to(echo_intervals))
                    .route(COUNTS_ROUTE, web::get().to(echo_counts))
                    .route(
                        PARTITION_INTERVALS_ROUTE,
                        web::get().to(echo_partition_intervals),
                    ),
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

    // === Partition-hash-keyed intervals ===

    use actix_web::ResponseError as _;
    use irys_domain::StorageModuleInfo;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{
        Config, ConsensusConfig, ConsensusOptions, NodeConfig, PartitionChunkOffset, ii,
        partition::PartitionAssignment, partition_chunk_offset_ii,
    };

    const PARTITION_CHUNKS: u64 = 20;

    fn storage_test_config(base_path: std::path::PathBuf) -> Config {
        let mut node_config = NodeConfig::testing();
        node_config.consensus = ConsensusOptions::Custom(ConsensusConfig {
            chunk_size: 32,
            num_chunks_in_partition: PARTITION_CHUNKS,
            entropy_packing_iterations: 1,
            ..node_config.consensus_config()
        });
        node_config.base_directory = base_path;
        Config::new_with_random_peer_id(node_config)
    }

    fn capacity_assignment_for(config: &Config, partition_hash: H256) -> PartitionAssignment {
        PartitionAssignment {
            partition_hash,
            miner_address: config.node_config.miner_address(),
            ledger_id: None,
            slot_index: None,
        }
    }

    fn module_with_assignment(
        id: usize,
        config: &Config,
        assignment: PartitionAssignment,
    ) -> Arc<StorageModule> {
        let info = StorageModuleInfo {
            id,
            partition_assignment: Some(assignment),
            submodules: vec![(
                partition_chunk_offset_ii!(0, PARTITION_CHUNKS as u32 - 1),
                format!("submodule_{id}").into(),
            )],
        };
        Arc::new(StorageModule::new(&info, config).expect("test storage module"))
    }

    #[actix_web::test]
    async fn partition_intervals_path_reaches_handler() {
        assert_extracted(
            "/v1/storage/partition/2fu2ar8SLem6ykDT8k7pjWM74qxiFEr4X2vBBjJHnLR/intervals/Entropy",
            serde_json::json!({
                "partition_hash": "2fu2ar8SLem6ykDT8k7pjWM74qxiFEr4X2vBBjJHnLR",
                "chunk_type": "Entropy",
            }),
        )
        .await;
    }

    #[actix_web::test]
    async fn parse_partition_hash_maps_malformed_input_to_bad_request() {
        // Invalid base58 alphabet, valid base58 of the wrong length, empty
        for raw in ["!!!not-base58!!!", "abc", ""] {
            let err = parse_partition_hash(raw).expect_err("must reject malformed hash");
            assert_eq!(
                err.status_code(),
                StatusCode::BAD_REQUEST,
                "wrong status for input {raw:?}"
            );
        }
        // The API's own base58 rendering round-trips
        let hash = H256::random();
        assert_eq!(parse_partition_hash(&hash.to_string()).unwrap(), hash);
    }

    /// Full lifecycle against a real capacity-assigned storage module (no
    /// ledger id, no slot index): resolvable by hash, empty before packing,
    /// real coalesced intervals when partially packed, full inclusive range
    /// when fully packed.
    #[actix_web::test]
    async fn capacity_partition_entropy_intervals_lifecycle() {
        let tmp_dir = TempDirBuilder::new()
            .prefix("partition_intervals_api")
            .build();
        let config = storage_test_config(tmp_dir.path().to_path_buf());
        let capacity_hash = H256::random();
        let module =
            module_with_assignment(0, &config, capacity_assignment_for(&config, capacity_hash));
        let modules = vec![module];

        // Capacity assignments carry no ledger/slot coordinates yet still
        // resolve by hash
        let found = find_module_by_partition_hash(&modules, capacity_hash)
            .expect("single module cannot violate the registry invariant")
            .expect("locally hosted capacity partition must resolve");
        let assignment = found.partition_assignment().expect("assignment");
        assert!(assignment.ledger_id.is_none() && assignment.slot_index.is_none());

        // Known but unpacked: empty interval list, not an error
        assert!(module_intervals(&found, ChunkType::Entropy).is_empty());

        // Partially packed, written out of order: intervals come back sorted
        // by start with touching chunks coalesced and inclusive ends
        for offset in [5_u32, 2, 0, 1] {
            found.write_chunk(
                PartitionChunkOffset::from(offset),
                vec![0xff; 32],
                ChunkType::Entropy,
            );
        }
        let partial: Vec<(u32, u32)> = module_intervals(&found, ChunkType::Entropy)
            .iter()
            .map(|i| (i.start, i.end))
            .collect();
        assert_eq!(partial, vec![(0, 2), (5, 5)]);

        // Fully packed: one interval spanning the whole partition,
        // 0 ..= num_chunks_in_partition - 1
        found.pack_with_zeros();
        let full: Vec<(u32, u32)> = module_intervals(&found, ChunkType::Entropy)
            .iter()
            .map(|i| (i.start, i.end))
            .collect();
        assert_eq!(full, vec![(0, PARTITION_CHUNKS as u32 - 1)]);

        // A valid hash this node does not host resolves to None (→ 404)
        assert!(
            find_module_by_partition_hash(&modules, H256::random())
                .unwrap()
                .is_none()
        );
    }

    #[actix_web::test]
    async fn duplicate_partition_hash_is_an_invariant_violation() {
        let tmp_dir = TempDirBuilder::new()
            .prefix("partition_intervals_dup")
            .build();
        let config = storage_test_config(tmp_dir.path().to_path_buf());
        let hash = H256::random();
        let modules = vec![
            module_with_assignment(0, &config, capacity_assignment_for(&config, hash)),
            module_with_assignment(1, &config, capacity_assignment_for(&config, hash)),
        ];

        let err = find_module_by_partition_hash(&modules, hash)
            .expect_err("duplicate hash claims must not silently pick a module");
        assert_eq!(err.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
        let message = err.to_string();
        assert!(
            message.contains("2 local storage modules") && message.contains("invariant"),
            "unexpected error message: {message}"
        );
    }

    #[actix_web::test]
    async fn partition_intervals_response_serialises_camel_case_keys() {
        let empty = PartitionIntervalsResponse {
            partition_hash: H256::from([7; 32]),
            chunk_type: ChunkType::Entropy,
            intervals: vec![],
        };
        let value = serde_json::to_value(&empty).expect("serialise");
        let object = value.as_object().expect("object");
        assert!(
            object.contains_key("partitionHash") && !object.contains_key("partition_hash"),
            "expected camelCase partitionHash, got keys {:?}",
            object.keys().collect::<Vec<_>>()
        );
        assert_eq!(object["partitionHash"], H256::from([7; 32]).to_string());
        assert_eq!(object["chunkType"], "Entropy");
        // Known-but-unpacked serialises an explicit empty array
        assert_eq!(object["intervals"], serde_json::json!([]));

        // Same ChunkInterval wire shape as the ledger/slot endpoint,
        // inclusive end included
        let full = PartitionIntervalsResponse {
            partition_hash: H256::from([7; 32]),
            chunk_type: ChunkType::Entropy,
            intervals: vec![ChunkInterval {
                start: 0,
                end: 2138,
            }],
        };
        let value = serde_json::to_value(&full).expect("serialise");
        assert_eq!(value["intervals"][0]["start"], 0);
        assert_eq!(value["intervals"][0]["end"], 2138);
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
