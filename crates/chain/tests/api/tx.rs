//! endpoint tests
use crate::utils::IrysNodeTest;
use actix_http::StatusCode;
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;
use base58::ToBase58 as _;
use irys_actors::packing::wait_for_packing;
use irys_database::{database, db::IrysDatabaseExt as _};
use irys_types::{
    irys::IrysSigner, CommitmentTransaction, IrysTransactionHeader, IrysTransactionResponse, ConsensusConfig,
    NodeConfig, H256,
};
use reth_db::Database as _;
use tokio::time::Duration;
use tracing::{error, info};

#[actix_web::test]
async fn test_get_tx() -> eyre::Result<()> {
    let mut config = NodeConfig::testnet();
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(690000000000000000_u128),
            ..Default::default()
        },
    )]);
    let node = IrysNodeTest::new_genesis(config.clone()).start().await;
    wait_for_packing(
        node.node_ctx.actor_addresses.packing.clone(),
        Some(Duration::from_secs(10)),
    )
    .await?;

    node.node_ctx.start_mining().await.unwrap();
    let db = node.node_ctx.db.clone();

    let storage_tx = IrysTransactionHeader {
        id: H256::random(),
        ..Default::default()
    };
    info!("Generated storage_tx.id: {}", storage_tx.id);

    let consensus = ConsensusConfig::testnet();
    let mut commitment_tx = CommitmentTransaction::new_stake(&consensus, H256::default());
    commitment_tx.id = H256::random();
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

    let app = node.start_public_api().await;

    // Test storage transaction
    let id: String = storage_tx.id.as_bytes().to_base58();
    let req = actix_web::test::TestRequest::get()
        .uri(&format!("/v1/tx/{}", &id))
        .to_request();

    let resp = actix_web::test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let transaction: IrysTransactionResponse = actix_web::test::read_body_json(resp).await;
    info!("{}", serde_json::to_string_pretty(&transaction).unwrap());

    // Extract storage transaction or fail
    let storage = match transaction {
        IrysTransactionResponse::Storage(storage) => storage,
        IrysTransactionResponse::Commitment(_) => {
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
    let transaction: IrysTransactionResponse = actix_web::test::read_body_json(resp).await;
    info!("{}", serde_json::to_string_pretty(&transaction).unwrap());

    // Extract commitment transaction or fail
    let commitment = match transaction {
        IrysTransactionResponse::Commitment(commitment) => commitment,
        IrysTransactionResponse::Storage(_) => {
            panic!("Expected Commitment transaction, got Storage")
        }
    };
    assert_eq!(commitment_tx, commitment);
    node.node_ctx.stop().await;
    Ok(())
}
