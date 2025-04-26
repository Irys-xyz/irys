#![allow(
    clippy::module_name_repetitions,
    reason = "I have no idea how to name this module to satisfy this lint"
)]
use crate::server_data_handler::GossipServerDataHandler;
use crate::types::{GossipDataRequest, InternalGossipError};
use crate::types::{GossipError, GossipResult};
use actix::{Actor, Context, Handler};
use actix_web::dev::Server;
use actix_web::{
    middleware,
    web::{self, Data},
    App, HttpResponse, HttpServer,
};
use irys_actors::block_discovery::BlockDiscoveredMessage;
use irys_actors::mempool_service::{ChunkIngressMessage, TxExistenceQuery, TxIngressMessage};
use irys_actors::peer_list_service::{PeerListFacade, ScoreDecreaseReason};
use irys_api_client::ApiClient;
use irys_types::{
    IrysBlockHeader, IrysTransactionHeader, PeerListItem, RethPeerInfo, UnpackedChunk,
};

#[derive(Debug)]
pub struct GossipServer<M, B, A, R>
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
    A: ApiClient + Clone + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    data_handler: GossipServerDataHandler<M, B, A, R>,
    peer_list: PeerListFacade<A, R>,
}

impl<M, B, A, R> Clone for GossipServer<M, B, A, R>
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
    A: ApiClient + Clone + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    fn clone(&self) -> Self {
        Self {
            data_handler: self.data_handler.clone(),
            peer_list: self.peer_list.clone(),
        }
    }
}

impl<M, B, A, R> GossipServer<M, B, A, R>
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
    A: ApiClient + Clone + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    pub const fn new(
        gossip_server_data_handler: GossipServerDataHandler<M, B, A, R>,
        peer_list: PeerListFacade<A, R>,
    ) -> Self {
        Self {
            data_handler: gossip_server_data_handler,
            peer_list,
        }
    }

    /// Start the gossip server
    ///
    /// # Errors
    ///
    /// If the server fails to bind to the specified address and port, an error is returned.
    pub fn run(self, bind_address: &str, port: u16) -> GossipResult<Server> {
        let server = self;

        Ok(HttpServer::new(move || {
            App::new()
                .app_data(Data::new(server.clone()))
                .wrap(middleware::Logger::default())
                .service(
                    web::scope("/gossip")
                        .route(
                            "/transaction",
                            web::post().to(handle_transaction::<M, B, A, R>),
                        )
                        .route("/chunk", web::post().to(handle_chunk::<M, B, A, R>))
                        .route("/block", web::post().to(handle_block::<M, B, A, R>))
                        .route("/get_data", web::post().to(handle_get_data::<M, B, A, R>))
                        .route("/health", web::get().to(handle_health_check::<M, B, A, R>)),
                )
        })
        .shutdown_timeout(5)
        .keep_alive(actix_web::http::KeepAlive::Disabled)
        .bind((bind_address, port))
        .map_err(|error| GossipError::Internal(InternalGossipError::Unknown(error.to_string())))?
        .run())
    }
}

async fn check_peer<R, A>(
    peer_service_addr: &PeerListFacade<A, R>,
    req: &actix_web::HttpRequest,
) -> Result<PeerListItem, HttpResponse>
where
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    A: ApiClient + Clone + 'static + Unpin + Default,
{
    let Some(peer_address) = req.peer_addr() else {
        tracing::debug!("Failed to get peer address from gossip post request");
        return Err(HttpResponse::BadRequest().finish());
    };

    match peer_service_addr.peer_by_gossip_address(peer_address).await {
        Ok(maybe_peer) => {
            if let Some(peer) = maybe_peer {
                Ok(peer)
            } else {
                tracing::debug!("Peer address is not allowed");
                Err(HttpResponse::Forbidden().finish())
            }
        }
        Err(error) => {
            tracing::error!("Failed to check if peer is allowed: {}", error);
            Err(HttpResponse::InternalServerError().finish())
        }
    }
}

