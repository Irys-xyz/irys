#![allow(
    clippy::module_name_repetitions,
    reason = "I have no idea how to name this module to satisfy this lint"
)]
use crate::peer_list::{PeerList, ScoreDecreaseReason};
use crate::server_data_handler::GossipServerDataHandler;
use crate::types::{GossipDataRequest, InternalGossipError};
use crate::types::{GossipError, GossipResult};
use actix_web::dev::Server;
use actix_web::{
    middleware,
    web::{self, Data},
    App, HttpResponse, HttpServer,
};
use alloy_rpc_types::engine::ExecutionData;
use base58::ToBase58 as _;
use irys_actors::{block_discovery::BlockDiscoveryFacade, mempool_service::MempoolFacade};
use irys_api_client::ApiClient;
use irys_types::{
    Address, CommitmentTransaction, GossipRequest, IrysBlockHeader, IrysTransactionHeader,
    PeerListItem, UnpackedChunk,
};
use reth::primitives::Block;
use std::net::TcpListener;
use tracing::{debug, error, info, warn};

#[derive(Debug)]
pub(crate) struct GossipServer<M, B, A, P>
where
    M: MempoolFacade,
    B: BlockDiscoveryFacade,
    A: ApiClient,
    P: PeerList,
{
    data_handler: GossipServerDataHandler<M, B, A, P>,
    peer_list: P,
}

impl<M, B, A, P> Clone for GossipServer<M, B, A, P>
where
    M: MempoolFacade,
    B: BlockDiscoveryFacade,
    A: ApiClient,
    P: PeerList,
{
    fn clone(&self) -> Self {
        Self {
            data_handler: self.data_handler.clone(),
            peer_list: self.peer_list.clone(),
        }
    }
}

