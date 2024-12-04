use crate::error::ApiError;
use crate::ApiState;
use actix_web::{
    body::BoxBody,
    http::header::ContentType,
    web::{self, Json},
    HttpRequest, HttpResponse, Responder, ResponseError, Result,
};
use awc::http::StatusCode;
use irys_actors::{
    mempool::{TxIngressError, TxIngressMessage},
    ActorAddresses,
};
use irys_database::database;
use irys_types::{IrysTransactionHeader, H256};
use reth_db::DatabaseEnv;
use serde::{Deserialize, Serialize};
use serde_json;

/// Handles the HTTP POST request for adding a chunk to the mempool.
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
    let tx_ingress_msg = TxIngressMessage { 0: tx };
    let msg_result = state.actors.mempool.send(tx_ingress_msg).await;

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
        };
    }

    // If everything succeeded, return an HTTP 200 OK response
    Ok(HttpResponse::Ok().finish())
}

pub async fn get_tx(
    state: web::Data<ApiState>,
    path: web::Path<H256>,
) -> Result<Json<IrysTransactionHeader>, ApiError> {
    let tx_id: H256 = path.into_inner();
    match database::tx_header_by_txid(&state.db, &tx_id) {
        Result::Err(_error) => Err(ApiError::Internal {
            err: String::from("db error"),
        }),
        Result::Ok(None) => Err(ApiError::ErrNoId {
            id: tx_id.to_string(),
            err: String::from("tx not found"),
        }),
        Result::Ok(Some(tx_header)) => Ok(web::Json(tx_header)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, App, Error};
    use base58::ToBase58;
    use database::open_or_create_db;
    use irys_database::tables::IrysTables;
    use irys_types::app_state::DatabaseProvider;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[actix_web::test]
    async fn test_get_tx() -> Result<(), Error> {
        // TODO: set up testing log environment
        // std::env::set_var("RUST_LOG", "debug");
        // env_logger::init();

        //let path = get_data_dir();
        let path = tempdir().unwrap();
        let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();
        let tx = IrysTransactionHeader::default();
        let result = database::insert_tx_header(&db, &tx);
        assert!(result.is_ok(), "tx can not be stored");

        let tx_get = database::tx_header_by_txid(&db, &tx.id)
            .expect("db error")
            .expect("no tx");
        assert_eq!(tx, tx_get, "retrived another tx");

        let db_arc = Arc::new(db);
        let state = ApiState {
            db: DatabaseProvider(db_arc),
            actors: todo!(),
        };

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .service(web::scope("/v1").route("/tx/{tx_id}", web::get().to(get_tx))),
        )
        .await;

        let id: String = tx.id.as_bytes().to_base58();
        let req = test::TestRequest::get()
            .uri(&format!("/v1/tx/{}", &id))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let result: IrysTransactionHeader = test::read_body_json(resp).await;
        assert_eq!(tx, result);
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
        let state = ApiState {
            db: DatabaseProvider(db_arc),
            actors: todo!(),
        };

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .service(web::scope("/v1").route("/tx/{tx_id}", web::get().to(get_tx))),
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
