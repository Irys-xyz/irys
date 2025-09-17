use crate::error::ApiError;
use crate::ApiState;
use actix_web::web::{Data, Json, Path};
use irys_types::{parse_address, partition::PartitionAssignment, DataLedger};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct LedgerSummary {
    node_id: String,
    ledger_type: DataLedger,
    assignment_count: usize,
}

fn get_canonical_epoch_snapshot(
    app_state: &Data<ApiState>,
) -> std::sync::Arc<irys_domain::EpochSnapshot> {
    app_state.block_tree.read().canonical_epoch_snapshot()
}

fn get_assignments_by_ledger_type(
    node_id: &str,
    partition_assignments: Vec<PartitionAssignment>,
    ledger_type: DataLedger,
) -> Result<Vec<PartitionAssignment>, ApiError> {
    let assignments: Vec<PartitionAssignment> = partition_assignments
        .into_iter()
        .filter(|pa| pa.ledger_id == Some(ledger_type as u32))
        .collect();

    if assignments.is_empty() {
        return Err(ApiError::LedgerNotFound {
            node_id: node_id.to_string(),
            ledger_type,
        });
    }

    Ok(assignments)
}

#[expect(clippy::unused_async)]
async fn get_ledger_summary(
    node_id: Path<String>,
    app_state: Data<ApiState>,
    ledger_type: DataLedger,
) -> Result<Json<LedgerSummary>, ApiError> {
    // Get the current epoch snapshot
    let epoch_snapshot = get_canonical_epoch_snapshot(&app_state);

    // Get partition assignments
    let node_address = parse_address(node_id.as_str())?;
    let partition_assignments = epoch_snapshot.get_partition_assignments(node_address);

    let submit_assignments: Vec<_> =
        get_assignments_by_ledger_type(&node_id, partition_assignments, ledger_type)?;

    let assignment_count = submit_assignments.len();

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

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::partition::PartitionAssignment;
    use rstest::*;

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
                ledger_id: Some(1), // Submit ledger
                slot_index: Some(0),
                ..Default::default()
            },
            PartitionAssignment {
                ledger_id: Some(1), // Submit ledger
                slot_index: Some(1),
                ..Default::default()
            },
        ]
    }

    #[fixture]
    fn sample_publish_assignments() -> Vec<PartitionAssignment> {
        vec![PartitionAssignment {
            ledger_id: Some(0), // Publish ledger
            slot_index: Some(0),
            ..Default::default()
        }]
    }

    #[fixture]
    fn mixed_assignments() -> Vec<PartitionAssignment> {
        vec![
            PartitionAssignment {
                ledger_id: Some(0), // Publish ledger
                slot_index: Some(0),
                ..Default::default()
            },
            PartitionAssignment {
                ledger_id: Some(1), // Submit ledger
                slot_index: Some(1),
                ..Default::default()
            },
            PartitionAssignment {
                ledger_id: Some(1), // Submit ledger
                slot_index: Some(2),
                ..Default::default()
            },
        ]
    }

    #[fixture]
    fn empty_assignments() -> Vec<PartitionAssignment> {
        vec![]
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
                assert_eq!(assignment.ledger_id, Some(ledger_type as u32));
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

            // Verify all submit assignments have ledger_id = 1
            for assignment in submit_assignments {
                assert_eq!(assignment.ledger_id, Some(1));
            }

            // Verify all publish assignments have ledger_id = 0
            for assignment in publish_assignments {
                assert_eq!(assignment.ledger_id, Some(0));
            }
        }
    }


}
