#![allow(
    clippy::module_name_repetitions,
    reason = "I have no idea how to name this module to satisfy this lint"
)]
use crate::{
    server_data_handler::GossipServerDataHandler,
    types::{GossipError, GossipResult, InternalGossipError},
    BlockPoolError,
};
use actix_web::{
    dev::Server,
    middleware,
    web::{self, Data},
    App, HttpResponse, HttpServer,
};
use irys_actors::{block_discovery::BlockDiscoveryFacade, mempool_service::MempoolFacade};
use irys_api_client::ApiClient;
use irys_domain::{PeerList, ScoreDecreaseReason};
use irys_types::{
    Address, CommitmentTransaction, DataTransactionHeader, GossipDataRequest, GossipRequest,
    IngressProof, IrysBlockHeader, PeerListItem, UnpackedChunk,
};
use reth::{builder::Block as _, primitives::Block};
use std::net::TcpListener;
use tracing::{debug, error, info, warn};

#[derive(Debug)]
pub(crate) struct GossipServer<M, B, A>
where
    M: MempoolFacade,
    B: BlockDiscoveryFacade,
    A: ApiClient,
{
    data_handler: GossipServerDataHandler<M, B, A>,
    peer_list: PeerList,
}

impl<M, B, A> Clone for GossipServer<M, B, A>
where
    M: MempoolFacade,
    B: BlockDiscoveryFacade,
    A: ApiClient,
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
    M: MempoolFacade,
    B: BlockDiscoveryFacade,
    A: ApiClient,
{
    pub(crate) const fn new(
        gossip_server_data_handler: GossipServerDataHandler<M, B, A>,
        peer_list: PeerList,
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
        if !server.data_handler.sync_state.is_gossip_reception_enabled() {
            let node_id = server.data_handler.gossip_client.mining_address;
            let chunk_hash = unpacked_chunk_json.0.data.chunk_path_hash();
            warn!(
                "Node {}: Gossip reception is disabled, ignoring chunk {:?}",
                node_id, chunk_hash
            );
            return HttpResponse::Forbidden().finish();
        }
        let gossip_request = unpacked_chunk_json.0;
        let source_miner_address = gossip_request.miner_address;

        match Self::check_peer(&server.peer_list, &req, source_miner_address) {
            Ok(peer_address) => peer_address,
            Err(error_response) => return error_response,
        };

        if let Err(error) = server.data_handler.handle_chunk(gossip_request).await {
            Self::handle_invalid_data(&source_miner_address, &error, &server.peer_list);
            error!("Failed to send chunk: {}", error);
            return HttpResponse::InternalServerError().finish();
        }

        HttpResponse::Ok().finish()
    }

    fn check_peer(
        peer_list: &PeerList,
        req: &actix_web::HttpRequest,
        miner_address: Address,
    ) -> Result<PeerListItem, HttpResponse> {
        let Some(peer_address) = req.peer_addr() else {
            debug!("Failed to get peer address from gossip post request");
            return Err(HttpResponse::BadRequest().finish());
        };

        if let Some(peer) = peer_list.peer_by_mining_address(&miner_address) {
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

    #[expect(
        clippy::unused_async,
        reason = "Actix-web handler signature requires handlers to be async"
    )]
    async fn handle_block(
        server: Data<Self>,
        irys_block_header_json: web::Json<GossipRequest<IrysBlockHeader>>,
        req: actix_web::HttpRequest,
    ) -> HttpResponse {
        if !server.data_handler.sync_state.is_gossip_reception_enabled() {
            let node_id = server.data_handler.gossip_client.mining_address;
            let block_hash = irys_block_header_json.0.data.block_hash;
            warn!(
                "Node {}: Gossip reception is disabled, ignoring block header {:?}",
                node_id, block_hash
            );
            return HttpResponse::Forbidden().finish();
        }
        let gossip_request = irys_block_header_json.0;
        let source_miner_address = gossip_request.miner_address;
        let Some(source_socket_addr) = req.peer_addr() else {
            return HttpResponse::BadRequest().finish();
        };

        let peer = match Self::check_peer(&server.peer_list, &req, gossip_request.miner_address) {
            Ok(peer_address) => peer_address,
            Err(error_response) => return error_response,
        };

        let this_node_id = server.data_handler.gossip_client.mining_address;

        tokio::spawn(async move {
            let block_hash_string = gossip_request.data.block_hash;
            if let Err(error) = server
                .data_handler
                .handle_block_header_request(gossip_request, peer.address.api, source_socket_addr)
                .await
            {
                Self::handle_invalid_data(&source_miner_address, &error, &server.peer_list);
                error!(
                    "Node {:?}: Failed to process the block {:?}: {:?}",
                    this_node_id, block_hash_string, error
                );
                // return HttpResponse::InternalServerError().finish();
            } else {
                info!(
                    "Node {:?}: Server handler handled block {:?}",
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
        if !server.data_handler.sync_state.is_gossip_reception_enabled() {
            let node_id = server.data_handler.gossip_client.mining_address;
            let evm_block_hash = irys_execution_payload_json.0.data.seal_slow().hash();
            warn!(
                "Node {}: Gossip reception is disabled, ignoring the execution payload for block {:?}",
                node_id, evm_block_hash
            );
            return HttpResponse::Forbidden().finish();
        }
        let evm_block_request = irys_execution_payload_json.0;
        let source_miner_address = evm_block_request.miner_address;

        if let Err(error_response) =
            Self::check_peer(&server.peer_list, &req, evm_block_request.miner_address)
        {
            return error_response;
        };

        if let Err(error) = server
            .data_handler
            .handle_execution_payload(evm_block_request)
            .await
        {
            Self::handle_invalid_data(&source_miner_address, &error, &server.peer_list);
            error!("Failed to send transaction: {}", error);
            return HttpResponse::InternalServerError().finish();
        }

        debug!("Gossip execution payload handled");
        HttpResponse::Ok().finish()
    }

    async fn handle_transaction(
        server: Data<Self>,
        irys_transaction_header_json: web::Json<GossipRequest<DataTransactionHeader>>,
        req: actix_web::HttpRequest,
    ) -> HttpResponse {
        if !server.data_handler.sync_state.is_gossip_reception_enabled() {
            let node_id = server.data_handler.gossip_client.mining_address;
            let tx_id = irys_transaction_header_json.0.data.id;
            warn!(
                "Node {}: Gossip reception is disabled, ignoring transaction {:?}",
                node_id, tx_id
            );
            return HttpResponse::Forbidden().finish();
        }
        let gossip_request = irys_transaction_header_json.0;
        let source_miner_address = gossip_request.miner_address;

        match Self::check_peer(&server.peer_list, &req, gossip_request.miner_address) {
            Ok(peer_address) => peer_address,
            Err(error_response) => return error_response,
        };

        if let Err(error) = server.data_handler.handle_transaction(gossip_request).await {
            Self::handle_invalid_data(&source_miner_address, &error, &server.peer_list);
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
        if !server.data_handler.sync_state.is_gossip_reception_enabled() {
            let node_id = server.data_handler.gossip_client.mining_address;
            let tx_id = commitment_tx_json.0.data.id;
            warn!(
                "Node {}: Gossip reception is disabled, ignoring the commitment transaction {:?}",
                node_id, tx_id
            );
            return HttpResponse::Forbidden().finish();
        }
        let gossip_request = commitment_tx_json.0;
        let source_miner_address = gossip_request.miner_address;

        match Self::check_peer(&server.peer_list, &req, gossip_request.miner_address) {
            Ok(peer_address) => peer_address,
            Err(error_response) => return error_response,
        };

        if let Err(error) = server
            .data_handler
            .handle_commitment_tx(gossip_request)
            .await
        {
            Self::handle_invalid_data(&source_miner_address, &error, &server.peer_list);
            error!("Failed to send transaction: {}", error);
            return HttpResponse::InternalServerError().finish();
        }

        debug!("Gossip data handled");
        HttpResponse::Ok().finish()
    }

    async fn handle_ingress_proof(
        server: Data<Self>,
        proof_json: web::Json<GossipRequest<IngressProof>>,
        req: actix_web::HttpRequest,
    ) -> HttpResponse {
        if !server.data_handler.sync_state.is_gossip_reception_enabled() {
            let node_id = server.data_handler.gossip_client.mining_address;
            let data_root = proof_json.0.data.data_root;
            warn!(
                "Node {}: Gossip reception is disabled, ignoring the ingress proof for data_root: {:?}",
                node_id, data_root
            );
            return HttpResponse::Forbidden().finish();
        }
        let gossip_request = proof_json.0;
        let source_miner_address = gossip_request.miner_address;

        match Self::check_peer(&server.peer_list, &req, gossip_request.miner_address) {
            Ok(peer_address) => peer_address,
            Err(error_response) => return error_response,
        };

        if let Err(error) = server
            .data_handler
            .handle_ingress_proof(gossip_request)
            .await
        {
            Self::handle_invalid_data(&source_miner_address, &error, &server.peer_list);
            error!("Failed to send ingress proof: {}", error);
            return HttpResponse::InternalServerError().finish();
        }

        debug!("Gossip data handled");
        HttpResponse::Ok().finish()
    }

    #[expect(
        clippy::unused_async,
        reason = "Actix-web handler signature requires handlers to be async"
    )]
    async fn handle_health_check(server: Data<Self>, req: actix_web::HttpRequest) -> HttpResponse {
        let Some(peer_addr) = req.peer_addr() else {
            return HttpResponse::BadRequest().finish();
        };

        match server.peer_list.peer_by_gossip_address(peer_addr) {
            Some(_info) => HttpResponse::Ok().json(true),
            None => HttpResponse::NotFound().finish(),
        }
    }

    fn handle_invalid_data(
        peer_miner_address: &Address,
        error: &GossipError,
        peer_list: &PeerList,
    ) {
        match error {
            GossipError::InvalidData(_) => {
                peer_list.decrease_peer_score(peer_miner_address, ScoreDecreaseReason::BogusData);
            }
            GossipError::BlockPool(BlockPoolError::BlockError(_)) => {
                peer_list.decrease_peer_score(peer_miner_address, ScoreDecreaseReason::BogusData);
            }
            _ => {}
        };
    }

    async fn handle_get_data(
        server: Data<Self>,
        data_request: web::Json<GossipRequest<GossipDataRequest>>,
        req: actix_web::HttpRequest,
    ) -> HttpResponse {
        if !server.data_handler.sync_state.is_gossip_reception_enabled()
            || !server.data_handler.sync_state.is_gossip_broadcast_enabled()
        {
            let node_id = server.data_handler.gossip_client.mining_address;
            let request_id = match &data_request.0.data {
                GossipDataRequest::Block(hash) => format!("block {:?}", hash),
                GossipDataRequest::ExecutionPayload(hash) => {
                    format!("execution payload for block {:?}", hash)
                }
                GossipDataRequest::Chunk(chunk_path_hash) => {
                    format!("chunk {:?}", chunk_path_hash)
                }
            };
            warn!(
                "Node {}: Gossip reception/broadcast is disabled, ignoring the get data request for {}",
                node_id, request_id
            );
            return HttpResponse::Forbidden().finish();
        }
        let peer = match Self::check_peer(&server.peer_list, &req, data_request.miner_address) {
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
                .app_data(web::JsonConfig::default().limit(100 * 1024 * 1024))
                .wrap(middleware::Logger::default()) // TODO: use tracing_actix_web TracingLogger
                .service(
                    web::scope("/gossip")
                        .route("/transaction", web::post().to(Self::handle_transaction))
                        .route("/commitment_tx", web::post().to(Self::handle_commitment_tx))
                        .route("/chunk", web::post().to(Self::handle_chunk))
                        .route("/block", web::post().to(Self::handle_block))
                        .route("/ingress_proof", web::post().to(Self::handle_ingress_proof))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::util::{ApiClientStub, BlockDiscoveryStub, MempoolStub};
    use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{Config, DatabaseProvider, NodeConfig, PeerNetworkSender, PeerScore};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    #[actix_rt::test]
    // test that handle_invalid_data subtracts from peerscore in the case of GossipError::BlockPool(BlockPoolError::BlockError(_)))
    async fn handle_invalid_block_penalizes_peer() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let node_config = NodeConfig::testing();
        let config = Config::new(node_config);
        let db_env =
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf()).expect("db");
        let db = DatabaseProvider(Arc::new(db_env));
        let (tx, _rx) = mpsc::unbounded_channel();
        let peer_network_sender = PeerNetworkSender::new(tx);
        let peer_list = PeerList::new(&config, &db, peer_network_sender).expect("peer list");

        let miner = Address::new([1_u8; 20]);
        peer_list.add_or_update_peer(miner, PeerListItem::default(), true);

        let error = GossipError::BlockPool(BlockPoolError::BlockError("bad".into()));
        GossipServer::<MempoolStub, BlockDiscoveryStub, ApiClientStub>::handle_invalid_data(
            &miner, &error, &peer_list,
        );

        let peer = peer_list.peer_by_mining_address(&miner).unwrap();
        assert_eq!(peer.reputation_score.get(), PeerScore::INITIAL - 5);
    }
}
