//! endpoint tests
use crate::utils::IrysNodeTest;
use actix_http::StatusCode;
use actix_web::{middleware::Logger, App};
use alloy_core::primitives::U256;
use base58::ToBase58;
use irys_actors::packing::wait_for_packing;
use irys_api_server::{routes, routes::tx::IrysTransaction, ApiState};
use irys_database::database;
use irys_types::{
    irys::IrysSigner, CommitmentTransaction, Config, IrysTransactionHeader,
    H256,
};
use reth_db::Database;
use reth_primitives::GenesisAccount;
use tokio::time::Duration;
use tracing::{error, info};

#[actix_web::test]
async fn test_get_tx() -> eyre::Result<()> {
    let test_config = Config::testnet();
    let signer = IrysSigner::random_signer(&test_config);
    let mut node = IrysNodeTest::new_genesis(test_config.clone());
    node.cfg.irys_node_config.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(690000000000000000_u128),
            ..Default::default()
        },
    )]);
    let node = node.start().await;
    wait_for_packing(
        node.node_ctx.actor_addresses.packing.clone(),
        Some(Duration::from_secs(10)),
    )
    .await?;

    node.node_ctx.actor_addresses.start_mining().unwrap();
    let db = node.node_ctx.db.clone();

    let storage_tx = IrysTransactionHeader {
        id: H256::random(),
        ..Default::default()
    };
    info!("Generated storage_tx.id: {}", storage_tx.id);

    let commitment_tx = CommitmentTransaction {
        id: H256::random(),
        ..Default::default()
    };
    info!("Generated commitment_tx.id: {}", commitment_tx.id);

    // Insert the storage_tx and make sure it's in the database
    let _ = db.update(|tx| -> eyre::Result<()> { database::insert_tx_header(tx, &storage_tx) })?;
    match db.view_eyre(|tx| database::tx_header_by_txid(tx, &storage_tx.id))? {
        None => error!("tx not found, test db error!"),
        Some(_tx_header) => info!("storage_tx found!"),
    };

    // Insert the commitment_tx and make sure it's in the database
    let _ =
        db.update(|tx| -> eyre::Result<()> { database::insert_commitment_tx(tx, &commitment_tx) })?;
    match db.view_eyre(|tx| database::commitment_tx_by_txid(tx, &commitment_tx.id))? {
        None => error!("tx not found, test db error!"),
        Some(_tx_header) => info!("commitment_tx found!"),
    };

    let app_state = ApiState {
        reth_provider: node.node_ctx.reth_handle.clone(),
        reth_http_url: node
            .node_ctx
            .reth_handle
            .rpc_server_handle()
            .http_url()
            .unwrap(),
        block_index: node.node_ctx.block_index_guard.clone(),
        block_tree: node.node_ctx.block_tree_guard.clone(),
        db: node.node_ctx.db.clone(),
        mempool: node.node_ctx.actor_addresses.mempool.clone(),
        chunk_provider: node.node_ctx.chunk_provider.clone(),
        config: test_config,
    };

    // Start the actix webserver
    let app = actix_web::test::init_service(
        App::new()
            .wrap(Logger::default())
            .app_data(actix_web::web::Data::new(app_state))
            .service(routes()),
    )
    .await;

    // Test storage transaction
    let id: String = storage_tx.id.as_bytes().to_base58();
    let req = actix_web::test::TestRequest::get()
        .uri(&format!("/v1/tx/{}", &id))
        .to_request();

    let resp = actix_web::test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let transaction: IrysTransaction = actix_web::test::read_body_json(resp).await;
    info!("{}", serde_json::to_string_pretty(&transaction).unwrap());

    // Extract storage transaction or fail
    let storage = match transaction {
        IrysTransaction::Storage(storage) => storage,
        IrysTransaction::Commitment(_) => {
            panic!("Expected Storage transaction, got Commitment")
        }
    };
    assert_eq!(storage_tx, storage);

    // Test commitment transaction
    let id: String = commitment_tx.id.as_bytes().to_base58();
    let req = actix_web::test::TestRequest::get()
        .uri(&format!("/v1/tx/{}", &id))
        .to_request();

    let resp = actix_web::test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let transaction: IrysTransaction = actix_web::test::read_body_json(resp).await;
    info!("{}", serde_json::to_string_pretty(&transaction).unwrap());

    // Extract commitment transaction or fail
    let commitment = match transaction {
        IrysTransaction::Commitment(commitment) => commitment,
        IrysTransaction::Storage(_) => panic!("Expected Commitment transaction, got Storage"),
    };
    assert_eq!(commitment_tx, commitment);
    node.node_ctx.stop().await;
    Ok(())
}
