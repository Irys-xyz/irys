use crate::error::ApiError;
use crate::ApiState;
use actix_web::{
    web::{self, Json},
    HttpResponse, Result,
};
use awc::http::StatusCode;
use irys_actors::mempool_service::{TxIngressError, TxIngressMessage};
use irys_database::{database, Ledger};
use irys_types::{u64_stringify, IrysTransactionHeader, H256};
use log::info;
use reth_db::Database;
use serde::{Deserialize, Serialize};

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
        };
    }

    // If everything succeeded, return an HTTP 200 OK response
    Ok(HttpResponse::Ok().finish())
}

pub async fn get_tx_header_api(
    state: web::Data<ApiState>,
    path: web::Path<H256>,
) -> Result<Json<IrysTransactionHeader>, ApiError> {
    let tx_id: H256 = path.into_inner();
    info!("Get tx by tx_id: {}", tx_id);
    get_tx_header(&state, tx_id).map(web::Json)
}

pub fn get_tx_header(
    state: &web::Data<ApiState>,
    tx_id: H256,
) -> Result<IrysTransactionHeader, ApiError> {
    match state
        .db
        .view_eyre(|tx| database::tx_header_by_txid(tx, &tx_id))
    {
        Err(_error) => Err(ApiError::Internal {
            err: String::from("db error"),
        }),
        Ok(None) => Err(ApiError::ErrNoId {
            id: tx_id.to_string(),
            err: String::from("tx not found"),
        }),
        Ok(Some(tx_header)) => Ok(tx_header),
    }
}

pub async fn get_tx_local_start_offset(
    state: web::Data<ApiState>,
    path: web::Path<H256>,
) -> Result<Json<TxOffset>, ApiError> {
    let tx_id: H256 = path.into_inner();
    info!("Get tx data metadata by tx_id: {}", tx_id);
    let tx_header = get_tx_header(&state, tx_id)?;
    let ledger = Ledger::try_from(tx_header.ledger_id).unwrap();

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
    use actix::{Actor, ArbiterService, Registry};
    use actix_web::{middleware::Logger, test, App, Error};
    use base58::ToBase58;
    use database::open_or_create_db;
    use irys_actors::mempool_service::MempoolService;
    use irys_database::tables::IrysTables;
    use irys_storage::ChunkProvider;
    use irys_types::{app_state::DatabaseProvider, irys::IrysSigner, StorageConfig};
    use reth::tasks::TaskManager;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tracing::{error, info};

    #[actix_web::test]
    async fn test_get_tx() -> eyre::Result<()> {
        //std::env::set_var("RUST_LOG", "debug");
        let _ = env_logger::try_init();

        let path = tempdir().unwrap();
        let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();
        let tx_header = IrysTransactionHeader::default();
        info!("Generated tx_id: {}", tx_header.id);

        let _ =
            db.update(|tx| -> eyre::Result<()> { database::insert_tx_header(tx, &tx_header) })?;

        match db.view_eyre(|tx| database::tx_header_by_txid(tx, &tx_header.id))? {
            None => error!("tx not found, test db error!"),
            Some(_tx_header) => info!("tx found!"),
        };

        let arc_db = Arc::new(db);

        let task_manager = TaskManager::current();
        let storage_config = StorageConfig::default();

        let mempool_service = MempoolService::new(
            irys_types::app_state::DatabaseProvider(arc_db.clone()),
            task_manager.executor(),
            IrysSigner::random_signer(),
            storage_config.clone(),
            Arc::new(Vec::new()).to_vec(),
        );
        SystemRegistry::set(mempool_service.start());
        let mempool_addr = MempoolService::from_registry();

        let chunk_provider = ChunkProvider::new(
            storage_config.clone(),
            Arc::new(Vec::new()).to_vec(),
            DatabaseProvider(arc_db.clone()),
        );

        let app_state = ApiState {
            db: DatabaseProvider(arc_db.clone()),
            mempool: mempool_addr,
            chunk_provider: Arc::new(chunk_provider),
        };

        let app = test::init_service(
            App::new()
                .wrap(Logger::default())
                .app_data(web::Data::new(app_state))
                .service(routes()),
        )
        .await;

        let id: String = tx_header.id.as_bytes().to_base58();
        let req = test::TestRequest::get()
            .uri(&format!("/v1/tx/{}", &id))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let result: IrysTransactionHeader = test::read_body_json(resp).await;
        assert_eq!(tx_header, result);
        Ok(())
    }

    #[actix_web::test]
    async fn test_get_non_existent_tx() -> Result<(), Error> {
        // std::env::set_var("RUST_LOG", "debug");
        // env_logger::init();

        let path = tempdir().unwrap();
        let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();
        let tx = IrysTransactionHeader::default();

        let db_arc = Arc::new(db);

        let task_manager = TaskManager::current();
        let storage_config = StorageConfig::default();

        let mempool_service = MempoolService::new(
            irys_types::app_state::DatabaseProvider(db_arc.clone()),
            task_manager.executor(),
            IrysSigner::random_signer(),
            storage_config.clone(),
            Arc::new(Vec::new()).to_vec(),
        );
        SystemRegistry::set(mempool_service.start());
        let mempool_addr = MempoolService::from_registry();

        let chunk_provider = ChunkProvider::new(
            storage_config.clone(),
            Arc::new(Vec::new()).to_vec(),
            DatabaseProvider(db_arc.clone()),
        );

        let app_state = ApiState {
            db: DatabaseProvider(db_arc.clone()),
            mempool: mempool_addr,
            chunk_provider: Arc::new(chunk_provider),
        };

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(app_state))
                .service(web::scope("/v1").route("/tx/{tx_id}", web::get().to(get_tx_header_api))),
        )
        .await;

        let id: String = tx.id.as_bytes().to_base58();
        let req = test::TestRequest::get()
            .uri(&format!("/v1/tx/{}", &id))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let result: ApiError = test::read_body_json(resp).await;
        let tx_error = ApiError::ErrNoId {
            id: tx.id.to_string(),
            err: String::from("tx not found"),
        };
        assert_eq!(tx_error, result);
        Ok(())
    }
}
