use crate::{
    block_pool::BlockPool,
    cache::GossipCache,
    rate_limiting::DataRequestTracker,
    types::{AdvisoryGossipError, InternalGossipError, InvalidDataError},
    GossipClient, GossipError, GossipResult,
};
use core::net::SocketAddr;
use irys_actors::block_discovery::build_block_body_for_processed_block;
use irys_actors::{
    block_discovery::BlockDiscoveryFacade, AdvisoryChunkIngressError, ChunkIngressError,
    CriticalChunkIngressError, MempoolFacade,
};
use irys_api_client::ApiClient;
use irys_domain::chain_sync_state::ChainSyncState;
use irys_domain::{ExecutionPayloadCache, PeerList, ScoreDecreaseReason};
use irys_types::{BlockBody, IrysAddress};
use irys_types::{
    BlockHash, CommitmentTransaction, DataTransactionHeader, EvmBlockHash, GossipCacheKey,
    GossipData, GossipDataRequest, GossipRequest, IngressProof, IrysBlockHeader, PeerListItem,
    UnpackedChunk,
};
use reth::builder::Block as _;
use reth::primitives::Block;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{debug, error, instrument, warn, Instrument as _, Span};

const HEADER_AND_BODY_RETRIES: usize = 3;

/// Handles data received by the `GossipServer`
#[derive(Debug)]
pub struct GossipDataHandler<TMempoolFacade, TBlockDiscovery, TApiClient>
where
    TMempoolFacade: MempoolFacade,
    TBlockDiscovery: BlockDiscoveryFacade,
    TApiClient: ApiClient,
{
    pub mempool: TMempoolFacade,
    pub block_pool: Arc<BlockPool<TBlockDiscovery, TMempoolFacade>>,
    pub(crate) cache: Arc<GossipCache>,
    pub api_client: TApiClient,
    pub gossip_client: GossipClient,
    pub peer_list: PeerList,
    pub sync_state: ChainSyncState,
    /// Tracing span
    pub span: Span,
    pub execution_payload_cache: ExecutionPayloadCache,
    pub data_request_tracker: DataRequestTracker,
}

impl<M, B, A> Clone for GossipDataHandler<M, B, A>
where
    M: MempoolFacade,
    B: BlockDiscoveryFacade,
    A: ApiClient,
{
    fn clone(&self) -> Self {
        Self {
            mempool: self.mempool.clone(),
            block_pool: self.block_pool.clone(),
            cache: Arc::clone(&self.cache),
            api_client: self.api_client.clone(),
            gossip_client: self.gossip_client.clone(),
            peer_list: self.peer_list.clone(),
            sync_state: self.sync_state.clone(),
            span: self.span.clone(),
            execution_payload_cache: self.execution_payload_cache.clone(),
            data_request_tracker: DataRequestTracker::new(),
        }
    }
}

