pub mod error;
pub mod metrics;
pub mod routes;

use actix_cors::Cors;
use actix_web::{
    dev::{HttpServiceFactory, Server},
    error::InternalError,
    web::{self, JsonConfig, Redirect},
    App, HttpResponse, HttpServer,
};
use irys_actors::{
    chunk_ingress_service::ChunkIngressMessage, mempool_guard::MempoolReadGuard,
    mempool_service::MempoolServiceMessage, pledge_provider::MempoolPledgeProvider,
};
use irys_domain::chain_sync_state::ChainSyncState;
use irys_domain::{
    BlockIndexReadGuard, BlockTreeReadGuard, ChunkProvider, PeerList, SupplyStateReadGuard,
};
use irys_reth_node_bridge::node::RethNodeProvider;
use irys_types::{app_state::DatabaseProvider, Config, IrysAddress, PeerAddress, Traced};
use routes::{
    balance, block, block_index, block_tree, commitment, config, get_chunk, index, ledger, mempool,
    mining, pd_pricing, peer_list, post_chunk, price, proxy::proxy, storage, tx,
};
use std::{
    net::{SocketAddr, TcpListener},
    sync::Arc,
    time::Instant,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};
use tracing_actix_web::TracingLogger;

use crate::routes::anchor;

/// API version prefix for all routes
pub const API_VERSION: &str = "v1";

#[derive(Clone)]
pub struct ApiState {
    pub mempool_service: UnboundedSender<Traced<MempoolServiceMessage>>,
    pub chunk_ingress: UnboundedSender<Traced<ChunkIngressMessage>>,
    pub mempool_guard: MempoolReadGuard,
    pub chunk_provider: Arc<ChunkProvider>,
    pub peer_list: PeerList,
    pub db: DatabaseProvider,
    pub config: Config,
    // TODO: slim this down to what we actually use - beware the types!
    pub reth_provider: RethNodeProvider,
    pub reth_http_url: String,
    pub block_tree: BlockTreeReadGuard,
    pub block_index: BlockIndexReadGuard,
    pub supply_state: Option<SupplyStateReadGuard>,
    pub sync_state: ChainSyncState,
    pub mempool_pledge_provider: Arc<MempoolPledgeProvider>,
    pub pd_pricing: Arc<irys_actors::pd_pricing::PdPricing>,
    pub started_at: Instant,
    pub mining_address: IrysAddress,
}

impl ApiState {
    pub fn get_known_peers(&self) -> Vec<PeerAddress> {
        self.peer_list.all_known_peers()
    }
}

