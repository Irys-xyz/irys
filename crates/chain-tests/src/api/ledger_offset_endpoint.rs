//! Integration tests for the canonical ledger attribution endpoints:
//! `GET /v1/ledger/{ledger_id}/offset/{ledger_offset}/tx` and the
//! slot-relative variant
//! `GET /v1/ledger/{ledger_id}/slot/{slot_index}/offset/{slot_offset}/tx`.

use crate::utils::IrysNodeTest;
use actix_web::body::MessageBody;
use actix_web::dev::{Service, ServiceResponse};
use actix_web::http::StatusCode;
use actix_web::test::{TestRequest, call_service, read_body_json};
use irys_api_server::routes::ledger::LedgerOffsetTxResponse;
use irys_api_server::routes::tx::TxOffset;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_types::{
    BoundedFee, DataLedger, H256, NodeConfig, UnixTimestamp, hardfork_config::Cascade,
    irys::IrysSigner,
};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::sleep;

async fn get_status<T, B>(app: &T, uri: &str) -> StatusCode
where
    T: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
{
    let req = TestRequest::get().uri(uri).to_request();
    call_service(app, req).await.status()
}

async fn get_attribution<T, B>(app: &T, uri: &str) -> LedgerOffsetTxResponse
where
    T: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: MessageBody,
{
    let req = TestRequest::get().uri(uri).to_request();
    let resp = call_service(app, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "GET {uri} should succeed");
    read_body_json(resp).await
}

