use crate::{
    block_pool::BlockPool,
    cache::GossipCache,
    metrics::{
        record_gossip_chunk_processing_duration, record_gossip_chunk_received,
        record_gossip_inbound_error,
    },
    rate_limiting::{DataRequestTracker, RequestCheckResult},
    types::{AdvisoryGossipError, InternalGossipError, InvalidDataError},
    GossipClient, GossipError, GossipResult,
};
use core::net::SocketAddr;
use irys_actors::block_discovery::{
    build_block_body_for_processed_block_header, get_commitment_tx_in_parallel,
    get_data_tx_in_parallel,
};
use irys_actors::{
    block_discovery::BlockDiscoveryFacade, chunk_ingress_service::facade::ChunkIngressFacadeImpl,
    ChunkIngressError, CriticalChunkIngressError, MempoolFacade,
};
use irys_domain::chain_sync_state::ChainSyncState;
use irys_domain::{
    BlockIndexReadGuard, BlockTreeReadGuard, ExecutionPayloadCache, PeerList, ScoreDecreaseReason,
};
use irys_types::v2::{GossipDataRequestV2, GossipDataV2};
use irys_types::{BlockBody, Config, IrysAddress, IrysPeerId, PeerNetworkError, H256};
use irys_types::{
    BlockHash, CommitmentTransaction, DataTransactionHeader, EvmBlockHash, GossipCacheKey,
    GossipRequestV2, IngressProof, IrysBlockHeader, PeerListItem, SealedBlock, UnpackedChunk,
};
use irys_utils::ElapsedMs as _;
use reth::builder::Block as _;
use reth_ethereum_primitives::Block;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, instrument, warn, Instrument as _};

const HEADER_AND_BODY_RETRIES: usize = 3;

/// Builds a human-readable summary from a list of failed peer attempts.
fn format_failure_summary(
    operation: &str,
    failed_attempts: &[(Option<IrysPeerId>, GossipError)],
) -> String {
    if failed_attempts.is_empty() {
        return format!("Failed to pull {}", operation);
    }
    let entries: Vec<String> = failed_attempts
        .iter()
        .map(|(source, err)| match source {
            Some(peer_id) => format!("[Peer {}: {}]", peer_id, err),
            None => format!("[Unknown peer: {}]", err),
        })
        .collect();
    format!(
        "Failed to pull {} after {} attempts: {}",
        operation,
        failed_attempts.len(),
        entries.join(", ")
    )
}

/// Handles data received by the `GossipServer`
#[derive(Debug)]
pub struct GossipDataHandler<TMempoolFacade, TBlockDiscovery>
where
    TMempoolFacade: MempoolFacade,
    TBlockDiscovery: BlockDiscoveryFacade,
{
    pub mempool: TMempoolFacade,
    pub chunk_ingress: ChunkIngressFacadeImpl,
    pub block_pool: Arc<BlockPool<TBlockDiscovery, TMempoolFacade>>,
    pub(crate) cache: Arc<GossipCache>,
    pub gossip_client: GossipClient,
    pub peer_list: PeerList,
    pub sync_state: ChainSyncState,
    pub execution_payload_cache: ExecutionPayloadCache,
    pub data_request_tracker: DataRequestTracker,
    pub block_index: BlockIndexReadGuard,
    pub block_tree: BlockTreeReadGuard,
    pub config: Config,
    pub started_at: Instant,
    /// Precomputed hash of the consensus config to avoid recomputing on every handshake
    pub consensus_config_hash: H256,
}

impl<M, B> Clone for GossipDataHandler<M, B>
where
    M: MempoolFacade,
    B: BlockDiscoveryFacade,
{
    fn clone(&self) -> Self {
        Self {
            mempool: self.mempool.clone(),
            chunk_ingress: self.chunk_ingress.clone(),
            block_pool: self.block_pool.clone(),
            cache: Arc::clone(&self.cache),
            gossip_client: self.gossip_client.clone(),
            peer_list: self.peer_list.clone(),
            sync_state: self.sync_state.clone(),
            execution_payload_cache: self.execution_payload_cache.clone(),
            data_request_tracker: DataRequestTracker::new(),
            block_index: self.block_index.clone(),
            block_tree: self.block_tree.clone(),
            config: self.config.clone(),
            started_at: self.started_at,
            consensus_config_hash: self.consensus_config_hash,
        }
    }
}