pub fn routes() -> impl HttpServiceFactory {
    web::scope(API_VERSION)
        .route("/", web::get().to(index::info_route))
        .route(
            "/block/{block_tag}",
            web::get().to(block::get_block_without_poa),
        )
        .route(
            "/block/{block_tag}/full",
            web::get().to(block::get_block_with_poa),
        )
        .route(
            "/block-index",
            web::get().to(block_index::block_index_route),
        )
        .route(
            "/block-tree/forks",
            web::get().to(block_tree::get_block_tree_forks),
        )
        .route(
            "/commitment-tx",
            web::post().to(commitment::post_commitment_tx),
        )
        .route("/chunk", web::post().to(post_chunk::post_chunk))
        .route(
            "/chunk/data-root/{ledger_id}/{data_root}/{offset}",
            web::get().to(get_chunk::get_chunk_by_data_root_offset),
        )
        .route(
            "/chunk/ledger/{ledger_id}/{ledger_offset}",
            web::get().to(get_chunk::get_chunk_by_ledger_offset),
        )
        .route("/execution-rpc", web::to(proxy))
        .route("/info", web::get().to(index::info_route))
        .route("/genesis", web::get().to(index::genesis_route))
        // TODO: need to fix serialization here
        .route("/network/config", web::get().to(config::get_config))
        .route("/peer-list", web::get().to(peer_list::peer_list_route))
        .route(
            "/price/commitment/stake",
            web::get().to(price::get_stake_price),
        )
        .route(
            "/price/commitment/unstake",
            web::get().to(price::get_unstake_price),
        )
        .route(
            "/price/commitment/pledge/{user_address}",
            web::get().to(price::get_pledge_price),
        )
        .route(
            "/price/commitment/unpledge/{user_address}",
            web::get().to(price::get_unpledge_price),
        )
        .route(
            "/price/pd/fee-history",
            web::get().to(pd_pricing::get_fee_history),
        )
        .route("/price/{ledger}/{size}", web::get().to(price::get_price))
        .route("/tx", web::post().to(tx::post_tx))
        .route("/tx/{tx_id}", web::get().to(tx::get_transaction_api))
        .route("/tx/{tx_id}/status", web::get().to(tx::get_tx_status))
        .route(
            "/tx/{tx_id}/promotion-status",
            web::get().to(tx::get_tx_promotion_status),
        )
        .route(
            "/tx/{tx_id}/local/data-start-offset",
            web::get().to(tx::get_tx_local_start_offset),
        )
        .route("/anchor", web::get().to(anchor::anchor_route))
        .route("/balance/{address}", web::get().to(balance::get_balance))
        // Ledger endpoints
        .route(
            "/ledger/submit/{miner_address}/summary",
            web::get().to(ledger::get_submit_summary),
        )
        .route(
            "/ledger/publish/{miner_address}/summary",
            web::get().to(ledger::get_publish_summary),
        )
        .route(
            "/ledger/submit/{miner_address}/assignments",
            web::get().to(ledger::get_submit_assignments),
        )
        .route(
            "/ledger/publish/{miner_address}/assignments",
            web::get().to(ledger::get_publish_assignments),
        )
        .route(
            "/ledger/{miner_address}/assignments",
            web::get().to(ledger::get_all_assignments),
        )
        // Epoch endpoints
        .route("/epoch/current", web::get().to(ledger::get_current_epoch))
        // Storage endpoints
        .route(
            "/storage/intervals/{ledger}/{slot_index}/{chunk_type}",
            web::get().to(storage::get_intervals),
        )
        .route(
            "/storage/counts/{ledger}/{slot_index}",
            web::get().to(storage::get_chunk_counts),
        )
        .route(
            "/mempool/status",
            web::get().to(mempool::get_mempool_status),
        )
        // Mining endpoint
        .route("/mining/info", web::get().to(mining::get_mining_info))
        // Supply endpoint
        .route("/supply", web::get().to(routes::supply::supply))
}

pub fn run_server(app_state: ApiState, listener: TcpListener) -> Server {
    let port = listener.local_addr().expect("listener to work").port();
    info!(custom.port = ?port, "Starting API server");
    let state = web::Data::new(app_state);
    HttpServer::new(move || {
        let span = tracing::info_span!(target: "irys-api-http", "api_server");
        let _guard = span.enter();

        let awc_client = awc::Client::new();
        App::new()
            .app_data(state.clone())
            .app_data(web::Data::new(awc_client))
            .app_data(
                JsonConfig::default()
                    .limit(1024 * 1024) // Set JSON payload limit to 1MB
                    .error_handler(|err, req| {
                        warn!("JSON decode error for req {} - {}", &req.path(), &err);
                        let error_message = format!("JSON decode/parse error: {}", err);
                        InternalError::from_response(
                            err,
                            HttpResponse::BadRequest().body(error_message),
                        )
                        .into()
                    }),
            )
            // not a permanent redirect, so we can redirect to the highest API version
            .route("/", web::get().to(|| async { Redirect::to("/v1/info") }))
            .service(routes())
            .wrap(Cors::permissive())
            .wrap(TracingLogger::default())
            .wrap(metrics::RequestMetrics)
    })
    .listen(listener)
    .unwrap()
    .run()
}

// Adapted from /actix-web-4.9.0/src/server.rs create_listener
// This is required as we need to access the TcpListener directly to figure out what port we've been assigned
// if randomisation (requested port 0) is used.
pub fn create_listener(addr: SocketAddr) -> eyre::Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let backlog = 1024;
    let domain = Domain::for_address(addr);
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    // need this so application restarts can pick back up the same port without suffering from time-wait
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    // clamp backlog to max u32 that fits in i32 range
    let backlog = core::cmp::min(backlog, i32::MAX as u32) as i32;
    socket.listen(backlog)?;
    let listener = TcpListener::from(socket);
    Ok(listener)
}