impl<M, B, A> GossipDataHandler<M, B, A>
where
    M: MempoolFacade,
    B: BlockDiscoveryFacade,
    A: ApiClient,
{
    #[tracing::instrument(level = "trace", skip_all, err)]
    pub(crate) async fn handle_chunk(
        &self,
        chunk_request: GossipRequest<UnpackedChunk>,
    ) -> GossipResult<()> {
        let source_miner_address = chunk_request.miner_address;
        let chunk = chunk_request.data;
        let chunk_path_hash = chunk.chunk_path_hash();
        match self.mempool.handle_chunk_ingress(chunk).await {
            Ok(()) => {
                // Success. Mempool will send the tx data to the internal mempool,
                //  but we still need to update the cache with the source address.
                self.cache
                    .record_seen(source_miner_address, GossipCacheKey::Chunk(chunk_path_hash))
            }
            Err(error) => {
                match error {
                    ChunkIngressError::Critical(err) => {
                        match err {
                            // ===== External invalid data errors
                            CriticalChunkIngressError::InvalidProof => Err(
                                GossipError::InvalidData(InvalidDataError::ChunkInvalidProof),
                            ),
                            CriticalChunkIngressError::InvalidDataHash => Err(
                                GossipError::InvalidData(InvalidDataError::ChunkInvalidDataHash),
                            ),
                            CriticalChunkIngressError::InvalidChunkSize => Err(
                                GossipError::InvalidData(InvalidDataError::ChunkInvalidChunkSize),
                            ),
                            CriticalChunkIngressError::InvalidDataSize => Err(
                                GossipError::InvalidData(InvalidDataError::ChunkInvalidDataSize),
                            ),
                            CriticalChunkIngressError::InvalidOffset(msg) => Err(
                                GossipError::InvalidData(InvalidDataError::ChunkInvalidOffset(msg)),
                            ),
                            // ===== Internal errors
                            CriticalChunkIngressError::DatabaseError => {
                                Err(GossipError::Internal(InternalGossipError::Database(
                                    "Chunk ingress database error".to_string(),
                                )))
                            }
                            CriticalChunkIngressError::ServiceUninitialized => Err(
                                GossipError::Internal(InternalGossipError::ServiceUninitialized),
                            ),
                            CriticalChunkIngressError::Other(other) => {
                                Err(GossipError::Internal(InternalGossipError::Unknown(other)))
                            }
                        }
                    }

                    ChunkIngressError::Advisory(err) => {
                        match err {
                            // ===== Interval data 'errors' (peers should not be punished)
                            AdvisoryChunkIngressError::PreHeaderOversizedBytes => {
                                Err(GossipError::Advisory(AdvisoryGossipError::ChunkIngress(
                                    AdvisoryChunkIngressError::PreHeaderOversizedBytes,
                                )))
                            }
                            AdvisoryChunkIngressError::PreHeaderOversizedDataPath => {
                                Err(GossipError::Advisory(AdvisoryGossipError::ChunkIngress(
                                    AdvisoryChunkIngressError::PreHeaderOversizedDataPath,
                                )))
                            }
                            AdvisoryChunkIngressError::PreHeaderOffsetExceedsCap => {
                                Err(GossipError::Advisory(AdvisoryGossipError::ChunkIngress(
                                    AdvisoryChunkIngressError::PreHeaderOffsetExceedsCap,
                                )))
                            }
                            AdvisoryChunkIngressError::PreHeaderInvalidOffset(msg) => {
                                Err(GossipError::Advisory(AdvisoryGossipError::ChunkIngress(
                                    AdvisoryChunkIngressError::PreHeaderInvalidOffset(msg),
                                )))
                            }
                            AdvisoryChunkIngressError::Other(other) => {
                                Err(GossipError::Advisory(AdvisoryGossipError::ChunkIngress(
                                    AdvisoryChunkIngressError::Other(other),
                                )))
                            }
                        }
                    }
                }
            }
        }
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub(crate) async fn handle_transaction(
        &self,
        transaction_request: GossipRequest<DataTransactionHeader>,
    ) -> GossipResult<()> {
        debug!(
            "Node {}: Gossip transaction received from peer {}: {:?}",
            self.gossip_client.mining_address,
            transaction_request.miner_address,
            transaction_request.data.id
        );
        let tx = transaction_request.data;
        let source_miner_address = transaction_request.miner_address;
        let tx_id = tx.id;

        let already_seen = self.cache.seen_transaction_from_any_peer(&tx_id)?;

        if already_seen {
            debug!(
                "Node {}: Transaction {} is already recorded in the cache, skipping",
                self.gossip_client.mining_address, tx_id
            );
            return Ok(());
        }

        if self
            .mempool
            .is_known_data_transaction(tx_id)
            .await
            .map_err(|e| {
                GossipError::Internal(InternalGossipError::Unknown(format!(
                    "is_known_transaction() errored: {:?}",
                    e
                )))
            })?
            .is_known()
        {
            debug!(
                "Node {}: Transaction has already been handled, skipping",
                self.gossip_client.mining_address
            );
            return Ok(());
        }

        match self
            .mempool
            .handle_data_transaction_ingress_gossip(tx)
            .await
            .map_err(GossipError::from)
        {
            Ok(()) | Err(GossipError::TransactionIsAlreadyHandled) => {
                debug!("Transaction sent to mempool");
                // Only record as seen after successful validation
                self.cache
                    .record_seen(source_miner_address, GossipCacheKey::Transaction(tx_id))?;
                Ok(())
            }
            Err(error) => {
                error!("Error when sending transaction to mempool: {}", error);
                Err(error)
            }
        }
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub(crate) async fn handle_ingress_proof(
        &self,
        proof_request: GossipRequest<IngressProof>,
    ) -> GossipResult<()> {
        debug!(
            "Node {}: Gossip ingress_proof received from peer {}: {:?}",
            self.gossip_client.mining_address,
            proof_request.miner_address,
            proof_request.data.proof
        );

        let proof = proof_request.data;
        let source_miner_address = proof_request.miner_address;
        let proof_hash = proof.proof;

        let already_seen = self.cache.seen_ingress_proof_from_any_peer(&proof_hash)?;

        if already_seen {
            debug!(
                "Node {}: Ingress Proof {} is already recorded in the cache, skipping",
                self.gossip_client.mining_address, proof_hash
            );
            return Ok(());
        }

        // TODO: Check to see if this proof is in the DB LRU Cache

        match self
            .mempool
            .handle_ingest_ingress_proof(proof)
            .await
            .map_err(GossipError::from)
        {
            Ok(()) | Err(GossipError::TransactionIsAlreadyHandled) => {
                debug!("Ingress Proof sent to mempool");
                // Only record as seen after successful validation
                self.cache.record_seen(
                    source_miner_address,
                    GossipCacheKey::IngressProof(proof_hash),
                )?;
                Ok(())
            }
            Err(error) => {
                error!("Error when sending ingress proof to mempool: {}", error);
                Err(error)
            }
        }
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub(crate) async fn handle_commitment_tx(
        &self,
        transaction_request: GossipRequest<CommitmentTransaction>,
    ) -> GossipResult<()> {
        debug!(
            "Node {}: Gossip commitment transaction received from peer {}: {:?}",
            self.gossip_client.mining_address,
            transaction_request.miner_address,
            transaction_request.data.id
        );
        let tx = transaction_request.data;
        let source_miner_address = transaction_request.miner_address;
        let tx_id = tx.id;

        let already_seen = self.cache.seen_transaction_from_any_peer(&tx_id)?;

        if already_seen {
            debug!(
                "Node {}: Commitment Transaction {} is already recorded in the cache, skipping",
                self.gossip_client.mining_address, tx_id
            );
            return Ok(());
        }

        if self
            .mempool
            .is_known_commitment_transaction(tx_id)
            .await
            .map_err(|e| {
                GossipError::Internal(InternalGossipError::Unknown(format!(
                    "is_known_transaction() errored: {:?}",
                    e
                )))
            })?
            .is_known()
        {
            debug!(
                "Node {}: Commitment Transaction has already been handled, skipping",
                self.gossip_client.mining_address
            );
            return Ok(());
        }

        match self
            .mempool
            .handle_commitment_transaction_ingress_gossip(tx)
            .await
            .map_err(GossipError::from)
        {
            Ok(()) | Err(GossipError::TransactionIsAlreadyHandled) => {
                debug!("Commitment Transaction sent to mempool");
                // Only record as seen after successful validation
                self.cache
                    .record_seen(source_miner_address, GossipCacheKey::Transaction(tx_id))?;
                Ok(())
            }
            Err(error) => {
                error!(
                    "Error when sending commitment transaction to mempool: {}",
                    error
                );
                Err(error)
            }
        }
    }

    /// Pulls a block from the network and sends it to the BlockPool for processing
    #[tracing::instrument(level = "trace", skip_all, err)]
    pub async fn pull_and_process_block(
        &self,
        block_hash: BlockHash,
        use_trusted_peers_only: bool,
    ) -> GossipResult<()> {
        debug!("Pulling block {} from the network", block_hash);
        let (source_address, irys_block) = self
            .gossip_client
            .pull_block_header_from_network(block_hash, use_trusted_peers_only, &self.peer_list)
            .await?;

        let Some(peer_info) = self.peer_list.peer_by_mining_address(&source_address) else {
            // This shouldn't happen, but we still should have a safeguard just in case
            error!(
                "Sync task: Peer with address {:?} is not found in the peer list, which should never happen, as we just fetched the data from that peer",
                source_address
            );
            return Err(GossipError::InvalidPeer("Expected peer to be in the peer list since we just fetched the block from it, but it was not found".into()));
        };

        debug!(
            "Pulled block {} from peer {}, sending for processing",
            block_hash, source_address
        );
        self.handle_block_header(
            GossipRequest {
                miner_address: source_address,
                data: (*irys_block).clone(),
            },
            peer_info.address.gossip,
        )
        .in_current_span()
        .await
    }

    /// Pulls a block from a specific peer and sends it to the BlockPool for processing
    #[tracing::instrument(level = "trace", skip_all, err)]
    pub async fn pull_and_process_block_from_peer(
        &self,
        block_hash: BlockHash,
        peer: &(irys_types::IrysAddress, PeerListItem),
    ) -> GossipResult<()> {
        let (source_address, irys_block) = self
            .gossip_client
            .pull_block_from_peer(block_hash, peer, &self.peer_list)
            .await?;

        let Some(peer_info) = self.peer_list.peer_by_mining_address(&source_address) else {
            error!(
                "Sync task: Peer with address {:?} is not found in the peer list, which should never happen, as we just fetched the data from it",
                source_address
            );
            return Err(GossipError::InvalidPeer("Expected peer to be in the peer list since we just fetched the block from it, but it was not found".into()));
        };

        self.handle_block_header(
            GossipRequest {
                miner_address: source_address,
                data: (*irys_block).clone(),
            },
            peer_info.address.gossip,
        )
        .in_current_span()
        .await
    }

    #[instrument(skip_all, fields(block.hash = ?block_header_request.data.block_hash), parent = &self.span)]
    pub(crate) async fn handle_block_header(
        &self,
        block_header_request: GossipRequest<IrysBlockHeader>,
        data_source_ip: SocketAddr,
    ) -> GossipResult<()> {
        if block_header_request.data.poa.chunk.is_none() {
            error!("received a block without a POA chunk");
        }
        let source_miner_address = block_header_request.miner_address;
        let block_header = block_header_request.data;
        let block_hash = block_header.block_hash;
        debug!(
            "Node {}: Gossip block received from peer {}: {} height: {}",
            self.gossip_client.mining_address,
            source_miner_address,
            block_hash,
            block_header.height
        );

        let sync_target = self.sync_state.sync_target_height();
        if self.sync_state.is_syncing() && block_header.height > (sync_target + 1) as u64 {
            debug!(
                "Node {}: Block {} height {} is out of the sync range (target: {}, highest processed: {}), skipping",
                self.gossip_client.mining_address, block_hash, block_header.height, &sync_target, &self.sync_state.highest_processed_block()
            );
            return Ok(());
        }

        let is_block_requested_by_the_pool = self.block_pool.is_block_requested(&block_hash).await;
        let has_block_already_been_received = self.cache.seen_block_from_any_peer(&block_hash)?;

        // This check must be after we've added the block to the cache, otherwise we won't be
        // able to keep track of which peers seen what
        if has_block_already_been_received && !is_block_requested_by_the_pool {
            debug!(
                "Node {}: Block {} height {} already seen and not requested by the pool, skipping",
                self.gossip_client.mining_address, block_header.block_hash, block_header.height
            );
            return Ok(());
        }

        // This check also validates block hash, thus validating that block's fields hasn't
        //  been tampered with
        if !block_header.is_signature_valid() {
            warn!(
                "Node: {}: Block {} has an invalid signature",
                self.gossip_client.mining_address, block_header.block_hash
            );

            debug!(
                target = "invalid_block_header_json",
                "Invalid block: {:#}",
                &serde_json::to_string(&block_header).unwrap_or_else(|e| format!(
                    // fallback to debug printing the header
                    "error serializing block header: {}\n{:#}",
                    &e, &block_header
                ))
            );

            self.peer_list.decrease_peer_score(
                &source_miner_address,
                ScoreDecreaseReason::BogusData("Invalid block signature".into()),
            );

            return Err(GossipError::InvalidData(
                InvalidDataError::InvalidBlockSignature,
            ));
        }

        // Record block in cache
        self.cache
            .record_seen(source_miner_address, GossipCacheKey::Block(block_hash))?;

        let has_block_already_been_processed = self
            .block_pool
            .is_block_processing_or_processed(&block_header.block_hash, block_header.height)
            .await;

        if has_block_already_been_processed {
            debug!(
                "Node {}: Block {} height {} has already been processed, skipping",
                self.gossip_client.mining_address, block_header.block_hash, block_header.height
            );
            return Ok(());
        }

        debug!(
            "Node {}: Block {} height {} has not been processed yet, starting processing",
            self.gossip_client.mining_address, block_header.block_hash, block_header.height
        );

        let is_syncing_from_a_trusted_peer = self.sync_state.is_syncing_from_a_trusted_peer();
        let is_in_the_trusted_sync_range = self
            .sync_state
            .is_in_trusted_sync_range(block_header.height as usize);

        let skip_block_validation = is_syncing_from_a_trusted_peer
            && is_in_the_trusted_sync_range
            && self
                .peer_list
                .is_a_trusted_peer(source_miner_address, data_source_ip.ip());

        let mut block_body = None;
        let mut last_error = None;

        for attempt in 1..=HEADER_AND_BODY_RETRIES {
            match self
                .pull_block_body(&block_header, skip_block_validation)
                .await
            {
                Ok(body) => match body.tx_ids_match_the_header(&block_header) {
                    Ok(true) => {
                        block_body = Some(body);
                        break;
                    }
                    Ok(false) => {
                        warn!(
                                "Node {}: Block {} height {} has mismatching transactions between header and body (attempt {}/{})",
                                self.gossip_client.mining_address, block_header.block_hash, block_header.height, attempt, HEADER_AND_BODY_RETRIES
                            );
                        last_error = Some(GossipError::InvalidData(
                            InvalidDataError::BlockBodyTransactionsMismatch,
                        ));
                    }
                    Err(err) => {
                        last_error = Some(GossipError::Internal(InternalGossipError::Unknown(
                                format!(
                                    "Error when comparing block body transactions with header for block {}: {}",
                                    block_header.block_hash, err
                                ),
                            )));
                    }
                },
                Err(e) => {
                    last_error = Some(e);
                }
            }
        }

        let block_body = match block_body {
            Some(body) => body,
            None => {
                if let Some(GossipError::InvalidData(
                    InvalidDataError::BlockBodyTransactionsMismatch,
                )) = &last_error
                {
                    warn!(
                        "Node {}: Block {} height {} has mismatching transactions between header and body",
                        self.gossip_client.mining_address, block_header.block_hash, block_header.height
                    );

                    self.peer_list.decrease_peer_score(
                        &source_miner_address,
                        ScoreDecreaseReason::BogusData(
                            "Mismatching transactions between header and body".into(),
                        ),
                    );
                }

                return Err(last_error.unwrap_or_else(|| {
                    GossipError::Internal(InternalGossipError::Unknown(
                        "Failed to pull block body".to_string(),
                    ))
                }));
            }
        };

        self.block_pool
            .process_block::<A>(Arc::new(block_header), block_body, skip_block_validation)
            .await?;
        Ok(())
    }

    pub async fn handle_block_body(
        &self,
        block_body_request: GossipRequest<BlockBody>,
        data_source_ip: SocketAddr,
    ) -> GossipResult<()> {
        let source_miner_address = block_body_request.miner_address;
        let block_body = block_body_request.data;
        let block_hash = block_body.block_hash;

        debug!(
            "Node {}: Gossip block body received from peer {}: {}",
            self.gossip_client.mining_address, source_miner_address, block_hash
        );

        let is_block_requested_by_the_pool = self.block_pool.is_block_requested(&block_hash).await;
        let has_block_already_been_received = self.cache.seen_block_from_any_peer(&block_hash)?;

        if has_block_already_been_received && !is_block_requested_by_the_pool {
            debug!(
                "Node {}: Block {} already seen and not requested by the pool, skipping",
                self.gossip_client.mining_address, block_hash
            );
            return Ok(());
        }

        let maybe_header = self.block_pool.get_block_header(&block_hash).await?;

        let block_header = if let Some(header) = maybe_header {
            header
        } else {
            let mut fetched_header = None;
            let mut last_error = None;

            for _ in 1..=HEADER_AND_BODY_RETRIES {
                match self.pull_block_header(block_hash, false).await {
                    Ok((source, header)) => {
                        if !header.is_signature_valid() {
                            warn!(
                                "Node: {}: Block {} fetched from {} has an invalid signature",
                                self.gossip_client.mining_address, header.block_hash, source
                            );
                            self.peer_list.decrease_peer_score(
                                &source,
                                ScoreDecreaseReason::BogusData("Invalid block signature".into()),
                            );
                            last_error = Some(GossipError::InvalidData(
                                InvalidDataError::InvalidBlockSignature,
                            ));
                            continue;
                        }
                        fetched_header = Some(header);
                        break;
                    }
                    Err(e) => {
                        last_error = Some(e);
                    }
                }
            }

            match fetched_header {
                Some(h) => h,
                None => {
                    return Err(last_error.unwrap_or_else(|| {
                        GossipError::Internal(InternalGossipError::Unknown(
                            "Failed to pull block header".to_string(),
                        ))
                    }))
                }
            }
        };

        // Check sync range
        let sync_target = self.sync_state.sync_target_height();
        if self.sync_state.is_syncing() && block_header.height > (sync_target + 1) as u64 {
            debug!(
                "Node {}: Block {} height {} is out of the sync range (target: {}, highest processed: {}), skipping",
                self.gossip_client.mining_address, block_hash, block_header.height, &sync_target, &self.sync_state.highest_processed_block()
            );
            return Ok(());
        }

        let has_block_already_been_processed = self
            .block_pool
            .is_block_processing_or_processed(&block_header.block_hash, block_header.height)
            .await;

        if has_block_already_been_processed {
            debug!(
                "Node {}: Block {} height {} has already been processed, skipping",
                self.gossip_client.mining_address, block_header.block_hash, block_header.height
            );
            return Ok(());
        }

        debug!(
            "Node {}: Block {} height {} has not been processed yet, starting processing",
            self.gossip_client.mining_address, block_header.block_hash, block_header.height
        );

        let is_syncing_from_a_trusted_peer = self.sync_state.is_syncing_from_a_trusted_peer();
        let is_in_the_trusted_sync_range = self
            .sync_state
            .is_in_trusted_sync_range(block_header.height as usize);

        let skip_block_validation = is_syncing_from_a_trusted_peer
            && is_in_the_trusted_sync_range
            && self
                .peer_list
                .is_a_trusted_peer(source_miner_address, data_source_ip.ip());

        match block_body.tx_ids_match_the_header(&block_header) {
            Ok(true) => {}
            Ok(false) => {
                warn!(
                    "Node {}: Block {} height {} has mismatching transactions between header and body",
                    self.gossip_client.mining_address, block_header.block_hash, block_header.height
                );

                self.peer_list.decrease_peer_score(
                    &source_miner_address,
                    ScoreDecreaseReason::BogusData(
                        "Mismatching transactions between header and body".into(),
                    ),
                );
                return Err(GossipError::InvalidData(
                    InvalidDataError::BlockBodyTransactionsMismatch,
                ));
            }
            Err(err) => {
                return Err(GossipError::Internal(InternalGossipError::Unknown(
                    format!(
                        "Error when comparing block body transactions with header for block {}: {}",
                        block_header.block_hash, err
                    ),
                )));
            }
        }

        // Record block in cache
        self.cache
            .record_seen(source_miner_address, GossipCacheKey::Block(block_hash))?;

        self.block_pool
            .process_block::<A>(block_header, Arc::new(block_body), skip_block_validation)
            .await?;
        Ok(())
    }

    pub async fn pull_block_header(
        &self,
        block_hash: BlockHash,
        use_trusted_peers_only: bool,
    ) -> GossipResult<(IrysAddress, Arc<IrysBlockHeader>)> {
        debug!(
            "Fetching block header for block {} from the network",
            block_hash
        );

        let (source_address, irys_block_header) = self
            .gossip_client
            .pull_block_header_from_network(block_hash, use_trusted_peers_only, &self.peer_list)
            .await?;

        let Some(_peer_info) = self.peer_list.peer_by_mining_address(&source_address) else {
            error!(
                "Sync task: Peer with address {:?} is not found in the peer list, which should never happen, as we just fetched the data from that peer",
                source_address
            );
            return Err(GossipError::InvalidPeer("Expected peer to be in the peer list since we just fetched the block header from it, but it was not found".into()));
        };

        debug!(
            "Fetched block header for block {} from peer {}",
            block_hash, source_address
        );

        Ok((source_address, irys_block_header))
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub async fn pull_and_add_execution_payload_to_cache(
        &self,
        evm_block_hash: EvmBlockHash,
        use_trusted_peers_only: bool,
    ) -> GossipResult<()> {
        let mut last_err = None;
        for attempt in 1..=3 {
            match self
                .gossip_client
                .pull_payload_from_network(evm_block_hash, use_trusted_peers_only, &self.peer_list)
                .await
            {
                Ok((source_address, execution_payload)) => {
                    if let Err(e) = self
                        .handle_execution_payload(GossipRequest {
                            miner_address: source_address,
                            data: execution_payload,
                        })
                        .await
                    {
                        last_err = Some(e);
                        if attempt < 3 {
                            continue;
                        }
                    }
                    return Ok(());
                }
                Err(e) => {
                    last_err = Some(GossipError::from(e));
                    if attempt < 3 {
                        continue;
                    }
                }
            }
        }
        Err(last_err.expect("Error must be set after 3 attempts"))
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub(crate) async fn handle_execution_payload(
        &self,
        execution_payload_request: GossipRequest<Block>,
    ) -> GossipResult<()> {
        let source_miner_address = execution_payload_request.miner_address;
        let evm_block = execution_payload_request.data;

        // Basic validation: ensure the block can be sealed (structure validation)
        let sealed_block = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            evm_block.seal_slow()
        })) {
            Ok(sealed) => sealed,
            Err(_) => {
                return Err(GossipError::InvalidData(
                    InvalidDataError::ExecutionPayloadInvalidStructure,
                ));
            }
        };

        let evm_block_hash = sealed_block.hash();
        let payload_already_seen_before = self
            .cache
            .seen_execution_payload_from_any_peer(&evm_block_hash)?;
        let expecting_payload = self
            .execution_payload_cache
            .is_waiting_for_payload(&evm_block_hash)
            .await;

        if payload_already_seen_before && !expecting_payload {
            debug!(
                "Node {}: Execution payload for EVM block {:?} already seen, and no service requested it to be fetched again, skipping",
                self.gossip_client.mining_address,
                evm_block_hash
            );
            return Ok(());
        }

        // Additional validation: verify block structure is valid
        let header = sealed_block.header();
        if header.number == 0 && !header.parent_hash.is_zero() {
            return Err(GossipError::InvalidData(
                InvalidDataError::ExecutionPayloadInvalidStructure,
            ));
        }

        self.execution_payload_cache
            .add_payload_to_cache(sealed_block)
            .await;

        // Only record as seen after validation and successful cache addition
        self.cache.record_seen(
            source_miner_address,
            GossipCacheKey::ExecutionPayload(evm_block_hash),
        )?;

        debug!(
            "Node {}: Execution payload for EVM block {:?} have been added to the cache",
            self.gossip_client.mining_address, evm_block_hash
        );

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub(crate) async fn handle_get_data(
        &self,
        peer_info: &PeerListItem,
        request: GossipRequest<GossipDataRequest>,
        duplicate_request_milliseconds: u128,
    ) -> GossipResult<bool> {
        // Check rate limiting and score cap
        let check_result = self
            .data_request_tracker
            .check_request(&request.miner_address, duplicate_request_milliseconds);

        // If rate limited, don't serve data
        if !check_result.should_serve() {
            debug!(
                "Node {}: Rate limiting peer {:?} for data request",
                self.gossip_client.mining_address, request.miner_address
            );
            return Err(GossipError::RateLimited);
        }

        match request.data {
            GossipDataRequest::BlockHeader(block_hash) => {
                let maybe_block = self.block_pool.get_block_header(&block_hash).await?;

                match maybe_block {
                    Some(block) => {
                        if block.poa.chunk.is_none() {
                            error!(
                                target = "p2p::gossip_data_handler::handle_get_data",
                                block.hash = ?block.block_hash,
                                "Block pool returned a block without a POA chunk"
                            );
                        }
                        let data = Arc::new(GossipData::BlockHeader(block));
                        if check_result.should_update_score() {
                            self.gossip_client.send_data_and_update_score_for_request(
                                (&request.miner_address, peer_info),
                                data,
                                &self.peer_list,
                            );
                        } else {
                            self.gossip_client.send_data_without_score_update(
                                (&request.miner_address, peer_info),
                                data,
                            );
                        }
                        Ok(true)
                    }
                    None => Ok(false),
                }
            }
            GossipDataRequest::BlockBody(block_hash) => {
                debug!(
                    "Node {}: handling block body request for block {:?}",
                    self.gossip_client.mining_address, block_hash
                );
                let block_body = if let Some(block_body) =
                    self.block_pool.get_cached_block_body(&block_hash).await
                {
                    Some(block_body)
                } else {
                    let maybe_block = self.block_pool.get_block_header(&block_hash).await?;
                    if let Some(block) = &maybe_block {
                        let data_tx_ids = block
                            .data_ledgers
                            .iter()
                            .flat_map(|data_ledger| &data_ledger.tx_ids.0)
                            .copied()
                            .collect::<Vec<_>>();
                        let commitment_tx_ids = block
                            .system_ledgers
                            .iter()
                            .flat_map(|commitment_ledger| &commitment_ledger.tx_ids.0)
                            .copied()
                            .collect::<Vec<_>>();
                        let block_body = build_block_body_for_processed_block(
                            block_hash,
                            &data_tx_ids,
                            &commitment_tx_ids,
                            &self.mempool.get_internal_read_guard().await,
                            &self.block_pool.db,
                        )
                        .await
                        .map_err(|err| {
                            GossipError::Internal(InternalGossipError::Unknown(format!(
                                "Error building block body for block {}: {}",
                                block_hash, err
                            )))
                        })?;
                        Some(Arc::new(block_body))
                    } else {
                        None
                    }
                };

                if let Some(block_body) = block_body {
                    let data = Arc::new(GossipData::BlockBody(block_body));
                    if check_result.should_update_score() {
                        self.gossip_client.send_data_and_update_score_for_request(
                            (&request.miner_address, peer_info),
                            data,
                            &self.peer_list,
                        );
                    } else {
                        self.gossip_client.send_data_without_score_update(
                            (&request.miner_address, peer_info),
                            data,
                        );
                    }
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            GossipDataRequest::ExecutionPayload(evm_block_hash) => {
                debug!(
                    "Node {}: Handling execution payload request for block {:?}",
                    self.gossip_client.mining_address, evm_block_hash
                );
                let maybe_evm_block = self
                    .execution_payload_cache
                    .get_locally_stored_evm_block(&evm_block_hash)
                    .await;

                match maybe_evm_block {
                    Some(evm_block) => {
                        let data = Arc::new(GossipData::ExecutionPayload(evm_block));
                        if check_result.should_update_score() {
                            self.gossip_client.send_data_and_update_score_for_request(
                                (&request.miner_address, peer_info),
                                data,
                                &self.peer_list,
                            );
                        } else {
                            self.gossip_client.send_data_without_score_update(
                                (&request.miner_address, peer_info),
                                data,
                            );
                        }
                        Ok(true)
                    }
                    None => Ok(false),
                }
            }
            GossipDataRequest::Chunk(_chunk_path_hash) => Ok(false),
        }
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub(crate) async fn handle_get_data_sync(
        &self,
        request: GossipRequest<GossipDataRequest>,
    ) -> GossipResult<Option<GossipData>> {
        match request.data {
            GossipDataRequest::BlockHeader(block_hash) => {
                let maybe_block = self.block_pool.get_block_header(&block_hash).await?;
                if let Some(block) = &maybe_block {
                    if block.poa.chunk.is_none() {
                        error!(
                            target = "p2p::gossip_data_handler::handle_get_data_sync",
                            block.hash = ?block.block_hash,
                            "Block pool returned a block without a POA chunk"
                        );
                    }
                }
                Ok(maybe_block.map(GossipData::BlockHeader))
            }
            GossipDataRequest::BlockBody(block_hash) => {
                let maybe_block_body = if let Some(block_body) =
                    self.block_pool.get_cached_block_body(&block_hash).await
                {
                    Some(block_body)
                } else {
                    let maybe_block = self.block_pool.get_block_header(&block_hash).await?;
                    if let Some(block) = &maybe_block {
                        let data_tx_ids = block
                            .data_ledgers
                            .iter()
                            .flat_map(|data_ledger| &data_ledger.tx_ids.0)
                            .copied()
                            .collect::<Vec<_>>();
                        let commitment_tx_ids = block
                            .system_ledgers
                            .iter()
                            .flat_map(|commitment_ledger| &commitment_ledger.tx_ids.0)
                            .copied()
                            .collect::<Vec<_>>();
                        let block_body = build_block_body_for_processed_block(
                            block_hash,
                            &data_tx_ids,
                            &commitment_tx_ids,
                            &self.mempool.get_internal_read_guard().await,
                            &self.block_pool.db,
                        )
                        .await
                        .map_err(|err| {
                            GossipError::Internal(InternalGossipError::Unknown(format!(
                                "Error building block body for block {}: {}",
                                block_hash, err
                            )))
                        })?;
                        Some(Arc::new(block_body))
                    } else {
                        None
                    }
                };
                Ok(maybe_block_body.map(GossipData::BlockBody))
            }
            GossipDataRequest::ExecutionPayload(evm_block_hash) => {
                let maybe_evm_block = self
                    .execution_payload_cache
                    .get_locally_stored_evm_block(&evm_block_hash)
                    .await;

                Ok(maybe_evm_block.map(GossipData::ExecutionPayload))
            }
            GossipDataRequest::Chunk(_chunk_path_hash) => Ok(None),
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) async fn handle_get_stake_and_pledge_whitelist(&self) -> Vec<IrysAddress> {
        self.mempool
            .get_stake_and_pledge_whitelist()
            .await
            .into_iter()
            .collect()
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub(crate) async fn pull_and_process_stake_and_pledge_whitelist(&self) -> GossipResult<()> {
        let allowed_miner_addresses = self
            .gossip_client
            .clone()
            .stake_and_pledge_whitelist(&self.peer_list)
            .await?;

        self.mempool
            .update_stake_and_pledge_whitelist(HashSet::from_iter(
                allowed_miner_addresses.into_iter(),
            ))
            .await
            .map_err(|e| {
                GossipError::Internal(InternalGossipError::Unknown(format!(
                    "get_stake_and_pledge_whitelist() errored: {}",
                    e
                )))
            })
    }

    pub async fn pull_block_body(
        &self,
        header: &IrysBlockHeader,
        use_trusted_peers_only: bool,
    ) -> GossipResult<Arc<BlockBody>> {
        // TODO: check that transactions in the body match those in the header
        let block_hash = header.block_hash;
        debug!(
            "Fetching block body for block {} height {} from the network",
            block_hash, header.height
        );

        let (source_address, irys_block_body) = self
            .gossip_client
            .pull_block_body_from_network(block_hash, use_trusted_peers_only, &self.peer_list)
            .await?;

        debug!(
            "Fetched block body for block {} from peer {}",
            block_hash, source_address
        );

        Ok(irys_block_body)
    }
}