impl<M, B> GossipDataHandler<M, B>
where
    M: MempoolFacade,
    B: BlockDiscoveryFacade,
{
    #[tracing::instrument(level = "info", skip_all, err)]
    pub(crate) async fn handle_chunk(
        &self,
        chunk_request: GossipRequestV2<UnpackedChunk>,
    ) -> GossipResult<()> {
        let start = Instant::now();
        let source_peer_id = chunk_request.peer_id;
        let chunk = chunk_request.data;
        let chunk_size = chunk.bytes.0.len() as u64;
        let chunk_path_hash = chunk.chunk_path_hash();

        record_gossip_chunk_received(chunk_size);

        match self.chunk_ingress.handle_chunk_ingress(chunk).await {
            Ok(()) => {
                // Record processing duration on success
                record_gossip_chunk_processing_duration(start.elapsed_ms());

                // Success. Mempool will send the tx data to the internal mempool,
                //  but we still need to update the cache with the source address.
                self.cache
                    .record_seen(source_peer_id, GossipCacheKey::Chunk(chunk_path_hash))
            }
            Err(error) => {
                record_gossip_inbound_error(error.error_type(), error.is_advisory());

                Err(match error {
                    ChunkIngressError::Critical(err) => match err {
                        CriticalChunkIngressError::InvalidProof => {
                            GossipError::InvalidData(InvalidDataError::ChunkInvalidProof)
                        }
                        CriticalChunkIngressError::InvalidDataHash => {
                            GossipError::InvalidData(InvalidDataError::ChunkInvalidDataHash)
                        }
                        CriticalChunkIngressError::InvalidChunkSize => {
                            GossipError::InvalidData(InvalidDataError::ChunkInvalidChunkSize)
                        }
                        CriticalChunkIngressError::InvalidDataSize => {
                            GossipError::InvalidData(InvalidDataError::ChunkInvalidDataSize)
                        }
                        CriticalChunkIngressError::InvalidOffset(msg) => {
                            GossipError::InvalidData(InvalidDataError::ChunkInvalidOffset(msg))
                        }
                        CriticalChunkIngressError::DatabaseError => GossipError::Internal(
                            InternalGossipError::Database("Chunk ingress database error".into()),
                        ),
                        CriticalChunkIngressError::ServiceUninitialized => {
                            GossipError::Internal(InternalGossipError::ServiceUninitialized)
                        }
                        CriticalChunkIngressError::Other(other) => {
                            GossipError::Internal(InternalGossipError::Unknown(other))
                        }
                    },
                    ChunkIngressError::Advisory(err) => {
                        GossipError::Advisory(AdvisoryGossipError::ChunkIngress(err))
                    }
                })
            }
        }
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub(crate) async fn handle_transaction(
        &self,
        transaction_request: GossipRequestV2<DataTransactionHeader>,
    ) -> GossipResult<()> {
        let source_peer_id = transaction_request.peer_id;
        let source_miner_address = transaction_request.miner_address;
        debug!(
            "Node {}: Gossip transaction received from peer {}: {:?}",
            self.gossip_client.mining_address, source_miner_address, transaction_request.data.id
        );
        let tx = transaction_request.data;
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
                    .record_seen(source_peer_id, GossipCacheKey::Transaction(tx_id))?;
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
        proof_request: GossipRequestV2<IngressProof>,
    ) -> GossipResult<()> {
        let source_peer_id = proof_request.peer_id;
        let source_miner_address = proof_request.miner_address;
        debug!(
            "Node {}: Gossip ingress_proof received from peer {}: {:?}",
            self.gossip_client.mining_address, source_miner_address, proof_request.data.proof
        );

        let proof = proof_request.data;
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
            .chunk_ingress
            .handle_ingest_ingress_proof(proof)
            .await
            .map_err(GossipError::from)
        {
            Ok(()) | Err(GossipError::TransactionIsAlreadyHandled) => {
                debug!("Ingress Proof sent to chunk ingress");
                // Only record as seen after successful validation
                self.cache
                    .record_seen(source_peer_id, GossipCacheKey::IngressProof(proof_hash))?;
                Ok(())
            }
            Err(error) => {
                error!(
                    "Error when sending ingress proof to chunk ingress: {}",
                    error
                );
                Err(error)
            }
        }
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub(crate) async fn handle_commitment_tx(
        &self,
        transaction_request: GossipRequestV2<CommitmentTransaction>,
    ) -> GossipResult<()> {
        let source_peer_id = transaction_request.peer_id;
        let source_miner_address = transaction_request.miner_address;
        debug!(
            "Node {}: Gossip commitment transaction received from peer {}: {:?}",
            self.gossip_client.mining_address,
            source_miner_address,
            transaction_request.data.id()
        );
        let tx = transaction_request.data;
        let tx_id = tx.id();

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
                    .record_seen(source_peer_id, GossipCacheKey::Transaction(tx_id))?;
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
        let (source_peer_id, irys_block) = match self
            .gossip_client
            .pull_block_header_from_network(block_hash, use_trusted_peers_only, &self.peer_list)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                self.sync_state.record_data_pull_error(format!(
                    "pull_block_header_from_network failed for {}: {:?}",
                    block_hash, e
                ));
                return Err(e.into());
            }
        };

        let Some(peer_info) = self.peer_list.get_peer(&source_peer_id) else {
            // This shouldn't happen, but we still should have a safeguard just in case
            let error_msg = format!(
                "Peer with peer_id {} is not found in the peer list, which should never happen, as we just fetched the data from that peer (block {})",
                source_peer_id, block_hash
            );
            error!("Sync task: {}", error_msg);
            self.sync_state.record_data_pull_error(error_msg.clone());
            return Err(GossipError::InvalidPeer(error_msg));
        };

        debug!(
            "Pulled block {} from peer {}, sending for processing",
            block_hash, source_peer_id
        );
        // Get miner_address from the peer item
        let miner_address = peer_info.mining_address;
        self.handle_block_header(
            GossipRequestV2 {
                peer_id: source_peer_id,
                miner_address,

                data: Arc::unwrap_or_clone(irys_block),
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
        peer: &(irys_types::IrysPeerId, PeerListItem),
    ) -> GossipResult<()> {
        let (source_peer_id, irys_block) = match self
            .gossip_client
            .pull_block_header_from_peer(block_hash, peer, &self.peer_list)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                self.sync_state.record_data_pull_error(format!(
                    "pull_block_header_from_peer failed for {}: {:?}",
                    block_hash, e
                ));
                return Err(e.into());
            }
        };

        let Some(peer_info) = self.peer_list.get_peer(&source_peer_id) else {
            let error_msg = format!(
                "Peer with peer_id {} is not found in the peer list, which should never happen, as we just fetched the data from it (block {})",
                source_peer_id, block_hash
            );
            error!("Sync task: {}", error_msg);
            self.sync_state.record_data_pull_error(error_msg.clone());
            return Err(GossipError::InvalidPeer(error_msg));
        };

        // Get miner_address from the peer item
        let miner_address = peer_info.mining_address;
        self.handle_block_header(
            GossipRequestV2 {
                peer_id: source_peer_id,
                miner_address,

                data: Arc::unwrap_or_clone(irys_block),
            },
            peer_info.address.gossip,
        )
        .in_current_span()
        .await
    }

    #[instrument(skip_all, fields(block.hash = ?block_header_request.data.block_hash))]
    pub(crate) async fn handle_block_header(
        &self,
        block_header_request: GossipRequestV2<IrysBlockHeader>,
        data_source_ip: SocketAddr,
    ) -> GossipResult<()> {
        if block_header_request.data.poa.chunk.is_none() {
            error!("received a block without a POA chunk");
        }
        let source_peer_id = block_header_request.peer_id;
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

            self.peer_list.decrease_peer_score_by_peer_id(
                &source_peer_id,
                ScoreDecreaseReason::BogusData("Invalid block signature".into()),
            );

            return Err(GossipError::InvalidData(
                InvalidDataError::InvalidBlockSignature,
            ));
        }

        // Record block in cache
        self.cache
            .record_seen(source_peer_id, GossipCacheKey::Block(block_hash))?;

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

        // In trusted sync mode, this flag serves two purposes:
        // 1. Restricts peer selection for fetching block bodies to trusted peers only
        // 2. Disables computational heavy block validation during processing
        let skip_block_validation = self.should_skip_block_validation(
            block_header.height,
            source_miner_address,
            data_source_ip.ip(),
        );

        let use_trusted_peers_only = skip_block_validation;
        let skip_validation_for_fast_track = skip_block_validation;

        // Pull block body, trying the sender first before falling back to network
        let sealed_block = self
            .pull_block_body(&block_header, use_trusted_peers_only, source_peer_id)
            .await?;

        self.block_pool
            .process_block(Arc::new(sealed_block), skip_validation_for_fast_track)
            .await?;
        Ok(())
    }

    pub async fn handle_block_body(
        &self,
        block_body_request: GossipRequestV2<BlockBody>,
        data_source_ip: SocketAddr,
    ) -> GossipResult<()> {
        let source_peer_id = block_body_request.peer_id;
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

        let block_header = match maybe_header {
            Some(header) => header,
            None => {
                // When syncing from a trusted peer, we MUST only fetch from trusted peers —
                // this is a hard security invariant.
                self.fetch_header_with_retries(
                    block_hash,
                    self.sync_state.is_syncing_from_a_trusted_peer(),
                )
                .await?
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

        let skip_block_validation = self.should_skip_block_validation(
            block_header.height,
            source_miner_address,
            data_source_ip.ip(),
        );

        let sealed_block =
            SealedBlock::new(Arc::clone(&block_header), block_body).map_err(|e| {
                self.peer_list.decrease_peer_score_by_peer_id(
                    &source_peer_id,
                    ScoreDecreaseReason::BogusData(format!("{e:?}")),
                );
                GossipError::Internal(InternalGossipError::Unknown(format!(
                    "Failed to create SealedBlock: {:?}",
                    e
                )))
            })?;

        // Record block in cache only after header/body validation succeeds,
        // so invalid bodies don't poison the duplicate gate.
        self.cache
            .record_seen(source_peer_id, GossipCacheKey::Block(block_hash))?;

        self.block_pool
            .process_block(Arc::new(sealed_block), skip_block_validation)
            .await?;
        Ok(())
    }

    pub async fn pull_block_header(
        &self,
        block_hash: BlockHash,
        use_trusted_peers_only: bool,
    ) -> GossipResult<(IrysPeerId, Arc<IrysBlockHeader>)> {
        debug!(
            "Fetching block header for block {} from the network",
            block_hash
        );

        let (source_peer_id, irys_block_header) = match self
            .gossip_client
            .pull_block_header_from_network(block_hash, use_trusted_peers_only, &self.peer_list)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                self.sync_state.record_data_pull_error(format!(
                    "pull_block_header_from_network failed for {}: {:?}",
                    block_hash, e
                ));
                return Err(e.into());
            }
        };

        let Some(_peer_info) = self.peer_list.get_peer(&source_peer_id) else {
            let error_msg = format!(
                "Peer with peer_id {} is not found in the peer list, which should never happen, as we just fetched the data from that peer (block {})",
                source_peer_id, block_hash
            );
            error!("Sync task: {}", error_msg);
            self.sync_state.record_data_pull_error(error_msg.clone());
            return Err(GossipError::InvalidPeer(error_msg));
        };

        debug!(
            "Fetched block header for block {} from peer {}",
            block_hash, source_peer_id
        );

        Ok((source_peer_id, irys_block_header))
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
                Ok((source_peer_id, execution_payload)) => {
                    // Get miner_address from the peer item
                    let miner_address = match self.peer_list.get_peer(&source_peer_id) {
                        Some(peer) => peer.mining_address,
                        None => {
                            last_err = Some(GossipError::InvalidPeer(format!(
                                "Peer not found for peer_id {:?}",
                                source_peer_id
                            )));
                            if attempt < 3 {
                                continue;
                            }
                            return Err(last_err.unwrap());
                        }
                    };
                    if let Err(e) = self
                        .handle_execution_payload(GossipRequestV2 {
                            peer_id: source_peer_id,
                            miner_address,
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
        let err = last_err.expect("Error must be set after 3 attempts");
        self.sync_state.record_data_pull_error(format!(
            "pull_payload_from_network failed for {}: {:?}",
            evm_block_hash, err
        ));
        Err(err)
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub(crate) async fn handle_execution_payload(
        &self,
        execution_payload_request: GossipRequestV2<Block>,
    ) -> GossipResult<()> {
        let source_peer_id = execution_payload_request.peer_id;
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
            source_peer_id,
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
        request: GossipRequestV2<GossipDataRequestV2>,
        duplicate_request_milliseconds: u128,
    ) -> GossipResult<bool> {
        // Check rate limiting and score cap
        let check_result = self
            .data_request_tracker
            .check_request(&request.peer_id, duplicate_request_milliseconds);

        // If rate limited, don't serve data
        if !check_result.should_serve() {
            debug!(
                "Node {}: Rate limiting peer {:?} for data request",
                self.gossip_client.mining_address, request.miner_address
            );
            return Err(GossipError::RateLimited);
        }

        match self
            .resolve_data_request(&request.data, request.miner_address)
            .await?
        {
            Some(data) => {
                self.send_gossip_data((&request.peer_id, peer_info), Arc::new(data), &check_result);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub(crate) async fn handle_get_data_sync(
        &self,
        request: GossipRequestV2<GossipDataRequestV2>,
    ) -> GossipResult<Option<GossipDataV2>> {
        self.resolve_data_request(&request.data, request.miner_address)
            .await
    }

    /// Resolves a data request by looking up the requested item locally.
    /// Returns `Some(data)` if found, `None` if the item is not available.
    async fn resolve_data_request(
        &self,
        request: &GossipDataRequestV2,
        requester_address: IrysAddress,
    ) -> GossipResult<Option<GossipDataV2>> {
        match request {
            GossipDataRequestV2::BlockHeader(block_hash) => {
                let maybe_block = self.block_pool.get_block_header(block_hash).await?;
                if let Some(block) = &maybe_block {
                    if block.poa.chunk.is_none() {
                        error!(
                            block.hash = ?block.block_hash,
                            "Block pool returned a block without a POA chunk"
                        );
                    }
                }
                Ok(maybe_block.map(GossipDataV2::BlockHeader))
            }
            GossipDataRequestV2::BlockBody(block_hash) => {
                debug!(
                    "Node {}: handling block body request for block {:?}",
                    self.gossip_client.mining_address, block_hash
                );
                let maybe_block_body = get_block_body(
                    block_hash,
                    &self.block_pool,
                    &self.mempool,
                    &self.block_tree,
                )
                .await?;
                Ok(maybe_block_body.map(|body| GossipDataV2::BlockBody(Arc::clone(&body))))
            }
            GossipDataRequestV2::ExecutionPayload(evm_block_hash) => {
                debug!(
                    "Node {}: Handling execution payload request for block {:?}",
                    self.gossip_client.mining_address, evm_block_hash
                );
                let maybe_evm_block = self
                    .execution_payload_cache
                    .get_locally_stored_evm_block(evm_block_hash)
                    .await;
                Ok(maybe_evm_block.map(GossipDataV2::ExecutionPayload))
            }
            GossipDataRequestV2::Transaction(tx_id) => {
                debug!(
                    "Node {}: Handling transaction request for tx {:?}",
                    self.gossip_client.mining_address, tx_id
                );

                let vec = vec![*tx_id];
                let mempool_guard = &self.block_pool.mempool_guard;
                let db = &self.block_pool.db;

                // Try commitment txs first
                match get_commitment_tx_in_parallel(&vec, mempool_guard, db).await {
                    Ok(mut result) => {
                        if let Some(tx) = result.pop() {
                            return Ok(Some(GossipDataV2::CommitmentTransaction(tx)));
                        }
                    }
                    Err(e) => {
                        warn!(
                            "Node {}: Failed to retrieve commitment tx {} for peer {}: {}",
                            self.gossip_client.mining_address, tx_id, requester_address, e
                        );
                    }
                }

                // Try data txs
                match get_data_tx_in_parallel(vec, mempool_guard, db).await {
                    Ok(mut result) => {
                        if let Some(tx) = result.pop() {
                            return Ok(Some(GossipDataV2::Transaction(tx)));
                        }
                    }
                    Err(e) => {
                        warn!(
                            "Node {}: Failed to retrieve data tx {} for peer {}: {}",
                            self.gossip_client.mining_address, tx_id, requester_address, e
                        );
                    }
                }

                Ok(None)
            }
            GossipDataRequestV2::Chunk(_chunk_path_hash) => Ok(None),
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

    fn send_gossip_data(
        &self,
        peer: (&IrysPeerId, &PeerListItem),
        data: Arc<GossipDataV2>,
        check_result: &RequestCheckResult,
    ) {
        if check_result.should_update_score() {
            self.gossip_client
                .send_data_and_update_score_for_request(peer, data, &self.peer_list);
        } else {
            self.gossip_client
                .send_data_without_score_update(peer, data);
        }
    }

    fn should_skip_block_validation(
        &self,
        block_height: u64,
        source_miner_address: IrysAddress,
        data_source_ip: std::net::IpAddr,
    ) -> bool {
        self.sync_state.is_syncing_from_a_trusted_peer()
            && self
                .sync_state
                .is_in_trusted_sync_range(block_height as usize)
            && self
                .peer_list
                .is_a_trusted_peer(source_miner_address, data_source_ip)
    }

    /// Fetches a block header from the network with retries, validating the signature on each attempt.
    async fn fetch_header_with_retries(
        &self,
        block_hash: BlockHash,
        use_trusted_peers_only: bool,
    ) -> GossipResult<Arc<IrysBlockHeader>> {
        let mut fetched_header = None;
        let mut failed_attempts: Vec<(Option<IrysPeerId>, GossipError)> = Vec::new();

        for attempt in 1..=HEADER_AND_BODY_RETRIES {
            match self
                .pull_block_header(block_hash, use_trusted_peers_only)
                .await
            {
                Ok((source_peer_id, header)) => {
                    if !header.is_signature_valid() {
                        warn!(
                            "Node: {}: Block {} fetched from {} has an invalid signature (attempt {}/{})",
                            self.gossip_client.mining_address, header.block_hash, source_peer_id, attempt, HEADER_AND_BODY_RETRIES
                        );
                        let error =
                            GossipError::InvalidData(InvalidDataError::InvalidBlockSignature);
                        self.peer_list.decrease_peer_score_by_peer_id(
                            &source_peer_id,
                            ScoreDecreaseReason::BogusData("Invalid block signature".into()),
                        );
                        failed_attempts.push((Some(source_peer_id), error));
                        continue;
                    }
                    fetched_header = Some(header);
                    break;
                }
                Err(e) => {
                    debug!(
                        "Node {}: Failed to pull block header for {} (attempt {}/{}): {}",
                        self.gossip_client.mining_address,
                        block_hash,
                        attempt,
                        HEADER_AND_BODY_RETRIES,
                        e
                    );
                    failed_attempts.push((None, e));
                }
            }
        }

        match fetched_header {
            Some(h) => Ok(h),
            None => {
                let failure_summary = format_failure_summary("block header", &failed_attempts);
                Err(GossipError::Internal(InternalGossipError::Unknown(
                    failure_summary,
                )))
            }
        }
    }

    pub async fn pull_block_body(
        &self,
        header: &IrysBlockHeader,
        use_trusted_peers_only: bool,
        source_peer_id: IrysPeerId,
    ) -> GossipResult<SealedBlock> {
        // Try fetching from the source peer first (the peer that sent us the header)
        if let Some(body) = self
            .try_fetch_body_from_source(header, source_peer_id)
            .await
        {
            return Ok(body);
        }

        debug!(
            "Fetching block body for block {} height {} from the network",
            header.block_hash, header.height
        );

        self.fetch_block_body_from_network_with_retries(header, use_trusted_peers_only)
            .await
    }

    /// Attempts to fetch the block body from the peer that originally sent us the header.
    /// Returns `Some(sealed_block)` on success, `None` on any failure.
    async fn try_fetch_body_from_source(
        &self,
        header: &IrysBlockHeader,
        source_peer_id: IrysPeerId,
    ) -> Option<SealedBlock> {
        let block_hash = header.block_hash;
        let source_peer_item = self.peer_list.get_peer(&source_peer_id)?;

        debug!(
            "Trying to fetch block body for block {} height {} from source peer {}",
            block_hash, header.height, source_peer_id
        );
        let source_peer = (source_peer_id, source_peer_item);
        match self
            .gossip_client
            .pull_block_body_from_peer(header, &source_peer, &self.peer_list)
            .await
        {
            Ok((_peer_id, sealed_block)) => {
                debug!(
                    "Fetched block body for block {} height {} from source peer {}",
                    block_hash, header.height, source_peer_id
                );
                return Some(sealed_block);
            }
            Err(PeerNetworkError::InvalidBlockBody { peer_id, reason }) => {
                warn!(
                    "Source peer {} served invalid block body for block {} height {}: {}",
                    peer_id, block_hash, header.height, reason
                );
                self.peer_list.decrease_peer_score_by_peer_id(
                    &peer_id,
                    ScoreDecreaseReason::BogusData(reason),
                );
            }
            Err(e) => {
                debug!(
                    "Failed to fetch block body from source peer {} for block {} height {}: {}",
                    source_peer_id, block_hash, header.height, e
                );
            }
        }

        None
    }

    /// Fetches a block body from the network with retries, validating tx IDs on each attempt.
    async fn fetch_block_body_from_network_with_retries(
        &self,
        header: &IrysBlockHeader,
        use_trusted_peers_only: bool,
    ) -> GossipResult<SealedBlock> {
        let block_hash = header.block_hash;
        let header_arc = Arc::new(header.clone());
        let mut failed_attempts: Vec<(Option<IrysPeerId>, GossipError)> = Vec::new();

        for attempt in 1..=HEADER_AND_BODY_RETRIES {
            match self
                .gossip_client
                .pull_block_body_from_network(
                    Arc::clone(&header_arc),
                    use_trusted_peers_only,
                    &self.peer_list,
                )
                .await
            {
                Ok((body_source_peer, sealed_block)) => {
                    debug!(
                        "Fetched block body for block {} height {} from peer {:?}",
                        block_hash, header.height, body_source_peer
                    );
                    return Ok(sealed_block);
                }
                Err(PeerNetworkError::InvalidBlockBody { peer_id, reason }) => {
                    warn!(
                        "Node {}: Peer {} served invalid block body for block {} height {} (attempt {}/{}): {}",
                        self.gossip_client.mining_address, peer_id, block_hash, header.height, attempt, HEADER_AND_BODY_RETRIES, reason
                    );
                    self.peer_list.decrease_peer_score_by_peer_id(
                        &peer_id,
                        ScoreDecreaseReason::BogusData(reason.clone()),
                    );
                    let error = GossipError::Internal(InternalGossipError::Unknown(reason));
                    failed_attempts.push((Some(peer_id), error));
                }
                Err(e) => {
                    let error = GossipError::from(e);
                    debug!(
                        "Node {}: Failed to pull block body for {} height {} (attempt {}/{}): {}",
                        self.gossip_client.mining_address,
                        block_hash,
                        header.height,
                        attempt,
                        HEADER_AND_BODY_RETRIES,
                        error
                    );
                    failed_attempts.push((None, error));
                }
            }
        }

        let failure_summary = format_failure_summary("block body", &failed_attempts);
        Err(GossipError::Internal(InternalGossipError::Unknown(
            failure_summary,
        )))
    }
}

async fn get_block_body<M: MempoolFacade, B: BlockDiscoveryFacade>(
    block_hash: &BlockHash,
    block_pool: &BlockPool<B, M>,
    mempool: &M,
    block_tree: &BlockTreeReadGuard,
) -> GossipResult<Option<Arc<BlockBody>>> {
    // Check the pool caches (in-flight + recently-processed)
    if let Some(block_body) = block_pool.get_cached_block_body(block_hash).await {
        return Ok(Some(block_body));
    }

    // Check the block tree (code block to drop the guard)
    {
        let from_tree = block_tree.read().get_sealed_block(block_hash);
        if let Some(sealed) = from_tree {
            return Ok(Some(Arc::new(sealed.to_block_body())));
        }
    }

    // Expensive path: reconstruct from mempool/DB
    let maybe_block_header = block_pool.get_block_header(block_hash).await?;
    if let Some(block_header) = &maybe_block_header {
        let block_body = build_block_body_for_processed_block_header(
            block_header,
            &mempool.get_internal_read_guard().await,
            &block_pool.db,
        )
        .await
        .map_err(|err| {
            error!(
                "Error building block body for block {:?}: {:?}",
                block_hash, err
            );
            GossipError::Internal(InternalGossipError::Unknown(format!(
                "Error building block body for block {}: {}",
                block_hash, err
            )))
        })?;
        debug!("Successfully built block body for block {:?}", block_hash);
        Ok(Some(Arc::new(block_body)))
    } else {
        warn!(
            "Didn't find the block header to build the block body for the block {:?}",
            block_hash
        );
        Ok(None)
    }
}