//==============================================================================
// Tests
//------------------------------------------------------------------------------
// #[cfg(test)]
// #[tokio::test]
// async fn post_tx_and_chunks_golden_path() {
//     use irys_database::tables::IrysTables;
//     use reth::tasks::TaskManager;
//     use std::sync::Arc;

//     std::env::set_var("RUST_LOG", "trace");

//     use ::irys_database::{config::get_data_dir, open_or_create_db};
//     use actix::{Actor, SystemRegistry, SystemService as _};
//     use actix_web::{middleware::Logger, test};
//     use awc::http::StatusCode;
//     use irys_actors::mempool_service::MempoolService;
//     use irys_types::{irys::IrysSigner, Base64, StorageConfig, UnpackedChunk, MAX_CHUNK_SIZE};

//     use rand::Rng;

//     let path = get_data_dir();
//     let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();
//     let arc_db = Arc::new(db);

//     let task_manager = TaskManager::current();
//     let storage_config = StorageConfig::default();

//     // TODO Fixup this test, maybe with some stubs
//     let mempool_service = MempoolService::new(
//         irys_types::app_state::DatabaseProvider(arc_db.clone()),
//         task_manager.executor(),
//         IrysSigner::random_signer(),
//         storage_config.clone(),
//         Arc::new(Vec::new()).to_vec(),
//     );
//     SystemRegistry::set(mempool_service.start());
//     let mempool_addr = MempoolService::from_registry();

//     let chunk_provider = ChunkProvider::new(
//         storage_config.clone(),
//         Arc::new(Vec::new()).to_vec(),
//         DatabaseProvider(arc_db.clone()),
//     );

//     let app_state = ApiState {
//         db: DatabaseProvider(arc_db.clone()),
//         mempool: mempool_addr,
//         chunk_provider: Arc::new(chunk_provider),
//         reth_provider: None,
//         reth_http_url: None,
//         block_tree: None,
//         block_index: None,
//     };

//     // Initialize the app
//     let app = test::init_service(
//         App::new()
//             .app_data(JsonConfig::default().limit(1024 * 1024)) // 1MB limit
//             .app_data(web::Data::new(app_state))
//             .wrap(Logger::default())
//             .service(routes()),
//     )
//     .await;

//     // Create 2.5 chunks worth of data *  fill the data with random bytes
//     let data_size = (MAX_CHUNK_SIZE as f64 * 2.5).round() as usize;
//     let mut data_bytes = vec![0_u8; data_size];
//     rand::thread_rng().fill(&mut data_bytes[..]);

//     // Create a new Irys API instance & a signed transaction
//     let irys = IrysSigner::random_signer();
//     let tx = irys.create_transaction(data_bytes.clone(), None).unwrap();
//     let tx = irys.sign_transaction(tx).unwrap();

//     // Make a POST request with JSON payload
//     let req = test::TestRequest::post()
//         .uri("/v1/tx")
//         .set_json(&tx.header)
//         .to_request();

//     println!("{}", serde_json::to_string_pretty(&tx.header).unwrap());

//     // Call the service
//     let resp = test::call_service(&app, req).await;
//     assert_eq!(resp.status(), StatusCode::OK);

//     // Loop though each of the transaction chunks
//     for (tx_chunk_offset, chunk_node) in tx.chunks.iter().enumerate() {
//         let data_root = tx.header.data_root;
//         let data_size = tx.header.data_size;
//         let min = chunk_node.min_byte_range;
//         let max = chunk_node.max_byte_range;
//         let data_path = Base64(tx.proofs[tx_chunk_offset].proof.to_vec());

//         let chunk = UnpackedChunk {
//             data_root,
//             data_size,
//             data_path,
//             bytes: Base64(data_bytes[min..max].to_vec()),
//             tx_offset: tx_chunk_offset.try_into().expect("Value exceeds u32::MAX"),
//         };

//         // Make a POST request with JSON payload
//         let req = test::TestRequest::post()
//             .uri("/v1/chunk")
//             .set_json(&chunk)
//             .to_request();

//         let resp = test::call_service(&app, req).await;
//         // println!("{:#?}", resp.into_body());
//         assert_eq!(resp.status(), StatusCode::OK);
//     }
// }