impl<M, B, A, P> GossipServer<M, B, A, P>
where
    M: MempoolFacade,
    B: BlockDiscoveryFacade,
    A: ApiClient,
    P: PeerList,
{
    pub(crate) const fn new(
        gossip_server_data_handler: GossipServerDataHandler<M, B, A, P>,
        peer_list: P,
    ) -> Self {
        Self {
            data_handler: gossip_server_data_handler,
            peer_list,
        }
    }

    async fn handle_chunk(
        server: Data<Self>,
        unpacked_chunk_json: web::Json<GossipRequest<UnpackedChunk>>,
        req: actix_web::HttpRequest,
    ) -> HttpResponse {
        let gossip_request = unpacked_chunk_json.0;
        let source_miner_address = gossip_request.miner_address;

        match Self::check_peer(&server.peer_list, &req, source_miner_address).await {
            Ok(peer_address) => peer_address,
            Err(error_response) => return error_response,
        };

        if let Err(error) = server.data_handler.handle_chunk(gossip_request).await {
            Self::handle_invalid_data(&source_miner_address, &error, &server.peer_list).await;
            error!("Failed to send chunk: {}", error);
            return HttpResponse::InternalServerError().finish();
        }

        HttpResponse::Ok().finish()
    }

    async fn check_peer(
        peer_list: &P,
        req: &actix_web::HttpRequest,
        miner_address: Address,
    ) -> Result<PeerListItem, HttpResponse> {
        let Some(peer_address) = req.peer_addr() else {
            debug!("Failed to get peer address from gossip post request");
            return Err(HttpResponse::BadRequest().finish());
        };

        match peer_list.peer_by_mining_address(miner_address).await {
            Ok(maybe_peer) => {
                if let Some(peer) = maybe_peer {
                    if peer.address.gossip.ip() != peer_address.ip() {
                        debug!(
                            "Miner address {} request came from ip {}, but the expected ip was {}",
                            miner_address,
                            peer_address.ip(),
                            peer.address.gossip.ip()
                        );
                        return Err(HttpResponse::Forbidden().finish());
                    }
                    Ok(peer)
                } else {
                    warn!("Miner address {} is not allowed", miner_address);
                    Err(HttpResponse::Forbidden().finish())
                }
            }
            Err(error) => {
                error!("Failed to check if miner is allowed: {}", error);
                Err(HttpResponse::InternalServerError().finish())
            }
        }
    }

    async fn handle_block(
        server: Data<Self>,
        irys_block_header_json: web::Json<GossipRequest<IrysBlockHeader>>,
        req: actix_web::HttpRequest,
    ) -> HttpResponse {
        let gossip_request = irys_block_header_json.0;
        let source_miner_address = gossip_request.miner_address;
        let peer =
            match Self::check_peer(&server.peer_list, &req, gossip_request.miner_address).await {
                Ok(peer_address) => peer_address,
                Err(error_response) => return error_response,
            };

        let this_node_id = server.data_handler.gossip_client.mining_address;

        tokio::spawn(async move {
            let block_hash_string = gossip_request.data.block_hash.0.to_base58();
            if let Err(error) = server
                .data_handler
                .handle_block_header_request(gossip_request, peer.address.api)
                .await
            {
                Self::handle_invalid_data(&source_miner_address, &error, &server.peer_list).await;
                error!(
                    "Node {:?}: Failed to process the block {}: {:?}",
                    this_node_id, block_hash_string, error
                );
                // return HttpResponse::InternalServerError().finish();
            } else {
                info!(
                    "Node {:?}: Server handler handled block {}",
                    this_node_id, block_hash_string
                );
            }
        });

        debug!(
            "Node {:?}: Started handling block and returned ok response to the peer",
            this_node_id
        );
        HttpResponse::Ok().finish()
    }

    async fn handle_execution_payload(
        server: Data<Self>,
        irys_execution_payload_json: web::Json<GossipRequest<Block>>,
        req: actix_web::HttpRequest,
    ) -> HttpResponse {
        let evm_block_request = irys_execution_payload_json.0;
        let source_miner_address = evm_block_request.miner_address;

        if let Err(error_response) =
            Self::check_peer(&server.peer_list, &req, evm_block_request.miner_address).await
        {
            return error_response;
        };

        if let Err(error) = server
            .data_handler
            .handle_execution_payload(evm_block_request)
            .await
        {
            Self::handle_invalid_data(&source_miner_address, &error, &server.peer_list).await;
            error!("Failed to send transaction: {}", error);
            return HttpResponse::InternalServerError().finish();
        }

        debug!("Gossip execution payload handled");
        HttpResponse::Ok().finish()
    }

    async fn handle_transaction(
        server: Data<Self>,
        irys_transaction_header_json: web::Json<GossipRequest<IrysTransactionHeader>>,
        req: actix_web::HttpRequest,
    ) -> HttpResponse {
        let gossip_request = irys_transaction_header_json.0;
        let source_miner_address = gossip_request.miner_address;

        match Self::check_peer(&server.peer_list, &req, gossip_request.miner_address).await {
            Ok(peer_address) => peer_address,
            Err(error_response) => return error_response,
        };

        if let Err(error) = server.data_handler.handle_transaction(gossip_request).await {
            Self::handle_invalid_data(&source_miner_address, &error, &server.peer_list).await;
            error!("Failed to send transaction: {}", error);
            return HttpResponse::InternalServerError().finish();
        }

        debug!("Gossip data handled");
        HttpResponse::Ok().finish()
    }

    async fn handle_commitment_tx(
        server: Data<Self>,
        commitment_tx_json: web::Json<GossipRequest<CommitmentTransaction>>,
        req: actix_web::HttpRequest,
    ) -> HttpResponse {
        let gossip_request = commitment_tx_json.0;
        let source_miner_address = gossip_request.miner_address;

        match Self::check_peer(&server.peer_list, &req, gossip_request.miner_address).await {
            Ok(peer_address) => peer_address,
            Err(error_response) => return error_response,
        };

        if let Err(error) = server
            .data_handler
            .handle_commitment_tx(gossip_request)
            .await
        {
            Self::handle_invalid_data(&source_miner_address, &error, &server.peer_list).await;
            error!("Failed to send transaction: {}", error);
            return HttpResponse::InternalServerError().finish();
        }

        debug!("Gossip data handled");
        HttpResponse::Ok().finish()
    }

    async fn handle_health_check(server: Data<Self>, req: actix_web::HttpRequest) -> HttpResponse {
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

    async fn handle_invalid_data(peer_miner_address: &Address, error: &GossipError, peer_list: &P) {
        if let GossipError::InvalidData(_) = error {
            if let Err(error) = peer_list
                .decrease_peer_score(peer_miner_address, ScoreDecreaseReason::BogusData)
                .await
            {
                error!("Failed to decrease peer score: {}", error);
            }
        }
    }

    async fn handle_get_data(
        server: Data<Self>,
        data_request: web::Json<GossipRequest<GossipDataRequest>>,
        req: actix_web::HttpRequest,
    ) -> HttpResponse {
        let peer = match Self::check_peer(&server.peer_list, &req, data_request.miner_address).await
        {
            Ok(peer_address) => peer_address,
            Err(error_response) => return error_response,
        };

        match server
            .data_handler
            .handle_get_data(&peer, data_request.0)
            .await
        {
            Ok(has_data) => HttpResponse::Ok().json(has_data),
            Err(error) => {
                error!("Failed to handle get data request: {}", error);
                HttpResponse::InternalServerError().finish()
            }
        }
    }

    /// Start the gossip server
    ///
    /// # Errors
    ///
    /// If the server fails to bind to the specified address and port, an error is returned.
    pub(crate) fn run(self, listener: TcpListener) -> GossipResult<Server> {
        let node_id = self.data_handler.gossip_client.mining_address;
        debug!("Node {}: Starting the gossip server", node_id);
        let server = self;

        let server_handle = HttpServer::new(move || {
            App::new()
                .app_data(Data::new(server.clone()))
                .wrap(middleware::Logger::default())
                .service(
                    web::scope("/gossip")
                        .route("/transaction", web::post().to(Self::handle_transaction))
                        .route("/commitment_tx", web::post().to(Self::handle_commitment_tx))
                        .route("/chunk", web::post().to(Self::handle_chunk))
                        .route("/block", web::post().to(Self::handle_block))
                        .route(
                            "/execution_payload",
                            web::post().to(Self::handle_execution_payload),
                        )
                        .route("/get_data", web::post().to(Self::handle_get_data))
                        .route("/health", web::get().to(Self::handle_health_check)),
                )
        })
        .shutdown_timeout(5)
        .keep_alive(actix_web::http::KeepAlive::Disabled)
        .listen(listener)
        .map_err(|error| GossipError::Internal(InternalGossipError::Unknown(error.to_string())))?;

        debug!(
            "Node {}: Gossip server listens on {:?}",
            node_id,
            server_handle.addrs()
        );

        Ok(server_handle.run())
    }
}
