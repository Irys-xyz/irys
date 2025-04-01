use crate::types::InternalGossipError;
use crate::{
    cache::GossipCache,
    types::{GossipError, GossipResult},
    PeerListProvider,
};
use actix_web::dev::Server;
use actix_web::{
    middleware,
    web::{self, Data},
    App, HttpResponse, HttpServer,
};
use irys_types::GossipData;
use std::sync::Arc;
use actix::{Actor, Context, Handler};
use irys_actors::mempool_service::{ChunkIngressMessage, TxExistenceQuery, TxIngressMessage};
use irys_api_client::ApiClient;
use crate::server_data_handler::GossipServerDataHandler;

#[derive(Debug)]
pub struct GossipServer<M, A>
where
    M: Handler<TxIngressMessage>
    + Handler<ChunkIngressMessage>
    + Handler<TxExistenceQuery>
    + Actor<Context = Context<M>>,
    A: ApiClient + 'static,
{
    cache: Arc<GossipCache>,
    data_handler: GossipServerDataHandler<M, A>,
    peer_list: PeerListProvider,
}

impl<M, A> GossipServer<M, A>
where
    M: Handler<TxIngressMessage>
    + Handler<ChunkIngressMessage>
    + Handler<TxExistenceQuery>
    + Actor<Context = Context<M>>,
    A: ApiClient + 'static,
{
    pub fn new(
        cache: Arc<GossipCache>,
        gossip_server_data_handler: GossipServerDataHandler<M, A>,
        peer_list: PeerListProvider,
    ) -> Self {
        Self {
            cache,
            data_handler: gossip_server_data_handler,
            peer_list,
        }
    }

    pub fn run(self, bind_address: &str, port: u16) -> GossipResult<Server> {
        let server = Arc::new(self);

        Ok(HttpServer::new(move || {
            App::new()
                .app_data(Data::new(server.clone()))
                .wrap(middleware::Logger::default())
                .service(
                    web::scope("/gossip")
                        .route("/data", web::post().to(handle_gossip_data::<M, A>))
                        .route("/health", web::get().to(handle_health_check::<M, A>)),
                )
        })
        .bind((bind_address, port))
        .map_err(|e| GossipError::Internal(InternalGossipError::Unknown(e.to_string())))?
        .run())
    }
}

async fn handle_gossip_data<M, A>(
    server: Data<Arc<GossipServer<M, A>>>,
    data: web::Json<GossipData>,
    req: actix_web::HttpRequest,
) -> HttpResponse
where
    M: Handler<TxIngressMessage>
    + Handler<ChunkIngressMessage>
    + Handler<TxExistenceQuery>
    + Actor<Context = Context<M>>,
    A: ApiClient,
{
    tracing::debug!("Gossip data received: {:?}", data);
    let peer_address = match req.peer_addr() {
        Some(addr) => addr,
        None => {
            tracing::debug!("Failed to get peer address from gossip post request");
            return HttpResponse::BadRequest().finish();
        }
    };

    tracing::debug!("Gossip origin address is {}", peer_address);
    // Check if peer is allowed
    match server.peer_list.is_peer_allowed(&peer_address) {
        Ok(is_peer_allowed) => {
            if !is_peer_allowed {
                tracing::debug!("Peer address is not allowed");
                return HttpResponse::Forbidden().finish();
            }
        }
        Err(e) => {
            tracing::error!("Failed to check if peer is allowed: {}", e);
            return HttpResponse::InternalServerError().finish();
        }
    }

    // Record that this peer has seen this data
    if let Err(e) = server.cache.record_seen(peer_address, &data) {
        tracing::error!("Failed to record data in cache: {}", e);
        return HttpResponse::InternalServerError().finish();
    }

    match data.0 {
        GossipData::Chunk(unpacked_chunk) => {
            tracing::debug!("Gossip data is a chunk");
            if let Err(e) = server
                .data_handler
                .handle_chunk(unpacked_chunk, peer_address)
                .await
            {
                tracing::error!("Failed to send chunk: {}", e);
                return HttpResponse::InternalServerError().finish();
            }
        }
        GossipData::Transaction(irys_transaction_header) => {
            tracing::debug!("Gossip data is a transaction");
            if let Err(e) = server
                .data_handler
                .handle_transaction(irys_transaction_header, peer_address)
                .await
            {
                tracing::error!("Failed to send transaction: {}", e);
                return HttpResponse::InternalServerError().finish();
            }
        }
        GossipData::Block(irys_block_header) => {
            tracing::debug!("Gossip data is a block");
            if let Err(e) = server
                .data_handler
                .handle_block_header(irys_block_header, peer_address)
                .await
            {
                tracing::error!("Failed to send block: {}", e);
                return HttpResponse::InternalServerError().finish();
            }
        }
    }

    tracing::debug!("Gossip data handled");
    HttpResponse::Ok().finish()
}

async fn handle_health_check<M, A>(
    server: Data<Arc<GossipServer<M, A>>>,
    req: actix_web::HttpRequest,
) -> HttpResponse
where
    M: Handler<TxIngressMessage>
    + Handler<ChunkIngressMessage>
    + Handler<TxExistenceQuery>
    + Actor<Context = Context<M>>,
    A: ApiClient,
{
    let peer_addr = match req.peer_addr() {
        Some(addr) => addr,
        None => return HttpResponse::BadRequest().finish(),
    };

    match server.peer_list.get_peer_info(&peer_addr) {
        Ok(info) => match info {
            Some(info) => HttpResponse::Ok().json(info),
            None => HttpResponse::NotFound().finish(),
        },
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}
