use crate::error::ApiError;
use crate::ApiState;
use actix_web::web::{Data, Json, Path};
use irys_domain::{get_canonical_chain, ChunkType};
use irys_types::{parse_address, partition::PartitionAssignment, DataLedger};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct LedgerSummary {
    node_id: String,
    ledger_type: DataLedger,
    assignment_count: usize,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ChainHeight {
    height: u64,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct StorageIntervalsParams {
    ledger: DataLedger,
    slot_index: usize,
    chunk_type: ChunkType,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ChunkInterval {
    start: u32,
    end: u32,
}

#[derive(Serialize, Deserialize, Debug)]
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

fn get_canonical_epoch_snapshot(
    app_state: &Data<ApiState>,
) -> std::sync::Arc<irys_domain::EpochSnapshot> {
    app_state.block_tree.read().canonical_epoch_snapshot()
}

fn count_assignments_by_ledger_type(
    node_id: &str,
    partition_assignments: &[PartitionAssignment],
    ledger_type: DataLedger,
) -> Result<usize, ApiError> {
    let count = partition_assignments
        .iter()
        .filter(|pa| pa.ledger_id == Some(ledger_type.into()))
        .count();

    if count == 0 {
        return Err(ApiError::LedgerNotFound {
            node_id: node_id.to_string(),
            ledger_type,
        });
    }

    Ok(count)
}

#[expect(clippy::unused_async)]
async fn get_ledger_summary(
    node_id: Path<String>,
    app_state: Data<ApiState>,
    ledger_type: DataLedger,
) -> Result<Json<LedgerSummary>, ApiError> {
    // Parse address before acquiring lock
    let node_address = parse_address(node_id.as_str())?;

    // Minimize lock scope - get partition assignments and release lock immediately
    let partition_assignments = {
        let epoch_snapshot = get_canonical_epoch_snapshot(&app_state);
        epoch_snapshot.get_partition_assignments(node_address)
    }; // Lock released here

    let assignment_count =
        count_assignments_by_ledger_type(&node_id, &partition_assignments, ledger_type)?;

    Ok(Json(LedgerSummary {
        node_id: node_id.to_string(),
        ledger_type,
        assignment_count,
    }))
}

pub async fn get_submit_ledger_summary(
    node_id: Path<String>,
    app_state: Data<ApiState>,
) -> Result<Json<LedgerSummary>, ApiError> {
    get_ledger_summary(node_id, app_state, DataLedger::Submit).await
}

pub async fn get_publish_ledger_summary(
    node_id: Path<String>,
    app_state: Data<ApiState>,
) -> Result<Json<LedgerSummary>, ApiError> {
    get_ledger_summary(node_id, app_state, DataLedger::Publish).await
}

pub async fn get_chain_height(app_state: Data<ApiState>) -> Result<Json<ChainHeight>, ApiError> {
    // Clone is necessary as get_canonical_chain moves the guard into spawn_blocking
    let canonical_chain = get_canonical_chain(app_state.block_tree.clone())
        .await
        .map_err(ApiError::canonical_chain_error)?;

    let height = canonical_chain
        .0
        .last()
        .map(|block| block.height)
        .ok_or(ApiError::EmptyCanonicalChain)?;

    Ok(Json(ChainHeight { height }))
}

pub async fn get_storage_intervals(
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

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::partition::PartitionAssignment;
    use rstest::*;

    fn get_assignments_by_ledger_type(
        node_id: &str,
        partition_assignments: Vec<PartitionAssignment>,
        ledger_type: DataLedger,
    ) -> Result<Vec<PartitionAssignment>, ApiError> {
        let mut assignments = partition_assignments
            .into_iter()
            .filter(|pa| pa.ledger_id == Some(ledger_type.into()))
            .peekable();

        // Check if empty without consuming the iterator
        if assignments.peek().is_none() {
            return Err(ApiError::LedgerNotFound {
                node_id: node_id.to_string(),
                ledger_type,
            });
        }

        Ok(assignments.collect())
    }
    // Test data fixtures
    #[fixture]
    fn valid_node_id() -> String {
        "0x1234567890abcdef1234567890abcdef12345678".to_string()
    }

    #[fixture]
    fn invalid_node_id() -> String {
        "invalid_address".to_string()
    }

    #[fixture]
    fn sample_submit_assignments() -> Vec<PartitionAssignment> {
        vec![
            PartitionAssignment {
                ledger_id: Some(DataLedger::Submit.into()),
                slot_index: Some(0),
                ..Default::default()
            },
            PartitionAssignment {
                ledger_id: Some(DataLedger::Submit.into()),
                slot_index: Some(1),
                ..Default::default()
            },
        ]
    }

    #[fixture]
    fn sample_publish_assignments() -> Vec<PartitionAssignment> {
        vec![PartitionAssignment {
            ledger_id: Some(DataLedger::Publish.into()),
            slot_index: Some(0),
            ..Default::default()
        }]
    }

    #[fixture]
    fn mixed_assignments() -> Vec<PartitionAssignment> {
        vec![
            PartitionAssignment {
                ledger_id: Some(DataLedger::Publish.into()),
                slot_index: Some(0),
                ..Default::default()
            },
            PartitionAssignment {
                ledger_id: Some(DataLedger::Submit.into()),
                slot_index: Some(1),
                ..Default::default()
            },
            PartitionAssignment {
                ledger_id: Some(DataLedger::Submit.into()),
                slot_index: Some(2),
                ..Default::default()
            },
        ]
    }

    #[fixture]
    fn empty_assignments() -> Vec<PartitionAssignment> {
        vec![]
    }

    mod storage_intervals_tests {
        use super::*;

        #[test]
        fn should_serialize_storage_intervals_response_correctly() {
            let params = StorageIntervalsParams {
                ledger: DataLedger::Submit,
                slot_index: 5,
                chunk_type: ChunkType::Data,
            };
            let intervals = vec![
                ChunkInterval { start: 0, end: 10 },
                ChunkInterval { start: 20, end: 30 },
            ];
            let response = StorageIntervalsResponse::new(&params, intervals);

            let serialized = serde_json::to_string(&response).unwrap();
            // Check the actual serialization
            assert!(serialized.contains("\"ledger\":\"Submit\""));
            assert!(serialized.contains("\"slot_index\":5"));
            assert!(serialized.contains("\"chunk_type\":\"Data\""));
            assert!(serialized.contains("\"start\":0"));
            assert!(serialized.contains("\"end\":10"));
        }
    }

    mod get_assignments_by_ledger_type_tests {
        use super::*;

        #[rstest]
        #[case::submit_ledger_found(DataLedger::Submit, 2)]
        #[case::publish_ledger_found(DataLedger::Publish, 1)]
        fn should_return_assignments_when_found(
            mixed_assignments: Vec<PartitionAssignment>,
            #[case] ledger_type: DataLedger,
            #[case] expected_count: usize,
        ) {
            let result =
                get_assignments_by_ledger_type("test_node", mixed_assignments, ledger_type);

            assert!(result.is_ok());
            let assignments = result.unwrap();
            assert_eq!(assignments.len(), expected_count);
            // Verify all assignments have the correct ledger_id
            for assignment in assignments {
                assert_eq!(assignment.ledger_id, Some(ledger_type.into()));
            }
        }

        #[rstest]
        #[case::submit_not_found(DataLedger::Submit)]
        #[case::publish_not_found(DataLedger::Publish)]
        fn should_return_error_when_no_assignments_found(
            empty_assignments: Vec<PartitionAssignment>,
            #[case] ledger_type: DataLedger,
        ) {
            let node_id = "test_node";
            let result = get_assignments_by_ledger_type(node_id, empty_assignments, ledger_type);

            assert!(result.is_err());
            let error = result.unwrap_err();
            match error {
                ApiError::LedgerNotFound {
                    node_id: error_node_id,
                    ledger_type: error_ledger_type,
                } => {
                    assert_eq!(error_node_id, node_id);
                    assert_eq!(error_ledger_type, ledger_type);
                }
                _ => panic!("Expected LedgerNotFound error"),
            }
        }

        #[rstest]
        fn should_return_error_when_wrong_ledger_type_requested(
            sample_submit_assignments: Vec<PartitionAssignment>,
        ) {
            let node_id = "test_node";
            let result = get_assignments_by_ledger_type(
                node_id,
                sample_submit_assignments,
                DataLedger::Publish, // Looking for publish in submit assignments
            );

            assert!(result.is_err());
            let error = result.unwrap_err();
            match error {
                ApiError::LedgerNotFound { .. } => {}
                _ => panic!("Expected LedgerNotFound error"),
            }
        }

        #[rstest]
        fn should_filter_correctly_by_ledger_id(mixed_assignments: Vec<PartitionAssignment>) {
            let submit_result = get_assignments_by_ledger_type(
                "test_node",
                mixed_assignments.clone(),
                DataLedger::Submit,
            );
            let publish_result =
                get_assignments_by_ledger_type("test_node", mixed_assignments, DataLedger::Publish);

            assert!(submit_result.is_ok());
            assert!(publish_result.is_ok());

            let submit_assignments = submit_result.unwrap();
            let publish_assignments = publish_result.unwrap();

            assert_eq!(submit_assignments.len(), 2);
            assert_eq!(publish_assignments.len(), 1);

            // Verify all submit assignments have ledger_id = Submit
            for assignment in submit_assignments {
                assert_eq!(assignment.ledger_id, Some(DataLedger::Submit.into()));
            }

            // Verify all publish assignments have ledger_id = Publish
            for assignment in publish_assignments {
                assert_eq!(assignment.ledger_id, Some(DataLedger::Publish.into()));
            }
        }
    }
}
