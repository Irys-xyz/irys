use crate::error::ApiError;
use crate::ApiState;
use actix_web::{
    web::{self, Json},
    HttpResponse, Result,
};
use awc::http::StatusCode;
use irys_actors::mempool_service::{TxIngressError, TxIngressMessage};
use irys_database::{database, DataLedger};
use irys_types::{u64_stringify, CommitmentTransaction, IrysTransactionHeader, H256};
use reth_db::Database;
use serde::{Deserialize, Serialize};
use tracing::info;

/// Handles the HTTP POST request for adding a transaction to the mempool.
/// This function takes in a JSON payload of a `IrysTransactionHeader` type,
/// encapsulates it into a `TxIngressMessage` for further processing by the
/// mempool actor, and manages error handling based on the results of message
/// delivery and transaction validation.
pub async fn post_tx(
    state: web::Data<ApiState>,
    body: Json<IrysTransactionHeader>,
) -> actix_web::Result<HttpResponse> {
    let tx = body.into_inner();

    // Validate transaction is valid. Check balances etc etc.
    let tx_ingress_msg = TxIngressMessage(tx);
    let msg_result = state.mempool.send(tx_ingress_msg).await;

    // Handle failure to deliver the message (e.g., actor unresponsive or unavailable)
    if let Err(err) = msg_result {
        return Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
            .body(format!("Failed to deliver transaction: {:?}", err)));
    }

    // If message delivery succeeded, check for validation errors within the response
    let inner_result = msg_result.unwrap();
    if let Err(err) = inner_result {
        return match err {
            TxIngressError::InvalidSignature => Ok(HttpResponse::build(StatusCode::BAD_REQUEST)
                .body(format!("Invalid Signature: {:?}", err))),
            TxIngressError::Unfunded => Ok(HttpResponse::build(StatusCode::PAYMENT_REQUIRED)
                .body(format!("Unfunded: {:?}", err))),
            TxIngressError::Skipped => Ok(HttpResponse::Ok()
                .body("Already processed: the transaction was previously handled")),
            TxIngressError::Other(err) => {
                Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(format!("Failed to deliver transaction: {:?}", err)))
            }
            TxIngressError::InvalidAnchor => Ok(HttpResponse::build(StatusCode::BAD_REQUEST)
                .body(format!("Invalid Signature: {:?}", err))),
            TxIngressError::DatabaseError => {
                Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(format!("Internal database error: {:?}", err)))
            }
            TxIngressError::ServiceUninitialized => {
                Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(format!("Internal service error: {:?}", err)))
            }
        };
    }

    // If everything succeeded, return an HTTP 200 OK response
    Ok(HttpResponse::Ok().finish())
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum IrysTransaction {
    #[serde(rename = "commitment")]
    Commitment(CommitmentTransaction),

    #[serde(rename = "storage")]
    Storage(IrysTransactionHeader),
}

pub async fn get_transaction_api(
    state: web::Data<ApiState>,
    path: web::Path<H256>,
) -> Result<Json<IrysTransaction>, ApiError> {
    let tx_id: H256 = path.into_inner();
    info!("Get tx by tx_id: {}", tx_id);
    get_transaction(&state, tx_id).map(web::Json)
}
// Helper function to retrieve IrysTransactionHeader
pub fn get_storage_transaction(
    state: &web::Data<ApiState>,
    tx_id: H256,
) -> Result<IrysTransactionHeader, ApiError> {
    state
        .db
        .view_eyre(|tx| database::tx_header_by_txid(tx, &tx_id))
        .map_err(|_| ApiError::Internal {
            err: String::from("db error while looking up Irys transaction"),
        })
        .and_then(|opt| {
            opt.ok_or(ApiError::ErrNoId {
                id: tx_id.to_string(),
                err: String::from("storage tx not found"),
            })
        })
}

