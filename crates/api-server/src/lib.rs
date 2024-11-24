mod routes;

use actix_cors::Cors;
use actix_web::{web, App, HttpServer};
use irys_actors::ActorAddresses;
use routes::{chunks, index, price, proxy::proxy, tx};

pub async fn run_server(app_state: ActorAddresses) {
    HttpServer::new(move || {
        let awc_client = awc::Client::new();

        App::new()
            .app_data(web::Data::new(app_state.clone()))
            .app_data(web::Data::new(awc_client))
            .service(
                web::scope("v1")
                    .route("/info", web::get().to(index::info_route))
                    .route("/chunk", web::post().to(chunks::post_chunk))
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
    use irys_database::tables::Tables;
    use reth::tasks::TaskManager;
    use std::sync::Arc;

    use ::irys_database::{config::get_data_dir, open_or_create_db};
    use actix::{Actor, Addr};
    use actix_web::test;
    use awc::http::StatusCode;
    use irys_actors::{
        block_producer::BlockProducerActor, mempool::MempoolActor, packing::PackingActor,
    };
    use irys_types::{chunk, hash_sha256, irys::IrysSigner, Base64, Chunk, H256, MAX_CHUNK_SIZE};
    use tokio::runtime::Handle;

    use rand::Rng;

    let path = get_data_dir();
    let db = open_or_create_db(path, Tables::ALL, None).unwrap();
    let arc_db = Arc::new(db);

    let task_manager = TaskManager::current();

    let mempool_actor = MempoolActor::new(
        irys_types::app_state::DatabaseProvider(arc_db),
        task_manager.executor(),
        IrysSigner::random_signer(),
    );
    let mempool_actor_addr = mempool_actor.start();

    let block_producer_actor = BlockProducerActor {
        db: todo!(),
        mempool_addr: todo!(),
        block_index_addr: todo!(),
        reth_provider: todo!(),
    };
    let block_producer_addr = block_producer_actor.start();

    let mut part_actors = Vec::new();
    // let packing_actor_addr = PackingActor::new(Handle::current()).start();

    let app_state = ActorAddresses {
        partitions: part_actors,
        block_producer: block_producer_addr,
        mempool: mempool_actor_addr,
        block_index: todo!(),
        epoch_service: todo!(),
    };

    // Initialize the app
    let app = test::init_service(
        App::new().app_data(web::Data::new(app_state)).service(
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

    // Call the service
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Loop though each of the transaction chunks
    for (index, chunk_node) in tx.chunks.iter().enumerate() {
        let data_root = tx.header.data_root;
        let data_size = tx.header.data_size;
        let min = chunk_node.min_byte_range;
        let max = chunk_node.max_byte_range;
        let offset = tx.proofs[index].offset as u32;
        let data_path = Base64(tx.proofs[index].proof.to_vec());

        let chunk = Chunk {
            data_root,
            data_size,
            data_path,
            bytes: Base64(data_bytes[min..max].to_vec()),
            offset,
        };

        // Make a POST request with JSON payload
        let req = test::TestRequest::post()
            .uri("/v1/chunk")
            .set_json(&chunk)
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
