//! Integration tests for the partition-hash-keyed local interval endpoint:
//! `GET /v1/storage/partition/{partition_hash}/intervals/{chunk_type}`.
//!
//! Capacity partitions have no ledger/slot coordinates, so this route is the
//! only way to observe their locally stored entropy intervals.

use crate::utils::IrysNodeTest;
use actix_web::body::MessageBody;
use actix_web::dev::{Service, ServiceResponse};
use actix_web::http::StatusCode;
use actix_web::test::{TestRequest, call_service, read_body_json};
use irys_config::submodules::StorageSubmodulesConfig;
use irys_types::{DataLedger, H256, NodeConfig};

async fn get<T, B>(app: &T, uri: &str) -> (StatusCode, serde_json::Value)
where
    T: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: MessageBody,
{
    let req = TestRequest::get().uri(uri).to_request();
    let resp = call_service(app, req).await;
    let status = resp.status();
    let body = if status == StatusCode::OK {
        read_body_json(resp).await
    } else {
        serde_json::Value::Null
    };
    (status, body)
}

/// A fully packed node: the capacity partition's entropy intervals are
/// readable by hash and cover the whole partition; the hash-keyed route
/// agrees byte-for-byte with the ledger/slot route on data partitions;
/// malformed and unknown hashes map to 400/404.
#[test_log::test(tokio::test)]
async fn heavy_partition_intervals_endpoint() -> eyre::Result<()> {
    let num_chunks_in_partition = 10_u64;
    let config = NodeConfig::testing().with_consensus(|c| {
        c.chunk_size = 32;
        c.num_chunks_in_partition = num_chunks_in_partition;
        c.entropy_packing_iterations = 1_000;
    });

    // 5 storage submodules: Publish slot 0 + Submit slot 0 stay data
    // partitions, the rest remain capacity
    let test_node = IrysNodeTest::new_genesis(config);
    StorageSubmodulesConfig::load_for_test(test_node.cfg.base_directory.clone(), 5)?;
    let node = test_node.start_and_wait_for_packing("test", 30).await;
    let app = node.start_public_api().await;

    let snapshot = node.get_canonical_epoch_snapshot();
    let capacity_hash = *snapshot
        .partition_assignments
        .capacity_partitions
        .keys()
        .next()
        .expect("test setup should leave at least one capacity partition");

    // The acceptance scenario: a capacity assignment (no ledger id, no slot
    // index) resolved directly against the hosting miner by partition hash
    let (status, body) = get(
        &app,
        &format!("/v1/storage/partition/{capacity_hash}/intervals/Entropy"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["partitionHash"], capacity_hash.to_string());
    assert_eq!(body["chunkType"], "Entropy");
    // Fully packed after start_and_wait_for_packing: one interval covering
    // 0 ..= num_chunks_in_partition - 1 (inclusive end convention)
    assert_eq!(
        body["intervals"],
        serde_json::json!([{"start": 0, "end": num_chunks_in_partition - 1}]),
        "fully packed capacity partition must cover the whole partition"
    );

    // Intervals are sorted by start and bounded by the partition size
    let intervals = body["intervals"].as_array().expect("intervals array");
    for pair in intervals.windows(2) {
        assert!(pair[0]["start"].as_u64() < pair[1]["start"].as_u64());
    }
    for interval in intervals {
        assert!(interval["end"].as_u64().unwrap() < num_chunks_in_partition);
    }

    // The hash-keyed route and the ledger/slot route answer identically for
    // a data partition — same serialization, same inclusive end bound
    let submit_hash = snapshot
        .partition_assignments
        .data_partitions
        .values()
        .find(|pa| pa.ledger_id == Some(DataLedger::Submit.get_id()) && pa.slot_index == Some(0))
        .expect("Submit slot 0 must be assigned")
        .partition_hash;
    let (status_by_hash, by_hash) = get(
        &app,
        &format!("/v1/storage/partition/{submit_hash}/intervals/Entropy"),
    )
    .await;
    let (status_by_slot, by_slot) = get(&app, "/v1/storage/intervals/Submit/0/Entropy").await;
    assert_eq!(status_by_hash, StatusCode::OK);
    assert_eq!(status_by_slot, StatusCode::OK);
    assert_eq!(
        by_hash["intervals"], by_slot["intervals"],
        "hash-keyed and ledger/slot routes must share interval semantics"
    );

    // A well-formed hash this node does not host: 404
    let (status, _) = get(
        &app,
        &format!("/v1/storage/partition/{}/intervals/Entropy", H256::random()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Malformed hashes: 400 (bad alphabet, and valid base58 of wrong length)
    for bad in ["!!!not-base58!!!", "abc"] {
        let (status, _) = get(
            &app,
            &format!("/v1/storage/partition/{bad}/intervals/Entropy"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "input {bad:?}");
    }

    // Garbage chunk type follows the API-wide Path-extractor convention: 404
    let (status, _) = get(
        &app,
        &format!("/v1/storage/partition/{capacity_hash}/intervals/NotAChunkType"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    node.stop().await;
    Ok(())
}
