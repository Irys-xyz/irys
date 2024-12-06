use crate::error::ApiError;
use crate::ApiState;
use actix_web::{
    web::{self, Json},
    Result,
};
use awc::http::StatusCode;
use irys_database::database;
use irys_types::{IrysBlockHeader, H256};
use reth_db::{Database, DatabaseEnv};

pub async fn get_block(
    state: web::Data<ApiState>,
    path: web::Path<H256>,
) -> Result<Json<IrysBlockHeader>, ApiError> {
    let block_hash: H256 = path.into_inner();
    match state
        .db
        .view_eyre(|tx| database::block_header_by_hash(tx, &block_hash))
    {
        Err(_error) => Err(ApiError::Internal {
            err: String::from("db error"),
        }),
        Ok(None) => Err(ApiError::ErrNoId {
            id: block_hash.to_string(),
            err: String::from("block hash not found"),
        }),
        Ok(Some(tx_header)) => Ok(web::Json(tx_header)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix::Actor;
    use actix_web::{test, App, Error};
    use base58::ToBase58;
    use database::open_or_create_db;
    use irys_actors::mempool::MempoolActor;
    use irys_database::tables::IrysTables;
    use irys_types::{app_state::DatabaseProvider, irys::IrysSigner, StorageConfig};
    use reth::tasks::TaskManager;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[actix_web::test]
    async fn test_get_block() -> Result<(), Error> {
        // TODO: set up testing log environment
        // std::env::set_var("RUST_LOG", "debug");
        // env_logger::init();

        //let path = get_data_dir();
        let path = tempdir().unwrap();
        let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();
        let blk = IrysBlockHeader::default();
        let mut rw_tx = db.tx_mut().unwrap();
        let result = database::insert_block_header(&rw_tx, &blk);
        assert!(result.is_ok(), "block can not be stored");

        let blk_get = database::block_header_by_hash(&rw_tx, &blk.block_hash)
            .expect("db error")
            .expect("no block");
        assert_eq!(blk, blk_get, "retrived another block");

        let db_arc = Arc::new(db);
        let task_manager = TaskManager::current();
        let storage_config = StorageConfig::default();

        let mempool_actor = MempoolActor::new(
            irys_types::app_state::DatabaseProvider(db_arc.clone()),
            task_manager.executor(),
            IrysSigner::random_signer(),
            storage_config,
            Arc::new(Vec::new()).to_vec(),
        );
        let mempool_actor_addr = mempool_actor.start();
        let state = ApiState {
            db: DatabaseProvider(db_arc),
            mempool: mempool_actor_addr,
        };

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .service(web::scope("/v1").route("/block/{block_hash}", web::get().to(get_block))),
        )
        .await;

        let blk_hash: String = blk.block_hash.as_bytes().to_base58();
        let req = test::TestRequest::get()
            .uri(&format!("/v1/block/{}", &blk_hash))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let result: IrysBlockHeader = test::read_body_json(resp).await;
        assert_eq!(blk, result);
        Ok(())
    }

    #[actix_web::test]
    async fn test_get_non_existent_block() -> Result<(), Error> {
        let path = tempdir().unwrap();
        let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();
        let blk = IrysBlockHeader::default();

        let db_arc = Arc::new(db);

        let task_manager = TaskManager::current();
        let storage_config = StorageConfig::default();

        let mempool_actor = MempoolActor::new(
            irys_types::app_state::DatabaseProvider(db_arc.clone()),
            task_manager.executor(),
            IrysSigner::random_signer(),
            storage_config,
            Arc::new(Vec::new()).to_vec(),
        );
        let mempool_actor_addr = mempool_actor.start();
        let state = ApiState {
            db: DatabaseProvider(db_arc),
            mempool: mempool_actor_addr,
        };

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .service(web::scope("/v1").route("/block/{block_hash}", web::get().to(get_block))),
        )
        .await;

        let id: String = blk.block_hash.as_bytes().to_base58();
        let req = test::TestRequest::get()
            .uri(&format!("/v1/block/{}", &id))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let result: ApiError = test::read_body_json(resp).await;
        let blk_error = ApiError::ErrNoId {
            id: blk.block_hash.to_string(),
            err: String::from("block hash not found"),
        };
        assert_eq!(blk_error, result);
        Ok(())
    }
}
