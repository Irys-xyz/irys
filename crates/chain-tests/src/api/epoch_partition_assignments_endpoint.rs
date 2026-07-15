//! Integration tests for `GET /v1/epoch/current/partition-assignments`.

use crate::utils::IrysNodeTest;
use actix_web::body::MessageBody;
use actix_web::dev::{Service, ServiceResponse};
use actix_web::http::StatusCode;
use actix_web::test::{TestRequest, call_service, read_body_json};
use irys_config::submodules::StorageSubmodulesConfig;
use irys_domain::EpochSnapshot;
use irys_types::{
    DataLedger, H256, NodeConfig, UnixTimestamp, hardfork_config::Cascade,
    partition::PartitionAssignment,
};

async fn get_json<T, B>(app: &T, uri: &str) -> (StatusCode, serde_json::Value)
where
    T: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: MessageBody,
{
    let req = TestRequest::get().uri(uri).to_request();
    let resp = call_service(app, req).await;
    let status = resp.status();
    assert_eq!(status, StatusCode::OK, "GET {uri} should succeed");
    (status, read_body_json(resp).await)
}

fn assignment_rows(body: &serde_json::Value) -> Vec<PartitionAssignment> {
    serde_json::from_value(body["assignments"].clone())
        .expect("assignments should deserialize as PartitionAssignment rows")
}

/// GETs the global roster and asserts it mirrors `snapshot` exactly:
/// string epoch metadata, data assignments, capacity assignments, and
/// unassigned hashes (sorted). Returns the data + capacity rows.
async fn assert_roster_mirrors_snapshot<T, B>(
    app: &T,
    snapshot: &EpochSnapshot,
) -> (Vec<PartitionAssignment>, Vec<PartitionAssignment>)
where
    T: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: MessageBody,
{
    let (_, body) = get_json(app, "/v1/epoch/current/partition-assignments").await;

    assert_eq!(
        body["epochHeight"].as_str(),
        Some(snapshot.epoch_height.to_string().as_str()),
        "epochHeight must be a string matching the canonical snapshot"
    );
    assert_eq!(
        body["epochBlockHeight"].as_str(),
        Some(snapshot.epoch_block.height.to_string().as_str()),
        "epochBlockHeight must be a string matching the canonical snapshot"
    );

    let rows = assignment_rows(&body);
    let expected: Vec<PartitionAssignment> = snapshot
        .partition_assignments
        .data_partitions
        .values()
        .copied()
        .collect();
    assert_eq!(
        rows, expected,
        "global roster must mirror the canonical snapshot's data partitions"
    );

    let capacity_rows: Vec<PartitionAssignment> =
        serde_json::from_value(body["capacityAssignments"].clone())
            .expect("capacityAssignments should deserialize as PartitionAssignment rows");
    let expected_capacity: Vec<PartitionAssignment> = snapshot
        .partition_assignments
        .capacity_partitions
        .values()
        .copied()
        .collect();
    assert_eq!(
        capacity_rows, expected_capacity,
        "capacity roster must mirror the canonical snapshot's capacity partitions"
    );

    let unassigned: Vec<H256> = serde_json::from_value(body["unassignedPartitions"].clone())
        .expect("unassignedPartitions should deserialize as hashes");
    let mut expected_unassigned = snapshot.unassigned_partitions.clone();
    expected_unassigned.sort_unstable();
    assert_eq!(
        unassigned, expected_unassigned,
        "unassigned roster must mirror the canonical snapshot's unassigned partitions"
    );

    (rows, capacity_rows)
}

