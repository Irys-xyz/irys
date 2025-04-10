#![allow(
    clippy::module_name_repetitions,
    reason = "I have no idea how to name this module to satisfy this lint"
)]
use crate::server_data_handler::GossipServerDataHandler;
use crate::types::InternalGossipError;
use crate::types::{GossipError, GossipResult};
use actix::{Actor, Addr, Context, Handler};
use actix_web::dev::Server;
use actix_web::{
    middleware,
    web::{self, Data},
    App, HttpResponse, HttpServer,
};
use irys_actors::block_discovery::BlockDiscoveredMessage;
use irys_actors::mempool_service::{ChunkIngressMessage, TxExistenceQuery, TxIngressMessage};
use irys_actors::peer_list_service::{DecreasePeerScore, PeerListEntryRequest, PeerListService};
use irys_api_client::ApiClient;
use irys_types::{IrysBlockHeader, IrysTransactionHeader, PeerListItem, UnpackedChunk};

#[derive(Debug)]
pub struct GossipServer<M, B, A>
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
    A: ApiClient + Clone + 'static,
{
    data_handler: GossipServerDataHandler<M, B, A>,
    peer_list: Addr<PeerListService>,
}

impl<M, B, A> Clone for GossipServer<M, B, A>
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
    A: ApiClient + Clone + 'static,
{
    fn clone(&self) -> Self {
        Self {
            data_handler: self.data_handler.clone(),
            peer_list: self.peer_list.clone(),
        }
    }
}

impl<M, B, A> GossipServer<M, B, A>
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
    A: ApiClient + Clone + 'static,
{
    pub const fn new(
        gossip_server_data_handler: GossipServerDataHandler<M, B, A>,
        peer_list: Addr<PeerListService>,
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
                            web::post().to(handle_transaction::<M, B, A>),
                        )
                        .route("/chunk", web::post().to(handle_chunk::<M, B, A>))
                        .route("/block", web::post().to(handle_block::<M, B, A>))
                        .route("/health", web::get().to(handle_health_check::<M, B, A>)),
                )
        })
        .shutdown_timeout(5)
        .keep_alive(actix_web::http::KeepAlive::Disabled)
        .bind((bind_address, port))
        .map_err(|error| GossipError::Internal(InternalGossipError::Unknown(error.to_string())))?
        .run())
    }
}

async fn check_peer(
    peer_service_addr: &Addr<PeerListService>,
    req: &actix_web::HttpRequest,
) -> Result<PeerListItem, HttpResponse> {
    let Some(peer_address) = req.peer_addr() else {
        tracing::debug!("Failed to get peer address from gossip post request");
        return Err(HttpResponse::BadRequest().finish());
    };

    match peer_service_addr
        .send(PeerListEntryRequest::GossipSocketAddress(peer_address))
        .await
    {
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

async fn handle_block<M, B, A>(
    server: Data<GossipServer<M, B, A>>,
    irys_block_header_json: web::Json<IrysBlockHeader>,
    req: actix_web::HttpRequest,
) -> HttpResponse
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
    A: ApiClient + Clone,
{
    tracing::debug!("Gossip data received: {:?}", irys_block_header_json);
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

async fn handle_transaction<M, B, A>(
    server: Data<GossipServer<M, B, A>>,
    irys_transaction_header_json: web::Json<IrysTransactionHeader>,
    req: actix_web::HttpRequest,
) -> HttpResponse
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
    A: ApiClient + Clone,
{
    tracing::debug!("Gossip data received: {:?}", irys_transaction_header_json);
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

async fn handle_chunk<M, B, A>(
    server: Data<GossipServer<M, B, A>>,
    unpacked_chunk_json: web::Json<UnpackedChunk>,
    req: actix_web::HttpRequest,
) -> HttpResponse
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
    A: ApiClient + Clone,
{
    tracing::debug!("Gossip data received: {:?}", unpacked_chunk_json);
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

async fn handle_health_check<M, B, A>(
    server: Data<GossipServer<M, B, A>>,
    req: actix_web::HttpRequest,
) -> HttpResponse
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
    A: ApiClient + Clone,
{
    let Some(peer_addr) = req.peer_addr() else {
        return HttpResponse::BadRequest().finish();
    };

    match server
        .peer_list
        .send(PeerListEntryRequest::GossipSocketAddress(peer_addr))
        .await
    {
        Ok(info) => match info {
            Some(info) => HttpResponse::Ok().json(info),
            None => HttpResponse::NotFound().finish(),
        },
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}

async fn handle_invalid_data(
    peer: &mut PeerListItem,
    error: &GossipError,
    peer_list_service: &Addr<PeerListService>,
) {
    if let GossipError::InvalidData(_) = error {
        if let Err(error) = peer_list_service
            .send(DecreasePeerScore {
                peer: peer.address.gossip,
                reason: irys_actors::peer_list_service::ScoreDecreaseReason::BogusData,
            })
            .await
        {
            tracing::error!("Failed to decrease peer score: {}", error);
        }
    }
}
