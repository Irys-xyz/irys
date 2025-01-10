pub mod error;
mod routes;
use std::sync::Arc;

use actix::Addr;
use actix_cors::Cors;
use actix_web::{
    dev::HttpServiceFactory,
    error::InternalError,
    web::{self, JsonConfig},
    App, HttpResponse, HttpServer,
};

use irys_actors::mempool::MempoolActor;
use irys_storage::ChunkProvider;
use irys_types::app_state::DatabaseProvider;
use routes::{block, get_chunk, index, network_config, post_chunk, price, proxy::proxy, tx};
use tracing::debug;

#[derive(Clone)]
pub struct ApiState {
    pub mempool: Addr<MempoolActor>,
    pub chunk_provider: Arc<ChunkProvider>,
    pub db: DatabaseProvider,
}

pub fn routes() -> impl HttpServiceFactory {
    web::scope("v1")
        .route("/info", web::get().to(index::info_route))
        .route(
            "/network/config",
            web::get().to(network_config::get_network_config),
        )
        .route("/block/{block_hash}", web::get().to(block::get_block))
        .route(
            "/chunk/data_root/{ledger_num}/{data_root}/{offset}",
            web::get().to(get_chunk::get_chunk_by_data_root_offset),
        )
        .route(
            "/chunk/ledger/{ledger_num}/{ledger_offset}",
            web::get().to(get_chunk::get_chunk_by_ledger_offset),
        )
        .route("/chunk", web::post().to(post_chunk::post_chunk))
        .route("/tx/{tx_id}", web::get().to(tx::get_tx_header_api))
        .route(
            "/tx/{tx_id}/with_start_offsets",
            web::get().to(tx::get_tx_data_metadata),
        )
        .route("/tx", web::post().to(tx::post_tx))
        .route("/price/{size}", web::get().to(price::get_price))
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
                    .error_handler(|err, req| {
                        debug!("JSON decode error for req {:?} - {:?}", &req.path(), &err);
                        InternalError::from_response(err, HttpResponse::BadRequest().finish())
                            .into()
                    }),
            )
            .service(routes())
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
    use irys_types::{irys::IrysSigner, Base64, StorageConfig, UnpackedChunk, MAX_CHUNK_SIZE};

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
        storage_config.clone(),
        Arc::new(Vec::new()).to_vec(),
    );
    let mempool_actor_addr = mempool_actor.start();

    let chunk_provider = ChunkProvider::new(
        storage_config.clone(),
        Arc::new(Vec::new()).to_vec(),
        DatabaseProvider(arc_db.clone()),
    );

    let app_state = ApiState {
        db: DatabaseProvider(arc_db.clone()),
        mempool: mempool_actor_addr,
        chunk_provider: Arc::new(chunk_provider),
    };

    // Initialize the app
    let app = test::init_service(
        App::new()
            .app_data(JsonConfig::default().limit(1024 * 1024)) // 1MB limit
            .app_data(web::Data::new(app_state))
            .wrap(Logger::default())
            .service(routes()),
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
    for (tx_chunk_offset, chunk_node) in tx.chunks.iter().enumerate() {
        let data_root = tx.header.data_root;
        let data_size = tx.header.data_size;
        let min = chunk_node.min_byte_range;
        let max = chunk_node.max_byte_range;
        let data_path = Base64(tx.proofs[tx_chunk_offset].proof.to_vec());

        let chunk = UnpackedChunk {
            data_root,
            data_size,
            data_path,
            bytes: Base64(data_bytes[min..max].to_vec()),
            tx_offset: tx_chunk_offset as u32,
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