/// Cascade active from genesis: all four data ledgers in the global roster;
/// capacity separate; per-miner views keep their existing contracts; the
/// roster follows the canonical snapshot across an epoch boundary.
#[test_log::test(tokio::test)]
async fn heavy_epoch_partition_assignments_endpoint() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2_u64;
    let mut config = NodeConfig::testing().with_consensus(|c| {
        c.block_migration_depth = 1;
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        c.chunk_size = 32;
        c.num_chunks_in_partition = 10;
        c.entropy_packing_iterations = 1_000;
        c.hardforks.cascade = Some(Cascade {
            activation_timestamp: UnixTimestamp::from_secs(0),
            one_year_epoch_length: 8,
            thirty_day_epoch_length: 2,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
    });
    let signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&signer]);

    // 5 SMs: enough for all 4 data ledger slots + ≥1 capacity leftover.
    let test_node = IrysNodeTest::new_genesis(config);
    StorageSubmodulesConfig::load_for_test(test_node.cfg.base_directory.clone(), 5)?;
    let node = test_node.start_and_wait_for_packing("test", 30).await;
    let app = node.start_public_api().await;
    let miner = node.node_ctx.config.node_config.miner_address();

    // Cascade activates at timestamp 0, i.e. from genesis: all four data
    // ledgers already have partitions assigned in the genesis epoch snapshot.
    let all_data_ledgers = [
        DataLedger::Publish,
        DataLedger::Submit,
        DataLedger::OneYear,
        DataLedger::ThirtyDay,
    ];
    let snapshot = node.get_canonical_epoch_snapshot();
    for ledger in all_data_ledgers {
        assert!(
            snapshot
                .partition_assignments
                .data_partitions
                .values()
                .any(|a| a.ledger_id == Some(ledger.get_id())),
            "genesis snapshot missing an assignment for {ledger:?} (id {})",
            ledger.get_id()
        );
    }

    let (rows, capacity_rows) = assert_roster_mirrors_snapshot(&app, &snapshot).await;
    assert!(
        rows.windows(2)
            .all(|pair| pair[0].partition_hash < pair[1].partition_hash),
        "roster must be strictly ordered by partition hash"
    );

    for ledger in all_data_ledgers {
        assert!(
            rows.iter().any(|a| a.ledger_id == Some(ledger.get_id())),
            "roster missing an assignment for {ledger:?} (id {})",
            ledger.get_id()
        );
    }
    for row in &rows {
        assert_eq!(row.miner_address, miner);
        assert!(
            row.ledger_id.is_some() && row.slot_index.is_some(),
            "data partition rows must carry a ledger id and slot index"
        );
    }

    let capacity = &snapshot.partition_assignments.capacity_partitions;
    assert!(
        !capacity.is_empty(),
        "test setup should leave at least one capacity partition"
    );
    for capacity_hash in capacity.keys() {
        assert!(
            !rows.iter().any(|a| a.partition_hash == *capacity_hash),
            "capacity partition {capacity_hash} must not appear in the roster"
        );
    }
    for capacity_row in &capacity_rows {
        assert_eq!(capacity_row.miner_address, miner);
        assert!(
            capacity_row.ledger_id.is_none() && capacity_row.slot_index.is_none(),
            "capacity rows must not claim a ledger slot"
        );
    }

    // Per-miner generic view: all data ledgers + capacity.
    let (_, miner_body) = get_json(&app, &format!("/v1/ledger/{miner}/assignments")).await;
    let miner_rows = assignment_rows(&miner_body);
    let miner_expected = snapshot.get_partition_assignments(miner);
    assert_eq!(
        miner_rows, miner_expected,
        "generic per-miner endpoint must return the miner's data + capacity assignments"
    );
    for ledger in [DataLedger::OneYear, DataLedger::ThirtyDay] {
        assert!(
            miner_rows
                .iter()
                .any(|a| a.ledger_id == Some(ledger.get_id())),
            "per-miner endpoint missing {ledger:?} assignments"
        );
    }
    assert!(
        miner_rows.iter().any(|a| a.ledger_id.is_none()),
        "per-miner endpoint must retain capacity assignments"
    );

    // Submit/Publish views stay ledger-filtered; summary fields unchanged.
    for (route, ledger) in [
        ("submit", DataLedger::Submit),
        ("publish", DataLedger::Publish),
    ] {
        let (_, filtered_body) =
            get_json(&app, &format!("/v1/ledger/{route}/{miner}/assignments")).await;
        let filtered_rows = assignment_rows(&filtered_body);
        assert!(
            !filtered_rows.is_empty(),
            "{route} view should have assignments"
        );
        assert!(
            filtered_rows
                .iter()
                .all(|a| a.ledger_id == Some(ledger.get_id())),
            "{route} view must only contain {ledger:?} assignments"
        );
        for field in ["minerAddress", "assignmentStatus", "hashAnalysis"] {
            assert!(
                !filtered_body[field].is_null(),
                "{route} view lost its {field} field"
            );
        }
    }

    let (_, epoch_body) = get_json(&app, "/v1/epoch/current").await;
    assert_eq!(
        epoch_body["currentEpoch"].as_str(),
        Some(snapshot.epoch_height.to_string().as_str())
    );

    // Cross a real epoch boundary and verify the endpoint tracks the new
    // canonical snapshot rather than a stale one.
    node.mine_until_next_epoch().await?;
    let next_snapshot = node.get_canonical_epoch_snapshot();
    assert!(
        next_snapshot.epoch_height > snapshot.epoch_height,
        "canonical epoch snapshot must advance past genesis"
    );
    assert_roster_mirrors_snapshot(&app, &next_snapshot).await;

    node.stop().await;
    Ok(())
}
