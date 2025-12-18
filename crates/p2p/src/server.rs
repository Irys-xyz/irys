#![allow(
    clippy::module_name_repetitions,
    reason = "I have no idea how to name this module to satisfy this lint"
)]
use crate::block_pool::CriticalBlockPoolError;
use crate::types::{GossipResponse, RejectionReason};
use crate::{
    gossip_data_handler::GossipDataHandler,
    types::{GossipError, GossipResult, InternalGossipError},
};
use actix_web::{
    dev::Server,
    http::header::ContentType,
    web::{self, Data},
    App, HttpResponse, HttpServer,
};
use irys_actors::{block_discovery::BlockDiscoveryFacade, mempool_service::MempoolFacade};
use irys_domain::{get_node_info, PeerList, ScoreDecreaseReason};
use irys_types::{
    parse_user_agent, AcceptedResponse, BlockBody, BlockIndexQuery, CommitmentTransaction,
    DataTransactionHeader, GossipDataRequestV2, GossipRequest, IngressProof, IrysAddress,
    IrysBlockHeader, PeerListItem, ProtocolVersion, UnpackedChunk, VersionRequest,
};
use rand::prelude::SliceRandom as _;
use reth::{builder::Block as _, primitives::Block};
use semver::Version;
use std::net::{IpAddr, TcpListener};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, warn, Instrument as _};
use tracing_actix_web::TracingLogger;

/// Default deduplication window in milliseconds for data requests
/// Prevents rapid duplicate requests within this time window
const DEFAULT_DUPLICATE_REQUEST_MILLISECONDS: u128 = 10_000; // 10 seconds

#[derive(Debug)]
pub(crate) struct GossipServer<M, B>
where
    M: MempoolFacade,
    B: BlockDiscoveryFacade,
{
    data_handler: Arc<GossipDataHandler<M, B>>,
    peer_list: PeerList,
}

impl<M, B> Clone for GossipServer<M, B>
where
    M: MempoolFacade,
    B: BlockDiscoveryFacade,
{
    fn clone(&self) -> Self {
        Self {
            data_handler: self.data_handler.clone(),
            peer_list: self.peer_list.clone(),
        }
    }
}