/// Publish/Submit flow on a pre-Cascade chain: two data txs share one block's
/// Submit range, then one is promoted and cross-checked against the
/// storage-derived `/tx/{tx_id}/local/data-start-offset` endpoint.
#[test_log::test(tokio::test)]
async fn heavy_ledger_offset_tx_endpoint() -> eyre::Result<()> {
    let chunk_size = 32_u64;
    let num_chunks_in_partition = 10_u64;
    let mut config = NodeConfig::testing().with_consensus(|c| {
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = num_chunks_in_partition;
        c.num_chunks_in_recall_range = 2;
        c.num_partitions_per_slot = 1;
        c.entropy_packing_iterations = 1_000;
        c.block_migration_depth = 1;
    });
    config.storage.num_writes_before_sync = 1;
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);

    let node = IrysNodeTest::new_genesis(config).start().await;
    node.node_ctx
        .packing_waiter
        .wait_for_idle(Some(Duration::from_secs(10)))
        .await?;
    let app = node.start_public_api().await;

    // tx_a: 3 full chunks; tx_b: 2 chunks + 1 byte, so its partial last chunk
    // must round up to 3 chunks in the attribution walk
    let tx_a = node
        .post_data_tx(
            node.get_anchor().await?,
            vec![1_u8; (3 * chunk_size) as usize],
            &signer,
        )
        .await;
    let tx_b = node
        .post_data_tx(
            node.get_anchor().await?,
            vec![2_u8; (2 * chunk_size + 1) as usize],
            &signer,
        )
        .await;
    node.wait_for_mempool(tx_a.header.id, 20).await?;
    node.wait_for_mempool(tx_b.header.id, 20).await?;

    let inclusion_block = node.mine_block().await?;
    // block_migration_depth = 1: the next block pushes the inclusion block
    // into the block index
    node.mine_block().await?;
    node.wait_for_block_in_index_height(inclusion_block.height, 20)
        .await?;

    // Both txs landed in the Submit ledger; together they own [0, 6)
    let mut offsets_by_tx: HashMap<H256, Vec<u64>> = HashMap::new();
    for offset in 0..6_u64 {
        let resp = get_attribution(&app, &format!("/v1/ledger/1/offset/{offset}/tx")).await;
        assert_eq!(resp.ledger_id, 1);
        assert_eq!(resp.ledger, DataLedger::Submit);
        assert_eq!(resp.ledger_offset, offset);
        assert_eq!(resp.slot_index, 0);
        assert_eq!(resp.slot_offset, offset);
        assert_eq!(resp.block_height, inclusion_block.height);
        assert_eq!(resp.block_hash, inclusion_block.block_hash);
        assert_eq!(resp.block_start_offset, 0);
        assert_eq!(resp.block_end_offset, 6);
        assert_eq!(resp.chunks_in_tx, 3);
        assert!(
            resp.tx_start_offset <= offset && offset < resp.tx_end_offset,
            "offset {offset} outside claimed tx range [{}, {})",
            resp.tx_start_offset,
            resp.tx_end_offset
        );
        let expected_data_root = if resp.tx_id == tx_a.header.id {
            tx_a.header.data_root
        } else if resp.tx_id == tx_b.header.id {
            tx_b.header.data_root
        } else {
            panic!("offset {offset} attributed to unknown tx {}", resp.tx_id);
        };
        assert_eq!(resp.data_root, expected_data_root);
        offsets_by_tx.entry(resp.tx_id).or_default().push(offset);
    }
    // Each tx owns a contiguous 3-chunk range, meeting at the boundary
    assert_eq!(offsets_by_tx.len(), 2);
    for offsets in offsets_by_tx.values() {
        assert_eq!(offsets.len(), 3);
        assert_eq!(offsets[2] - offsets[0], 2, "range must be contiguous");
    }
    let at_boundary_left = get_attribution(&app, "/v1/ledger/1/offset/2/tx").await;
    let at_boundary_right = get_attribution(&app, "/v1/ledger/1/offset/3/tx").await;
    assert_ne!(at_boundary_left.tx_id, at_boundary_right.tx_id);
    assert_eq!(at_boundary_right.tx_start_offset, 3);
    // txIndex points into the block's Submit tx list — cross-check the
    // response indices against the canonical block content
    let submit_tx_ids = &inclusion_block
        .data_ledgers
        .iter()
        .find(|dl| dl.ledger_id == DataLedger::Submit)
        .expect("inclusion block should carry a Submit ledger entry")
        .tx_ids
        .0;
    assert_eq!(
        at_boundary_left.tx_id,
        submit_tx_ids[at_boundary_left.tx_index as usize]
    );
    assert_eq!(
        at_boundary_right.tx_id,
        submit_tx_ids[at_boundary_right.tx_index as usize]
    );

    // The slot-relative endpoint resolves to the identical response
    let direct = get_attribution(&app, "/v1/ledger/1/offset/4/tx").await;
    let via_slot = get_attribution(&app, "/v1/ledger/1/slot/0/offset/4/tx").await;
    assert_eq!(
        serde_json::to_value(&direct)?,
        serde_json::to_value(&via_slot)?
    );

    // Beyond the Submit frontier
    assert_eq!(
        get_status(&app, "/v1/ledger/1/offset/6/tx").await,
        StatusCode::NOT_FOUND
    );
    // Cascade term ledgers are not active on this chain: no data, clean 404
    assert_eq!(
        get_status(&app, "/v1/ledger/10/offset/0/tx").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get_status(&app, "/v1/ledger/20/offset/0/tx").await,
        StatusCode::NOT_FOUND
    );
    // Invalid ledger ids (2 is not a DataLedger discriminant)
    assert_eq!(
        get_status(&app, "/v1/ledger/2/offset/0/tx").await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get_status(&app, "/v1/ledger/999/offset/0/tx").await,
        StatusCode::BAD_REQUEST
    );
    // Malformed offset: actix's Path extractor maps deserialization failures
    // to 404 by default, like every other typed-path endpoint in this API
    assert_eq!(
        get_status(&app, "/v1/ledger/1/offset/notanumber/tx").await,
        StatusCode::NOT_FOUND
    );
    // Slot offset must be < num_chunks_in_partition
    assert_eq!(
        get_status(
            &app,
            &format!("/v1/ledger/1/slot/0/offset/{num_chunks_in_partition}/tx")
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    // Slot index that overflows the ledger offset space
    assert_eq!(
        get_status(&app, &format!("/v1/ledger/1/slot/{}/offset/0/tx", u64::MAX)).await,
        StatusCode::BAD_REQUEST
    );

    // Promote tx_a into the Publish ledger, then cross-check attribution
    // against the storage-derived start offset (the spec's reverse check)
    node.upload_chunks(&tx_a).await?;
    node.wait_for_ingress_proofs(vec![tx_a.header.id], 60)
        .await?;

    let mut publish_offset = None;
    for _ in 0..30 {
        let req = TestRequest::get()
            .uri(&format!(
                "/v1/tx/{}/local/data-start-offset",
                tx_a.header.id
            ))
            .to_request();
        let resp = call_service(&app, req).await;
        if resp.status() == StatusCode::OK {
            let tx_offset: TxOffset = read_body_json(resp).await;
            publish_offset = Some(tx_offset.data_start_offset);
            break;
        }
        node.mine_block().await?;
        sleep(Duration::from_millis(500)).await;
    }
    let publish_offset =
        publish_offset.expect("tx_a should be promoted and its chunks migrated to Publish");

    let resp = get_attribution(&app, &format!("/v1/ledger/0/offset/{publish_offset}/tx")).await;
    assert_eq!(resp.ledger, DataLedger::Publish);
    assert_eq!(resp.ledger_id, 0);
    assert_eq!(resp.tx_id, tx_a.header.id);
    assert_eq!(resp.data_root, tx_a.header.data_root);
    assert_eq!(resp.tx_start_offset, publish_offset);
    assert_eq!(resp.chunks_in_tx, 3);

    node.stop().await;
    Ok(())
}

/// Post-Cascade chain: data txs target the OneYear/ThirtyDay term ledgers
/// directly and the attribution endpoints resolve their offsets.
#[test_log::test(tokio::test)]
async fn heavy_ledger_offset_tx_endpoint_cascade_term_ledgers() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2_u64;
    let chunk_size = 32_u64;
    let num_chunks_in_partition = 10_u64;

    let mut config = NodeConfig::testing().with_consensus(|c| {
        c.block_migration_depth = 1;
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = num_chunks_in_partition;
        c.hardforks.cascade = Some(Cascade {
            activation_timestamp: UnixTimestamp::from_secs(0),
            one_year_epoch_length: 8,
            thirty_day_epoch_length: 2,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
    });
    let signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&signer]);

    // 5 storage submodules so all 4 data ledgers have assigned partitions
    let test_node = IrysNodeTest::new_genesis(config);
    StorageSubmodulesConfig::load_for_test(test_node.cfg.base_directory.clone(), 5)?;
    let node = test_node.start_and_wait_for_packing("test", 30).await;
    let app = node.start_public_api().await;

    // Cascade activates at the first epoch boundary
    while node.get_canonical_chain_height().await <= num_blocks_in_epoch {
        node.mine_block().await?;
    }

    // Term-ledger txs target their ledger directly (no Submit staging)
    let mut term_txs = Vec::new();
    for (ledger, data) in [
        (DataLedger::OneYear, vec![7_u8; (3 * chunk_size) as usize]),
        (
            DataLedger::ThirtyDay,
            // partial last chunk rounds up to 3 chunks
            vec![8_u8; (2 * chunk_size + 1) as usize],
        ),
    ] {
        let price = node.get_data_price(ledger, data.len() as u64).await?;
        let tx = signer.sign_transaction(signer.create_transaction_with_fees(
            data,
            node.get_anchor().await?,
            ledger,
            BoundedFee::new(price.term_fee),
            None,
        )?)?;
        node.ingest_data_tx(tx.header.clone()).await?;
        node.wait_for_mempool(tx.header.id, 30).await?;
        term_txs.push((ledger, tx));
    }

    let inclusion_block = node.mine_block().await?;
    node.mine_block().await?;
    node.wait_for_block_in_index_height(inclusion_block.height, 20)
        .await?;

    for (ledger, tx) in &term_txs {
        let ledger_id = ledger.get_id();
        for offset in 0..3_u64 {
            let resp =
                get_attribution(&app, &format!("/v1/ledger/{ledger_id}/offset/{offset}/tx")).await;
            assert_eq!(resp.ledger, *ledger);
            assert_eq!(resp.ledger_id, ledger_id);
            assert_eq!(resp.ledger_offset, offset);
            assert_eq!(resp.block_height, inclusion_block.height);
            assert_eq!(resp.tx_id, tx.header.id);
            assert_eq!(resp.data_root, tx.header.data_root);
            assert_eq!(resp.tx_index, 0);
            assert_eq!(resp.tx_start_offset, 0);
            assert_eq!(resp.tx_end_offset, 3);
            assert_eq!(resp.chunks_in_tx, 3);
        }
        // Beyond each term ledger's frontier
        assert_eq!(
            get_status(&app, &format!("/v1/ledger/{ledger_id}/offset/3/tx")).await,
            StatusCode::NOT_FOUND
        );
    }

    // Slot-relative endpoint works for term ledgers too
    let direct = get_attribution(&app, "/v1/ledger/20/offset/1/tx").await;
    let via_slot = get_attribution(&app, "/v1/ledger/20/slot/0/offset/1/tx").await;
    assert_eq!(
        serde_json::to_value(&direct)?,
        serde_json::to_value(&via_slot)?
    );

    // Term txs never pass through the Submit staging ledger
    assert_eq!(
        get_status(&app, "/v1/ledger/1/offset/0/tx").await,
        StatusCode::NOT_FOUND
    );

    node.stop().await;
    Ok(())
}