// Helper function to retrieve CommitmentTransaction
pub fn get_commitment_transaction(
    state: &web::Data<ApiState>,
    tx_id: H256,
) -> Result<CommitmentTransaction, ApiError> {
    state
        .db
        .view_eyre(|tx| database::commitment_tx_by_txid(tx, &tx_id))
        .map_err(|_| ApiError::Internal {
            err: String::from("db error while looking up commitment transaction"),
        })
        .and_then(|opt| {
            opt.ok_or(ApiError::ErrNoId {
                id: tx_id.to_string(),
                err: String::from("commitment tx not found"),
            })
        })
}

// Combined function to get either type of transaction
pub fn get_transaction(
    state: &web::Data<ApiState>,
    tx_id: H256,
) -> Result<IrysTransaction, ApiError> {
    get_storage_transaction(state, tx_id)
        .map(IrysTransaction::Storage)
        .or_else(|err| match err {
            ApiError::ErrNoId { .. } => {
                get_commitment_transaction(state, tx_id).map(IrysTransaction::Commitment)
            }
            other => Err(other),
        })
}

// Modified to work only with storage transactions
pub async fn get_tx_local_start_offset(
    state: web::Data<ApiState>,
    path: web::Path<H256>,
) -> Result<Json<TxOffset>, ApiError> {
    let tx_id: H256 = path.into_inner();
    info!("Get tx data metadata by tx_id: {}", tx_id);

    // Only works for storage transaction header
    let tx_header = get_storage_transaction(&state, tx_id)?;
    let ledger = DataLedger::try_from(tx_header.ledger_id).unwrap();

    match state
        .chunk_provider
        .get_ledger_offsets_for_data_root(ledger, tx_header.data_root)
    {
        Err(_error) => Err(ApiError::Internal {
            err: String::from("db error"),
        }),
        Ok(None) => Err(ApiError::ErrNoId {
            id: tx_id.to_string(),
            err: String::from("Transaction data isn't stored by this node"),
        }),
        Ok(Some(offsets)) => {
            let offset = offsets.first().copied().ok_or(ApiError::Internal {
                err: String::from("ChunkProvider error"), // the ChunkProvider should only return a Some if the vec has at least one element
            })?;
            Ok(web::Json(TxOffset {
                data_start_offset: offset,
            }))
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TxOffset {
    #[serde(default, with = "u64_stringify")]
    pub data_start_offset: u64,
}

#[cfg(test)]
mod tests {
    use crate::routes;

    use super::*;
    use actix::SystemService as _;
    use actix_web::{middleware::Logger, test, App};
    use base58::ToBase58;
    use database::open_or_create_db;
    use irys_actors::mempool_service::MempoolService;
    use irys_actors::peer_list_service::PeerListService;
    use irys_database::tables::IrysTables;
    use irys_storage::ChunkProvider;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{app_state::DatabaseProvider, Config, StorageConfig};
    use std::sync::Arc;
    use tracing::{error, info};

    #[actix_web::test]
    async fn test_get_tx() -> eyre::Result<()> {
        std::env::set_var("RUST_LOG", "debug");
        let tmp_dir = setup_tracing_and_temp_dir(Some("test_get_tx"), false);
        let db_env = open_or_create_db(tmp_dir, IrysTables::ALL, None).unwrap();
        let db = DatabaseProvider(Arc::new(db_env));

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
        let _ =
            db.update(|tx| -> eyre::Result<()> { database::insert_tx_header(tx, &storage_tx) })?;
        match db.view_eyre(|tx| database::tx_header_by_txid(tx, &storage_tx.id))? {
            None => error!("tx not found, test db error!"),
            Some(_tx_header) => info!("storage_tx found!"),
        };

        // Insert the commitment_tx and make sure it's in the database
        let _ = db.update(|tx| -> eyre::Result<()> {
            database::insert_commitment_tx(tx, &commitment_tx)
        })?;
        match db.view_eyre(|tx| database::commitment_tx_by_txid(tx, &commitment_tx.id))? {
            None => error!("tx not found, test db error!"),
            Some(_tx_header) => info!("commitment_tx found!"),
        };

        // Initialize a dummy ApiState so we can start a test actix web server
        let config = Config::testnet();
        let storage_config = StorageConfig::new(&config);
        let mempool_addr = MempoolService::from_registry();
        let peer_list_addr = PeerListService::from_registry();
        let chunk_provider =
            ChunkProvider::new(storage_config.clone(), Arc::new(Vec::new()).to_vec());
        let app_state = ApiState {
            reth_provider: None,
            reth_http_url: None,
            block_index: None,
            block_tree: None,
            db: db,
            mempool: mempool_addr,
            peer_list: peer_list_addr,
            chunk_provider: Arc::new(chunk_provider),
            config,
        };

        // Start the actix webserver
        let app = test::init_service(
            App::new()
                .wrap(Logger::default())
                .app_data(web::Data::new(app_state))
                .service(routes()),
        )
        .await;

        // Test storage transaction
        let id: String = storage_tx.id.as_bytes().to_base58();
        let req = test::TestRequest::get()
            .uri(&format!("/v1/tx/{}", &id))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let transaction: IrysTransaction = test::read_body_json(resp).await;
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
        let req = test::TestRequest::get()
            .uri(&format!("/v1/tx/{}", &id))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let transaction: IrysTransaction = test::read_body_json(resp).await;
        info!("{}", serde_json::to_string_pretty(&transaction).unwrap());

        // Extract commitment transaction or fail
        let commitment = match transaction {
            IrysTransaction::Commitment(commitment) => commitment,
            IrysTransaction::Storage(_) => panic!("Expected Commitment transaction, got Storage"),
        };
        assert_eq!(commitment_tx, commitment);

        Ok(())
    }

    //     #[actix_web::test]
    //     async fn test_get_non_existent_tx() -> Result<(), Error> {
    //         // std::env::set_var("RUST_LOG", "debug");
    //         // env_logger::init();

    //         let path = tempdir().unwrap();
    //         let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();
    //         let tx = IrysTransactionHeader::default();

    //         let db_arc = Arc::new(db);

    //         let task_manager = TaskManager::current();
    //         let storage_config = StorageConfig::default();

    //         let mempool_service = MempoolService::new(
    //             irys_types::app_state::DatabaseProvider(db_arc.clone()),
    //             task_manager.executor(),
    //             IrysSigner::random_signer(),
    //             storage_config.clone(),
    //             Arc::new(Vec::new()).to_vec(),
    //         );
    //         SystemRegistry::set(mempool_service.start());
    //         let mempool_addr = MempoolService::from_registry();

    //         let chunk_provider = ChunkProvider::new(
    //             storage_config.clone(),
    //             Arc::new(Vec::new()).to_vec(),
    //             DatabaseProvider(db_arc.clone()),
    //         );

    //         let app_state = ApiState {
    //             reth_provider: None,
    //             reth_http_url: None,
    //             block_index: None,
    //             block_tree: None,
    //             db: DatabaseProvider(db_arc.clone()),
    //             mempool: mempool_addr,
    //             chunk_provider: Arc::new(chunk_provider),
    //         };

    //         let app = test::init_service(
    //             App::new()
    //                 .app_data(web::Data::new(app_state))
    //                 .service(web::scope("/v1").route("/tx/{tx_id}", web::get().to(get_tx_header_api))),
    //         )
    //         .await;

    //         let id: String = tx.id.as_bytes().to_base58();
    //         let req = test::TestRequest::get()
    //             .uri(&format!("/v1/tx/{}", &id))
    //             .to_request();

    //         let resp = test::call_service(&app, req).await;
    //         assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    //         let result: ApiError = test::read_body_json(resp).await;
    //         let tx_error = ApiError::ErrNoId {
    //             id: tx.id.to_string(),
    //             err: String::from("tx not found"),
    //         };
    //         assert_eq!(tx_error, result);
    //         Ok(())
    //     }
}