impl<M, B> GossipServer<M, B>
where
    M: MempoolFacade,
    B: BlockDiscoveryFacade,
{
    pub(crate) const fn new(
        gossip_server_data_handler: Arc<GossipDataHandler<M, B>>,
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
            return HttpResponse::Ok().json(GossipResponse::<()>::Rejected(
                RejectionReason::GossipDisabled,
            ));
        }
        let gossip_request = unpacked_chunk_json.0;
        let source_miner_address = gossip_request.miner_address;

        match Self::check_peer(&server.peer_list, &req, source_miner_address) {
            Ok(peer_address) => peer_address,
            Err(error_response) => return error_response,
        };
        server.peer_list.set_is_online(&source_miner_address, true);

        if let Err(error) = server.data_handler.handle_chunk(gossip_request).await {
            Self::handle_invalid_data(&source_miner_address, &error, &server.peer_list);
            error!("Failed to send chunk: {}", error);
            return HttpResponse::Ok()
                .json(GossipResponse::<()>::Rejected(RejectionReason::InvalidData));
        }

        HttpResponse::Ok().json(GossipResponse::Accepted(()))
    }

    fn check_peer(
        peer_list: &PeerList,
        req: &actix_web::HttpRequest,
        miner_address: IrysAddress,
    ) -> Result<PeerListItem, HttpResponse> {
        let Some(peer_address) = req.peer_addr() else {
            warn!("Failed to get peer address from gossip POST request");
            return Err(HttpResponse::Ok().json(GossipResponse::<()>::Rejected(
                RejectionReason::UnableToVerifyOrigin,
            )));
        };

        if let Some(peer) = peer_list.peer_by_mining_address(&miner_address) {
            if peer.address.gossip.ip() != peer_address.ip() {
                return Err(HttpResponse::Ok().json(GossipResponse::<()>::Rejected(
                    RejectionReason::HandshakeRequired,
                )));
            }
            Ok(peer)
        } else {
            Err(HttpResponse::Ok().json(GossipResponse::<()>::Rejected(
                RejectionReason::HandshakeRequired,
            )))
        }
    }

    #[tracing::instrument(skip_all)]
    async fn handle_block_header(
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
            return HttpResponse::Ok().json(GossipResponse::<()>::Rejected(
                RejectionReason::GossipDisabled,
            ));
        }
        let gossip_request = irys_block_header_json.0;
        let source_miner_address = gossip_request.miner_address;
        let Some(source_socket_addr) = req.peer_addr() else {
            return HttpResponse::Ok().json(GossipResponse::<()>::Rejected(
                RejectionReason::UnableToVerifyOrigin,
            ));
        };

        if let Err(error_response) =
            Self::check_peer(&server.peer_list, &req, gossip_request.miner_address)
        {
            return error_response;
        };
        server.peer_list.set_is_online(&source_miner_address, true);

        let this_node_id = server.data_handler.gossip_client.mining_address;
        let block_hash = gossip_request.data.block_hash;
        let block_height = gossip_request.data.height;
        if gossip_request.data.poa.chunk.is_none() {
            error!(
                target = "p2p::server",
                block.hash = ?gossip_request.data.block_hash,
                "received a block without a POA chunk"
            );
        }

        tokio::spawn(
            async move {
                let block_hash_string = gossip_request.data.block_hash;
                if let Err(error) = server
                    .data_handler
                    .handle_block_header(gossip_request, source_socket_addr)
                    .in_current_span()
                    .await
                {
                    Self::handle_invalid_data(&source_miner_address, &error, &server.peer_list);
                    if !error.is_advisory() {
                        error!(
                            "Node {:?}: Failed to process the block {} height {}: {:?}",
                            this_node_id, block_hash_string, block_height, error
                        );
                    }
                } else {
                    info!(
                        "Node {:?}: Server handler handled block {} height {}",
                        this_node_id, block_hash_string, block_height
                    );
                }
            }
            .in_current_span(),
        );

        debug!(
            "Node {:?}: Started handling block {} height {} and returned ok response to the peer",
            this_node_id, block_hash, block_height
        );
        HttpResponse::Ok().json(GossipResponse::Accepted(()))
    }

    #[tracing::instrument(skip_all)]
    async fn handle_block_body(
        server: Data<Self>,
        irys_block_header_json: web::Json<GossipRequest<BlockBody>>,
        req: actix_web::HttpRequest,
    ) -> HttpResponse {
        // TODO: this implementation is incomplete!
        if !server.data_handler.sync_state.is_gossip_reception_enabled() {
            let node_id = server.data_handler.gossip_client.mining_address;
            let block_hash = irys_block_header_json.0.data.block_hash;
            warn!(
                "Node {}: Gossip reception is disabled, ignoring block header {:?}",
                node_id, block_hash
            );
            return HttpResponse::Ok().json(GossipResponse::<()>::Rejected(
                RejectionReason::GossipDisabled,
            ));
        }
        let gossip_request = irys_block_header_json.0;
        let source_miner_address = gossip_request.miner_address;
        let Some(source_socket_addr) = req.peer_addr() else {
            return HttpResponse::Ok().json(GossipResponse::<()>::Rejected(
                RejectionReason::UnableToVerifyOrigin,
            ));
        };

        if let Err(error_response) =
            Self::check_peer(&server.peer_list, &req, gossip_request.miner_address)
        {
            return error_response;
        };
        server.peer_list.set_is_online(&source_miner_address, true);

        let this_node_id = server.data_handler.gossip_client.mining_address;
        let block_hash = gossip_request.data.block_hash;

        let handler = server.data_handler.clone();

        tokio::spawn(
            async move {
                if let Err(e) = handler
                    .handle_block_body(gossip_request, source_socket_addr)
                    .await
                {
                    Self::handle_invalid_data(&source_miner_address, &e, &server.peer_list);
                    error!(
                        "Node {:?}: Failed to process the block body {}: {:?}",
                        this_node_id, block_hash, e
                    );
                } else {
                    info!(
                        "Node {:?}: Server handler handled block body {}",
                        this_node_id, block_hash
                    );
                }
            }
            .in_current_span(),
        );

        debug!(
            "Node {:?}: Started handling block body {} and returned ok response to the peer",
            this_node_id, block_hash
        );
        HttpResponse::Ok().json(GossipResponse::Accepted(()))
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
            return HttpResponse::Ok().json(GossipResponse::<()>::Rejected(
                RejectionReason::GossipDisabled,
            ));
        }
        let evm_block_request = irys_execution_payload_json.0;
        let source_miner_address = evm_block_request.miner_address;

        if let Err(error_response) =
            Self::check_peer(&server.peer_list, &req, evm_block_request.miner_address)
        {
            return error_response;
        };
        server.peer_list.set_is_online(&source_miner_address, true);

        if let Err(error) = server
            .data_handler
            .handle_execution_payload(evm_block_request)
            .await
        {
            Self::handle_invalid_data(&source_miner_address, &error, &server.peer_list);
            error!("Failed to send transaction: {}", error);
            return HttpResponse::Ok()
                .json(GossipResponse::<()>::Rejected(RejectionReason::InvalidData));
        }

        debug!("Gossip execution payload handled");
        HttpResponse::Ok().json(GossipResponse::Accepted(()))
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
            return HttpResponse::Ok().json(GossipResponse::<()>::Rejected(
                RejectionReason::GossipDisabled,
            ));
        }
        let gossip_request = irys_transaction_header_json.0;
        let source_miner_address = gossip_request.miner_address;

        match Self::check_peer(&server.peer_list, &req, gossip_request.miner_address) {
            Ok(peer_address) => peer_address,
            Err(error_response) => return error_response,
        };
        server.peer_list.set_is_online(&source_miner_address, true);

        if let Err(error) = server.data_handler.handle_transaction(gossip_request).await {
            Self::handle_invalid_data(&source_miner_address, &error, &server.peer_list);
            error!("Failed to send transaction: {}", error);
            return HttpResponse::Ok()
                .json(GossipResponse::<()>::Rejected(RejectionReason::InvalidData));
        }

        debug!("Gossip data handled");
        HttpResponse::Ok().json(GossipResponse::Accepted(()))
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
            return HttpResponse::Ok().json(GossipResponse::<()>::Rejected(
                RejectionReason::GossipDisabled,
            ));
        }
        let gossip_request = commitment_tx_json.0;
        let source_miner_address = gossip_request.miner_address;

        match Self::check_peer(&server.peer_list, &req, gossip_request.miner_address) {
            Ok(peer_address) => peer_address,
            Err(error_response) => return error_response,
        };
        server.peer_list.set_is_online(&source_miner_address, true);

        if let Err(error) = server
            .data_handler
            .handle_commitment_tx(gossip_request)
            .await
        {
            Self::handle_invalid_data(&source_miner_address, &error, &server.peer_list);
            error!("Failed to send transaction: {}", error);
            return HttpResponse::Ok()
                .json(GossipResponse::<()>::Rejected(RejectionReason::InvalidData));
        }

        debug!("Gossip data handled");
        HttpResponse::Ok().json(GossipResponse::Accepted(()))
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
            return HttpResponse::Ok().json(GossipResponse::<()>::Rejected(
                RejectionReason::GossipDisabled,
            ));
        }
        let gossip_request = proof_json.0;
        let source_miner_address = gossip_request.miner_address;

        match Self::check_peer(&server.peer_list, &req, gossip_request.miner_address) {
            Ok(peer_address) => peer_address,
            Err(error_response) => return error_response,
        };
        server.peer_list.set_is_online(&source_miner_address, true);

        if let Err(error) = server
            .data_handler
            .handle_ingress_proof(gossip_request)
            .await
        {
            Self::handle_invalid_data(&source_miner_address, &error, &server.peer_list);
            error!("Failed to send ingress proof: {}", error);
            return HttpResponse::Ok()
                .json(GossipResponse::<()>::Rejected(RejectionReason::InvalidData));
        }

        debug!("Gossip data handled");
        HttpResponse::Ok().json(GossipResponse::Accepted(()))
    }

    #[expect(
        clippy::unused_async,
        reason = "Actix-web handler signature requires handlers to be async"
    )]
    async fn handle_health_check(server: Data<Self>, req: actix_web::HttpRequest) -> HttpResponse {
        let Some(peer_addr) = req.peer_addr() else {
            return HttpResponse::Ok().json(GossipResponse::<()>::Rejected(
                RejectionReason::UnableToVerifyOrigin,
            ));
        };

        match server.peer_list.peer_by_gossip_address(peer_addr) {
            Some(_info) => {
                let sync_state = &server.data_handler.sync_state;
                let is_gossip_enabled = sync_state.is_gossip_reception_enabled()
                    && sync_state.is_gossip_broadcast_enabled();
                if is_gossip_enabled {
                    HttpResponse::Ok().json(GossipResponse::Accepted(is_gossip_enabled))
                } else {
                    debug!("Rejecting health check from peer {peer_addr:?}: gossip is disabled");
                    HttpResponse::Ok().json(GossipResponse::rejected_gossip_disabled())
                }
            }
            None => HttpResponse::Ok().json(GossipResponse::<()>::Rejected(
                RejectionReason::HandshakeRequired,
            )),
        }
    }

    async fn handle_stake_and_pledge_whitelist(server: Data<Self>) -> HttpResponse {
        let whitelist = server
            .data_handler
            .handle_get_stake_and_pledge_whitelist()
            .await;
        HttpResponse::Ok().json(GossipResponse::Accepted(whitelist))
    }

    async fn handle_info(server: Data<Self>) -> HttpResponse {
        let block_index = &server.data_handler.block_index;
        let block_tree = &server.data_handler.block_tree;
        let peer_list = &server.peer_list;
        let sync_state = &server.data_handler.sync_state;
        let started_at = server.data_handler.started_at;
        let mining_address = server.data_handler.gossip_client.mining_address;
        let chain_id = server.data_handler.config.consensus.chain_id;

        let node_info = get_node_info(
            block_index,
            block_tree,
            peer_list,
            sync_state,
            started_at,
            mining_address,
            chain_id,
        )
        .await;

        HttpResponse::Ok()
            .content_type(ContentType::json())
            .json(GossipResponse::Accepted(node_info))
    }

    #[expect(
        clippy::unused_async,
        reason = "Actix-web handler signature requires handlers to be async"
    )]
    async fn handle_peer_list(server: Data<Self>) -> HttpResponse {
        let ips = server.peer_list.all_known_peers();
        HttpResponse::Ok()
            .content_type(ContentType::json())
            .json(GossipResponse::Accepted(ips))
    }

    #[expect(
        clippy::unused_async,
        reason = "Actix-web handler signature requires handlers to be async"
    )]
    async fn handle_version(
        server: Data<Self>,
        req: actix_web::HttpRequest,
        body: web::Json<VersionRequest>,
    ) -> HttpResponse {
        let connection_info = req.connection_info();
        let Some(source_addr_str) = connection_info.peer_addr() else {
            return HttpResponse::Ok().json(GossipResponse::<AcceptedResponse>::Rejected(
                RejectionReason::InvalidCredentials,
            ));
        };
        let Ok(source_addr) = source_addr_str.parse::<IpAddr>() else {
            return HttpResponse::Ok().json(GossipResponse::<AcceptedResponse>::Rejected(
                RejectionReason::InvalidCredentials,
            ));
        };

        let version_request = body.into_inner();

        if source_addr != version_request.address.gossip.ip() {
            return HttpResponse::Ok().json(GossipResponse::<AcceptedResponse>::Rejected(
                RejectionReason::InvalidCredentials,
            ));
        }

        if version_request.protocol_version != ProtocolVersion::V1 {
            return HttpResponse::Ok().json(GossipResponse::<AcceptedResponse>::Rejected(
                RejectionReason::ProtocolMismatch,
            ));
        }

        if !version_request.verify_signature() {
            return HttpResponse::Ok().json(GossipResponse::<AcceptedResponse>::Rejected(
                RejectionReason::InvalidCredentials,
            ));
        }

        let mut peers = server.peer_list.all_known_peers();
        peers.shuffle(&mut rand::thread_rng());
        let cap = server
            .data_handler
            .config
            .node_config
            .p2p_handshake
            .server_peer_list_cap;
        if peers.len() > cap {
            peers.truncate(cap);
        }

        let peer_address = version_request.address;
        let mining_addr = version_request.mining_address;
        let peer_list_entry = PeerListItem {
            address: peer_address,
            ..Default::default()
        };

        let is_staked = server
            .data_handler
            .block_tree
            .read()
            .canonical_epoch_snapshot()
            .is_staked(mining_addr);
        server
            .peer_list
            .add_or_update_peer(mining_addr, peer_list_entry, is_staked);

        let node_name = version_request
            .user_agent
            .and_then(|ua| parse_user_agent(&ua))
            .map(|(name, _, _, _)| name)
            .unwrap_or_default();

        let response = AcceptedResponse {
            version: Version::new(1, 2, 0),
            protocol_version: ProtocolVersion::V1,
            peers,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            message: Some(format!("Welcome to the network {node_name}")),
        };

        HttpResponse::Ok()
            .content_type(ContentType::json())
            .json(GossipResponse::Accepted(response))
    }

    #[expect(
        clippy::unused_async,
        reason = "Actix-web handler signature requires handlers to be async"
    )]
    async fn handle_block_index(
        server: Data<Self>,
        query: web::Query<BlockIndexQuery>,
    ) -> HttpResponse {
        const MAX_BLOCK_INDEX_QUERY_LIMIT: usize = 1_000;
        const DEFAULT_BLOCK_INDEX_QUERY_LIMIT: usize = 100;

        let limit = if query.limit == 0 {
            DEFAULT_BLOCK_INDEX_QUERY_LIMIT
        } else {
            query.limit
        };
        if limit > MAX_BLOCK_INDEX_QUERY_LIMIT {
            return HttpResponse::Ok()
                .json(GossipResponse::<()>::Rejected(RejectionReason::InvalidData));
        }
        let height = query.height;

        let requested_blocks = server
            .data_handler
            .block_index
            .read()
            .get_range(height as u64, limit);

        HttpResponse::Ok().json(GossipResponse::Accepted(requested_blocks))
    }

    #[expect(
        clippy::unused_async,
        reason = "Actix-web handler signature requires handlers to be async"
    )]
    async fn handle_protocol_version() -> HttpResponse {
        HttpResponse::Ok().json(crate::gossip_client::GossipClient::CURRENT_PROTOCOL_VERSION)
    }

    fn handle_invalid_data(
        peer_miner_address: &IrysAddress,
        error: &GossipError,
        peer_list: &PeerList,
    ) {
        match error {
            GossipError::InvalidData(invalid_data_error) => {
                peer_list.decrease_peer_score(
                    peer_miner_address,
                    ScoreDecreaseReason::BogusData(format!(
                        "Invalid data: {:?}",
                        invalid_data_error
                    )),
                );
            }
            GossipError::BlockPool(CriticalBlockPoolError::BlockError(msg)) => {
                peer_list.decrease_peer_score(
                    peer_miner_address,
                    ScoreDecreaseReason::BogusData(format!("Block pool error: {:?}", msg)),
                );
            }
            _ => {}
        };
    }

    #[tracing::instrument(skip_all)]
    async fn handle_data_request(
        server: Data<Self>,
        data_request: web::Json<GossipRequest<GossipDataRequestV2>>,
        req: actix_web::HttpRequest,
    ) -> HttpResponse {
        if !server.data_handler.sync_state.is_gossip_reception_enabled()
            || !server.data_handler.sync_state.is_gossip_broadcast_enabled()
        {
            let node_id = server.data_handler.gossip_client.mining_address;
            let request_id = match &data_request.0.data {
                GossipDataRequestV2::BlockHeader(hash) => format!("block header {:?}", hash),
                GossipDataRequestV2::ExecutionPayload(hash) => {
                    format!("execution payload for block {:?}", hash)
                }
                GossipDataRequestV2::Chunk(chunk_path_hash) => {
                    format!("chunk {:?}", chunk_path_hash)
                }
                GossipDataRequestV2::BlockBody(hash) => format!("block body {:?}", hash),
                GossipDataRequestV2::Transaction(hash) => format!("transaction {:?}", hash),
            };
            warn!(
                "Node {}: Gossip reception/broadcast is disabled, ignoring the get data request for {}",
                node_id, request_id
            );
            return HttpResponse::Ok().json(GossipResponse::<()>::Rejected(
                RejectionReason::GossipDisabled,
            ));
        }
        let peer = match Self::check_peer(&server.peer_list, &req, data_request.miner_address) {
            Ok(peer_address) => peer_address,
            Err(error_response) => return error_response,
        };

        match server
            .data_handler
            .handle_get_data(
                &peer,
                data_request.0,
                DEFAULT_DUPLICATE_REQUEST_MILLISECONDS,
            )
            .await
        {
            Ok(has_data) => HttpResponse::Ok().json(GossipResponse::Accepted(has_data)),
            Err(GossipError::RateLimited) => {
                debug!("Rate limited data request from peer");
                HttpResponse::Ok()
                    .json(GossipResponse::<()>::Rejected(RejectionReason::RateLimited))
            }
            Err(error) => {
                error!("Failed to handle get data request: {}", error);
                HttpResponse::Ok()
                    .json(GossipResponse::<()>::Rejected(RejectionReason::InvalidData))
            }
        }
    }

    #[tracing::instrument(skip_all)]
    async fn handle_pull_data(
        server: Data<Self>,
        data_request: web::Json<GossipRequest<GossipDataRequestV2>>,
        req: actix_web::HttpRequest,
    ) -> HttpResponse {
        if let Err(error_response) =
            Self::check_peer(&server.peer_list, &req, data_request.miner_address)
        {
            return error_response;
        };

        match server
            .data_handler
            .handle_get_data_sync(data_request.0)
            .in_current_span()
            .await
        {
            Ok(maybe_data) => HttpResponse::Ok().json(GossipResponse::Accepted(maybe_data)),
            Err(error) => {
                error!("Failed to handle get data request: {}", error);
                HttpResponse::Ok()
                    .json(GossipResponse::<()>::Rejected(RejectionReason::InvalidData))
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
            let span = tracing::info_span!(target: "irys-api-gossip", "gossip_server");
            let _guard = span.enter();

            App::new()
                .app_data(Data::new(server.clone()))
                .app_data(web::JsonConfig::default().limit(100 * 1024 * 1024))
                .wrap(TracingLogger::default())
                .service(
                    web::scope("/gossip")
                        .route("/transaction", web::post().to(Self::handle_transaction))
                        .route("/commitment_tx", web::post().to(Self::handle_commitment_tx))
                        .route("/chunk", web::post().to(Self::handle_chunk))
                        .route("/block", web::post().to(Self::handle_block_header))
                        .route("/block_body", web::post().to(Self::handle_block_body))
                        .route("/ingress_proof", web::post().to(Self::handle_ingress_proof))
                        .route(
                            "/execution_payload",
                            web::post().to(Self::handle_execution_payload),
                        )
                        .route("/get_data", web::post().to(Self::handle_data_request))
                        .route("/pull_data", web::post().to(Self::handle_pull_data))
                        .route("/health", web::get().to(Self::handle_health_check))
                        .route(
                            "/stake_and_pledge_whitelist",
                            web::get().to(Self::handle_stake_and_pledge_whitelist),
                        )
                        .route("/info", web::get().to(Self::handle_info))
                        .route("/peer-list", web::get().to(Self::handle_peer_list))
                        .route("/version", web::post().to(Self::handle_version))
                        .route("/block-index", web::get().to(Self::handle_block_index))
                        .route(
                            "/protocol_version",
                            web::get().to(Self::handle_protocol_version),
                        ),
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
    use crate::tests::util::{BlockDiscoveryStub, MempoolStub};
    use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{Config, DatabaseProvider, NodeConfig, PeerNetworkSender, PeerScore};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    #[tokio::test]
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
        let peer_list = PeerList::new(
            &config,
            &db,
            peer_network_sender,
            tokio::sync::broadcast::channel::<irys_domain::PeerEvent>(100).0,
        )
        .expect("peer list");

        let miner = IrysAddress::new([1_u8; 20]);
        peer_list.add_or_update_peer(miner, PeerListItem::default(), true);

        let error = GossipError::BlockPool(CriticalBlockPoolError::BlockError("bad".into()));
        GossipServer::<MempoolStub, BlockDiscoveryStub>::handle_invalid_data(
            &miner, &error, &peer_list,
        );

        let peer = peer_list.peer_by_mining_address(&miner).unwrap();
        assert_eq!(peer.reputation_score.get(), PeerScore::INITIAL - 5);
    }
}
