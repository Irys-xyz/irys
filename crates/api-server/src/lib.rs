mod error;
mod routes;

use actix::Addr;
use actix_cors::Cors;
use actix_web::{
    error::InternalError,
    web::{self, JsonConfig},
    App, HttpResponse, HttpServer,
};

use irys_actors::mempool::MempoolActor;
use irys_types::app_state::DatabaseProvider;
use routes::{block, chunks, index, price, proxy::proxy, tx};

#[derive(Clone)]
pub struct ApiState {
    pub mempool: Addr<MempoolActor>,
    pub db: DatabaseProvider,
}

pub async fn run_server(app_state: ApiState) {
    HttpServer::new(move || {
        let awc_client = awc::Client::new();

        App::new()
            .app_data(web::Data::new(app_state.clone()))
            .app_data(web::Data::new(awc_client))
            .app_data(
                JsonConfig::default()
                    .limit(1024 * 1024) // Set JSON payload limit to 1MB
                    .error_handler(|err, _req| {
                        InternalError::from_response(err, HttpResponse::BadRequest().finish())
                            .into()
                    }),
            )
            .service(
                web::scope("v1")
                    .route("/info", web::get().to(index::info_route))
                    .route("/block/{block_hash}", web::get().to(block::get_block))
                    .route("/chunk", web::post().to(chunks::post_chunk))
                    .route("/tx/{tx_id}", web::get().to(tx::get_tx))
                    .route("/tx", web::post().to(tx::post_tx))
                    .route("/price/{size}", web::get().to(price::get_price)),
            )
            .route("/", web::to(proxy))
            .wrap(Cors::permissive())
    })
    .bind(("0.0.0.0", 8080))
    .unwrap()
    .run()
    .await
    .unwrap();
}

//==============================================================================
// Tests
//------------------------------------------------------------------------------
#[cfg(test)]
#[actix_web::test]
async fn post_tx_and_chunks_golden_path() {
    use irys_database::tables::IrysTables;
    use reth::tasks::TaskManager;
    use std::sync::Arc;

    std::env::set_var("RUST_LOG", "trace");

    use ::irys_database::{config::get_data_dir, open_or_create_db};
    use actix::Actor;
    use actix_web::{middleware::Logger, test};
    use awc::http::StatusCode;
    use irys_actors::mempool::MempoolActor;
    use irys_types::{irys::IrysSigner, Base64, UnpackedChunk, StorageConfig, MAX_CHUNK_SIZE};

    use rand::Rng;

    let path = get_data_dir();
    let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();
    let arc_db = Arc::new(db);

    let task_manager = TaskManager::current();
    let storage_config = StorageConfig::default();

    // TODO Fixup this test, maybe with some stubs
    let mempool_actor = MempoolActor::new(
        irys_types::app_state::DatabaseProvider(arc_db.clone()),
        task_manager.executor(),
        IrysSigner::random_signer(),
        storage_config,
        Arc::new(Vec::new()).to_vec(),
    );
    let mempool_actor_addr = mempool_actor.start();

    let app_state = ApiState {
        db: DatabaseProvider(arc_db.clone()),
        mempool: mempool_actor_addr,
    };

    // Initialize the app
    let app = test::init_service(
        App::new()
            .app_data(JsonConfig::default().limit(1024 * 1024)) // 1MB limit
            .app_data(web::Data::new(app_state))
            .wrap(Logger::default())
            .service(
                web::scope("v1")
                    .route("/tx", web::post().to(tx::post_tx))
                    .route("/chunk", web::post().to(chunks::post_chunk)),
            ),
    )
    .await;

    // Create 2.5 chunks worth of data *  fill the data with random bytes
    let data_size = (MAX_CHUNK_SIZE as f64 * 2.5).round() as usize;
    let mut data_bytes = vec![0u8; data_size];
    rand::thread_rng().fill(&mut data_bytes[..]);

    // Create a new Irys API instance & a signed transaction
    let irys = IrysSigner::random_signer();
    let tx = irys.create_transaction(data_bytes.clone(), None).unwrap();
    let tx = irys.sign_transaction(tx).unwrap();

    // Make a POST request with JSON payload
    let req = test::TestRequest::post()
        .uri("/v1/tx")
        .set_json(&tx.header)
        .to_request();

    println!("{}", serde_json::to_string_pretty(&tx.header).unwrap());

    // Call the service
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Loop though each of the transaction chunks
    for (index, chunk_node) in tx.chunks.iter().enumerate() {
        let data_root = tx.header.data_root;
        let data_size = tx.header.data_size;
        let min = chunk_node.min_byte_range;
        let max = chunk_node.max_byte_range;
        let data_path = Base64(tx.proofs[index].proof.to_vec());

        let chunk = UnpackedChunk {
            data_root,
            data_size,
            data_path,
            bytes: Base64(data_bytes[min..max].to_vec()),
            chunk_index: index as u32,
        };

        // Make a POST request with JSON payload
        let req = test::TestRequest::post()
            .uri("/v1/chunk")
            .set_json(&chunk)
            .to_request();

        let resp = test::call_service(&app, req).await;
        // println!("{:#?}", resp.into_body());
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
