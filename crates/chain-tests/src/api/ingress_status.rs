//! `GET /v1/tx/{tx_id}/ingress-status` against a live node.

use crate::api::client_request;
use crate::utils::{IrysNodeTest, coverage_adjusted_timeout};
use irys_api_client::{ApiClient as _, ApiClientExt as _, IrysApiClient};
use irys_chain::IrysNodeCtx;
use irys_database::db::IrysDatabaseExt as _;
use irys_types::{H256, NodeConfig};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr as _;
use std::time::{Duration, Instant};
use tokio::time::sleep;

#[test_log::test(tokio::test)]
async fn slow_ingress_status_endpoint_reports_proofs_then_promotion() -> eyre::Result<()> {
    let config = NodeConfig::testing();
    let ctx = IrysNodeTest::new_genesis(config).start().await;
    ctx.wait_for_packing(20).await;
    let migration_depth = ctx.node_ctx.config.consensus.block_migration_depth;

    let api_address = SocketAddr::new(
        IpAddr::from_str("127.0.0.1").expect("localhost"),
        ctx.node_ctx.config.node_config.http.bind_port,
    );
    let api_client = IrysApiClient::new(irys_types::DEFAULT_MAX_PEER_RESPONSE_BYTES);
    let base = format!("http://{api_address}/v1");

    let unknown = H256::random();
    let missing = api_client.get_ingress_status(api_address, unknown).await?;
    assert!(
        missing.is_none(),
        "unknown id is 404, which the client maps to None"
    );
    let missing_raw = client_request(&format!("{base}/tx/{unknown}/ingress-status")).await;
    assert_eq!(missing_raw.status(), reqwest::StatusCode::NOT_FOUND);

    let malformed = client_request(&format!("{base}/tx/not-a-valid-id/ingress-status")).await;
    // `web::Path<H256>` rejects invalid base58 with actix's path-extractor 404,
    // the same status as an unknown transaction. The body identifies the decode.
    assert_eq!(malformed.status(), reqwest::StatusCode::NOT_FOUND);
    let malformed_body = malformed.text().await?;
    assert!(
        malformed_body.contains("Failed to decode H256"),
        "malformed path id body: {malformed_body}"
    );

    let tx = ctx
        .create_signed_data_tx(
            &ctx.node_ctx.config.irys_signer(),
            vec![1, 2, 3, 4, 5, 6, 7, 8],
        )
        .await?;
    let tx_id = tx.header.id;
    api_client.upload_chunks(api_address, &tx).await?;
    api_client
        .post_transaction(api_address, tx.header.clone())
        .await?;

    ctx.mine_block().await?;
    ctx.wait_for_ingress_proofs_no_mining(vec![tx_id], 20)
        .await?;

    let awaiting = api_client
        .get_ingress_status(api_address, tx_id)
        .await?
        .expect("submitted tx is known");
    let tip = ctx
        .get_canonical_chain()
        .last()
        .cloned()
        .expect("canonical tip");
    assert_eq!(awaiting.tx_id, tx_id);
    assert_eq!(awaiting.data_root, tx.header.data_root);
    assert_eq!(awaiting.as_of_block_height, tip.height());
    assert_eq!(awaiting.as_of_block_hash, tip.block_hash());
    assert!(
        awaiting.promotion_height.is_none(),
        "enough proofs are not a promotion"
    );
    let proofs = awaiting
        .proofs
        .as_ref()
        .expect("unpromoted cache is present");
    assert!(proofs.observed >= 1);
    let requirements = awaiting
        .requirements
        .as_ref()
        .expect("unpromoted requirements are present");
    assert_eq!(requirements.total, 1);
    assert_eq!(requirements.assigned, 0);
    let remaining = awaiting.remaining.as_ref().expect("unpromoted deficit");
    assert_eq!(remaining.total, 0);
    assert_eq!(remaining.assigned, 0);
    assert_eq!(awaiting.proof_requirements_met, Some(true));
    let miner = ctx.node_ctx.config.node_config.miner_address();
    assert!(
        proofs
            .miners
            .iter()
            .any(|row| row.address == miner && row.eligible),
        "the local miner should have an active proof"
    );

    let promotion_block = ctx.mine_block().await?;
    ctx.wait_for_promotion(&tx_id, 20).await?;

    let promoted = api_client
        .get_ingress_status(api_address, tx_id)
        .await?
        .expect("promoted tx is known");
    assert_eq!(promoted.promotion_height, Some(promotion_block.height));
    assert!(promoted.proofs.is_none());
    assert!(promoted.requirements.is_none());
    assert!(promoted.remaining.is_none());
    assert!(promoted.proof_requirements_met.is_none());

    let db_before_migration = db_promoted_height(&ctx, tx_id)?;
    assert!(
        db_before_migration.is_none(),
        "promoted_height is written at migration, not at confirmation"
    );

    ctx.mine_blocks(migration_depth as usize).await?;

    let deadline = Instant::now() + Duration::from_secs(coverage_adjusted_timeout(20_u64));
    let mut migrated = None;
    while Instant::now() < deadline {
        migrated = db_promoted_height(&ctx, tx_id)?;
        if migrated.is_some() {
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(migrated, Some(promotion_block.height));

    let after = api_client
        .get_ingress_status(api_address, tx_id)
        .await?
        .expect("migrated tx is known");
    let new_tip = ctx
        .get_canonical_chain()
        .last()
        .cloned()
        .expect("canonical tip");
    assert_eq!(after.promotion_height, Some(promotion_block.height));
    assert!(after.proofs.is_none());
    assert!(after.requirements.is_none());
    assert!(after.remaining.is_none());
    assert!(after.proof_requirements_met.is_none());
    assert_eq!(after.as_of_block_height, new_tip.height());
    assert_eq!(after.as_of_block_hash, new_tip.block_hash());
    assert_ne!(after.as_of_block_height, promotion_block.height);

    ctx.stop().await;
    Ok(())
}

fn db_promoted_height(ctx: &IrysNodeTest<IrysNodeCtx>, tx_id: H256) -> eyre::Result<Option<u64>> {
    let metadata = ctx.node_ctx.db.view_eyre(|tx| {
        irys_database::get_data_tx_metadata(tx, &tx_id).map_err(|e| eyre::eyre!("{:?}", e))
    })?;
    Ok(metadata.and_then(|meta| meta.promoted_height))
}
