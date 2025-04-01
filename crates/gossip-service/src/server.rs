use crate::server_data_handler::GossipServerDataHandler;
use crate::types::InternalGossipError;
use crate::{
    types::{GossipError, GossipResult},
    PeerListProvider,
};
use actix::{Actor, Context, Handler};
use actix_web::dev::Server;
use actix_web::{
    middleware,
    web::{self, Data},
    App, HttpResponse, HttpServer,
};
use core::net::SocketAddr;
use irys_actors::mempool_service::{ChunkIngressMessage, TxExistenceQuery, TxIngressMessage};
use irys_api_client::ApiClient;
use irys_types::{IrysBlockHeader, IrysTransactionHeader, UnpackedChunk};
use std::sync::Arc;

#[derive(Debug)]
pub struct GossipServer<M, A>
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    A: ApiClient + 'static,
{
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
    pub const fn new(
        gossip_server_data_handler: GossipServerDataHandler<M, A>,
        peer_list: PeerListProvider,
    ) -> Self {
        Self {
            data_handler: gossip_server_data_handler,
            peer_list,
        }
    }

    pub fn run(self, bind_address: &str, port: u16) -> GossipResult<Server> {
        let server = Arc::new(self);

        Ok(HttpServer::new(move || {
            App::new()
                .app_data(Data::new(Arc::clone(&server)))
                .wrap(middleware::Logger::default())
                .service(
                    web::scope("/gossip")
                        .route("/transaction", web::post().to(handle_transaction::<M, A>))
                        .route("/chunk", web::post().to(handle_chunk::<M, A>))
                        .route("/block", web::post().to(handle_block::<M, A>))
                        .route("/health", web::get().to(handle_health_check::<M, A>)),
                )
        })
        .bind((bind_address, port))
        .map_err(|error| GossipError::Internal(InternalGossipError::Unknown(error.to_string())))?
        .run())
    }
}

fn check_peer(
    peer_list: &PeerListProvider,
    req: &actix_web::HttpRequest,
) -> Result<SocketAddr, HttpResponse> {
    let Some(peer_address) = req.peer_addr() else {
        tracing::debug!("Failed to get peer address from gossip post request");
        return Err(HttpResponse::BadRequest().finish());
    };

    match peer_list.is_peer_allowed(&peer_address) {
        Ok(is_peer_allowed) => {
            if is_peer_allowed {
                Ok(peer_address)
            } else {
                tracing::debug!("Peer address is not allowed");
                Err(HttpResponse::Forbidden().finish())
            }
        }
        Err(e) => {
            tracing::error!("Failed to check if peer is allowed: {}", e);
            Err(HttpResponse::InternalServerError().finish())
        }
    }
}

async fn handle_block<M, A>(
    server: Data<Arc<GossipServer<M, A>>>,
    irys_block_header_json: web::Json<IrysBlockHeader>,
    req: actix_web::HttpRequest,
) -> HttpResponse
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    A: ApiClient,
{
    tracing::debug!("Gossip data received: {:?}", irys_block_header_json);
    let peer_address = match check_peer(&server.peer_list, &req) {
        Ok(peer_address) => peer_address,
        Err(error_response) => return error_response,
    };

    let irys_block_header = irys_block_header_json.0;
    if let Err(e) = server
        .data_handler
        .handle_block_header(irys_block_header, peer_address)
        .await
    {
        tracing::error!("Failed to send block: {}", e);
        return HttpResponse::InternalServerError().finish();
    }

    HttpResponse::Ok().finish()
}

async fn handle_transaction<M, A>(
    server: Data<Arc<GossipServer<M, A>>>,
    irys_transaction_header_json: web::Json<IrysTransactionHeader>,
    req: actix_web::HttpRequest,
) -> HttpResponse
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    A: ApiClient,
{
    tracing::debug!("Gossip data received: {:?}", irys_transaction_header_json);
    let peer_address = match check_peer(&server.peer_list, &req) {
        Ok(peer_address) => peer_address,
        Err(error_response) => return error_response,
    };

    let irys_transaction_header = irys_transaction_header_json.0;
    if let Err(e) = server
        .data_handler
        .handle_transaction(irys_transaction_header, peer_address)
        .await
    {
        tracing::error!("Failed to send transaction: {}", e);
        return HttpResponse::InternalServerError().finish();
    }

    tracing::debug!("Gossip data handled");
    HttpResponse::Ok().finish()
}

async fn handle_chunk<M, A>(
    server: Data<Arc<GossipServer<M, A>>>,
    unpacked_chunk_json: web::Json<UnpackedChunk>,
    req: actix_web::HttpRequest,
) -> HttpResponse
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    A: ApiClient,
{
    tracing::debug!("Gossip data received: {:?}", unpacked_chunk_json);
    let peer_address = match check_peer(&server.peer_list, &req) {
        Ok(peer_address) => peer_address,
        Err(error_response) => return error_response,
    };

    let unpacked_chunk = unpacked_chunk_json.0;
    if let Err(e) = server
        .data_handler
        .handle_chunk(unpacked_chunk, peer_address)
        .await
    {
        tracing::error!("Failed to send chunk: {}", e);
        return HttpResponse::InternalServerError().finish();
    }

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
    let Some(peer_addr) = req.peer_addr() else {
        return HttpResponse::BadRequest().finish();
    };

    match server.peer_list.get_peer_info(&peer_addr) {
        Ok(info) => match info {
            Some(info) => HttpResponse::Ok().json(info),
            None => HttpResponse::NotFound().finish(),
        },
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}
