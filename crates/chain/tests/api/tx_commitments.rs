use crate::{api::post_commitment_tx_request, utils::IrysNodeTest};
use actix_web::{middleware::Logger, App};
use alloy_core::primitives::U256;
use irys_actors::{packing::wait_for_packing, CommitmentCacheMessage, CommitmentStatus};
use irys_api_server::routes;
use irys_chain::IrysNodeCtx;
use irys_types::{irys::IrysSigner, CommitmentTransaction, NodeConfig, H256};
use reth_primitives::{irys_primitives::CommitmentType, GenesisAccount};
use tokio::time::Duration;
use tracing::info;

#[actix_web::test]
async fn test_commitments_basic_test() -> eyre::Result<()> {
    // ===== TEST SETUP =====
    // Create test environment with a funded signer for transaction creation
    let (ema_tx, _ema_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut config = NodeConfig::testnet();
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(690000000000000000_u128),
            ..Default::default()
        },
    )]);
    let node = IrysNodeTest::new_genesis(config.clone())
        .await
        .start()
        .await;

    let uri = format!(
        "http://127.0.0.1:{}",
        node.node_ctx.config.node_config.http.port
    );

    // Initialize packing and mining
    wait_for_packing(
        node.node_ctx.actor_addresses.packing.clone(),
        Some(Duration::from_secs(10)),
    )
    .await?;

    node.node_ctx.actor_addresses.start_mining().unwrap();
    let api_state = node.node_ctx.get_api_state(ema_tx);
    let _db = api_state.db.clone();

    // Start the API server
    let _app = actix_web::test::init_service(
        App::new()
            .wrap(Logger::default())
            .app_data(actix_web::web::Data::new(api_state))
            .service(routes()),
    )
    .await;

    // ===== TEST CASE 1: Stake Commitment Creation and Processing =====
    // Create a new stake commitment transaction
    let stake_tx = CommitmentTransaction {
        id: H256::random(),
        commitment_type: CommitmentType::Stake,
        fee: 1,
        ..Default::default()
    };
    let stake_tx = signer.sign_commitment(stake_tx).unwrap();
    info!("Generated stake_tx.id: {}", stake_tx.id);

    // Verify stake commitment starts in 'Unknown' state
    let status = get_commitment_status(&stake_tx, &node.node_ctx).await;
    assert_eq!(status, CommitmentStatus::Unknown);

    // Submit stake commitment via API
    post_commitment_tx_request(&uri, &stake_tx).await;

    // Mine a block to include the commitment
    node.mine_block().await.unwrap();

    // Verify stake commitment is now 'Accepted'
    let status = get_commitment_status(&stake_tx, &node.node_ctx).await;
    assert_eq!(status, CommitmentStatus::Accepted);

    // ===== TEST CASE 2: Pledge Creation for Staked Address =====
    // Create a pledge commitment for the already staked address
    let pledge_tx = CommitmentTransaction {
        id: H256::random(),
        commitment_type: CommitmentType::Pledge,
        fee: 1,
        ..Default::default()
    };
    let pledge_tx = signer.sign_commitment(pledge_tx).unwrap();
    info!("Generated pledge_tx.id: {}", pledge_tx.id);

    // Verify pledge starts in 'Unknown' state
    let status = get_commitment_status(&pledge_tx, &node.node_ctx).await;
    assert_eq!(status, CommitmentStatus::Unknown);

    // Submit pledge via API
    post_commitment_tx_request(&uri, &pledge_tx).await;

    // Verify pledge is still 'Unknown' before mining
    let status = get_commitment_status(&pledge_tx, &node.node_ctx).await;
    assert_eq!(status, CommitmentStatus::Unknown);

    // Mine a block to include the pledge
    node.mine_block().await.unwrap();

    // Verify pledge is now 'Accepted' after mining
    let status = get_commitment_status(&pledge_tx, &node.node_ctx).await;
    assert_eq!(status, CommitmentStatus::Accepted);

    // ===== TEST CASE 3: Re-submitting Existing Commitment =====
    // Verify stake commitment is still accepted
    let status = get_commitment_status(&stake_tx, &node.node_ctx).await;
    assert_eq!(status, CommitmentStatus::Accepted);

    // Re-submit the same stake commitment
    post_commitment_tx_request(&uri, &stake_tx).await;
    node.mine_block().await.unwrap();

    // Verify stake is still 'Accepted' (idempotent operation)
    let status = get_commitment_status(&stake_tx, &node.node_ctx).await;
    assert_eq!(status, CommitmentStatus::Accepted);

    // ===== TEST CASE 4: Pledge Without Stake (Should Fail) =====
    // Create a new signer without any stake commitment
    let signer2 = IrysSigner::random_signer(&config.consensus_config());

    // Create a pledge for the unstaked address
    let pledge_tx = CommitmentTransaction {
        id: H256::random(),
        commitment_type: CommitmentType::Pledge,
        fee: 1,
        ..Default::default()
    };
    let pledge_tx = signer2.sign_commitment(pledge_tx).unwrap();
    info!("Generated pledge_tx.id: {}", pledge_tx.id);

    // Verify pledge starts in 'Unstaked' state
    let status = get_commitment_status(&pledge_tx, &node.node_ctx).await;
    assert_eq!(status, CommitmentStatus::Unstaked);

    // Submit pledge via API
    post_commitment_tx_request(&uri, &pledge_tx).await;
    node.mine_block().await.unwrap();

    // Verify pledge remains 'Unstaked' (invalid without stake)
    let status = get_commitment_status(&pledge_tx, &node.node_ctx).await;
    assert_eq!(status, CommitmentStatus::Unstaked);

    // ===== TEST CLEANUP =====
    node.node_ctx.stop().await;
    Ok(())
}

async fn get_commitment_status(
    commitment_tx: &CommitmentTransaction,
    node_context: &IrysNodeCtx,
) -> CommitmentStatus {
    let commitment_cache = &node_context.service_senders.commitment_cache;
    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();

    let _ = commitment_cache.send(CommitmentCacheMessage::GetCommitmentStatus {
        commitment_tx: commitment_tx.clone(),
        response: oneshot_tx,
    });

    let status = oneshot_rx
        .await
        .expect("to receive CommitmentStatus from GetCommitmentStatus message");
    status
}