async fn handle_block<M, B, A, R>(
    server: Data<GossipServer<M, B, A, R>>,
    irys_block_header_json: web::Json<IrysBlockHeader>,
    req: actix_web::HttpRequest,
) -> HttpResponse
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
    A: ApiClient + Clone + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    let mut peer = match check_peer(&server.peer_list, &req).await {
        Ok(peer_address) => peer_address,
        Err(error_response) => return error_response,
    };

    let irys_block_header = irys_block_header_json.0;
    if let Err(error) = server
        .data_handler
        .handle_block_header(irys_block_header, peer.address.gossip, peer.address.api)
        .await
    {
        handle_invalid_data(&mut peer, &error, &server.peer_list).await;
        tracing::error!("Failed to send block: {}", error);
        return HttpResponse::InternalServerError().finish();
    }

    HttpResponse::Ok().finish()
}

async fn handle_transaction<M, B, A, R>(
    server: Data<GossipServer<M, B, A, R>>,
    irys_transaction_header_json: web::Json<IrysTransactionHeader>,
    req: actix_web::HttpRequest,
) -> HttpResponse
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
    A: ApiClient + Clone + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    let mut peer = match check_peer(&server.peer_list, &req).await {
        Ok(peer_address) => peer_address,
        Err(error_response) => return error_response,
    };

    let irys_transaction_header = irys_transaction_header_json.0;
    if let Err(error) = server
        .data_handler
        .handle_transaction(irys_transaction_header, peer.address.gossip)
        .await
    {
        handle_invalid_data(&mut peer, &error, &server.peer_list).await;
        tracing::error!("Failed to send transaction: {}", error);
        return HttpResponse::InternalServerError().finish();
    }

    tracing::debug!("Gossip data handled");
    HttpResponse::Ok().finish()
}

async fn handle_chunk<M, B, A, R>(
    server: Data<GossipServer<M, B, A, R>>,
    unpacked_chunk_json: web::Json<UnpackedChunk>,
    req: actix_web::HttpRequest,
) -> HttpResponse
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
    A: ApiClient + Clone + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    let mut peer = match check_peer(&server.peer_list, &req).await {
        Ok(peer_address) => peer_address,
        Err(error_response) => return error_response,
    };

    let unpacked_chunk = unpacked_chunk_json.0;
    if let Err(error) = server
        .data_handler
        .handle_chunk(unpacked_chunk, peer.address.gossip)
        .await
    {
        handle_invalid_data(&mut peer, &error, &server.peer_list).await;
        tracing::error!("Failed to send chunk: {}", error);
        return HttpResponse::InternalServerError().finish();
    }

    HttpResponse::Ok().finish()
}

async fn handle_health_check<M, B, A, R>(
    server: Data<GossipServer<M, B, A, R>>,
    req: actix_web::HttpRequest,
) -> HttpResponse
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
    A: ApiClient + Clone + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    let Some(peer_addr) = req.peer_addr() else {
        return HttpResponse::BadRequest().finish();
    };

    match server.peer_list.peer_by_gossip_address(peer_addr).await {
        Ok(info) => match info {
            Some(_info) => HttpResponse::Ok().json(true),
            None => HttpResponse::NotFound().finish(),
        },
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}

async fn handle_invalid_data<R, A>(
    peer: &mut PeerListItem,
    error: &GossipError,
    peer_list_service: &PeerListFacade<A, R>,
) where
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    A: ApiClient + Clone + 'static + Unpin + Default,
{
    if let GossipError::InvalidData(_) = error {
        if let Err(error) = peer_list_service
            .decrease_peer_score(peer, ScoreDecreaseReason::BogusData)
            .await
        {
            tracing::error!("Failed to decrease peer score: {}", error);
        }
    }
}

async fn handle_get_data<M, B, A, R>(
    server: Data<GossipServer<M, B, A, R>>,
    data_request: web::Json<GossipDataRequest>,
    req: actix_web::HttpRequest,
) -> HttpResponse
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
    A: ApiClient + Clone + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    let Some(source_addr) = req.peer_addr() else {
        return HttpResponse::BadRequest().finish();
    };

    match server
        .data_handler
        .handle_get_data(source_addr, data_request.0)
        .await
    {
        Ok(has_data) => HttpResponse::Ok().json(has_data),
        Err(error) => {
            tracing::error!("Failed to handle get data request: {}", error);
            HttpResponse::InternalServerError().finish()
        }
    }
}
