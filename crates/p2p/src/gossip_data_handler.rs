use crate::{
    block_pool::{order_transactions_for_block, BlockPool},
    cache::GossipCache,
    rate_limiting::DataRequestTracker,
    types::{AdvisoryGossipError, InternalGossipError, InvalidDataError},
    GossipClient, GossipError, GossipResult,
};
use core::net::SocketAddr;
use futures::stream::{self, StreamExt as _};
use irys_actors::{
    block_discovery::{BlockDiscoveryFacade, BlockTransactions},
    AdvisoryChunkIngressError, ChunkIngressError, CriticalChunkIngressError, MempoolFacade,
};
use irys_api_client::ApiClient;
use irys_database::reth_db::Database as _;
use irys_domain::chain_sync_state::ChainSyncState;
use irys_domain::{ExecutionPayloadCache, PeerList, ScoreDecreaseReason};
use irys_types::IrysAddress;
use irys_types::{
    BlockHash, CommitmentTransaction, DataTransactionHeader, EvmBlockHash, GossipCacheKey,
    GossipData, GossipDataRequest, GossipRequest, IngressProof, IrysBlockHeader,
    IrysTransactionResponse, PeerListItem, UnpackedChunk, H256,
};
use rand::prelude::SliceRandom as _;
use reth::builder::Block as _;
use reth::primitives::Block;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{debug, error, instrument, warn, Instrument as _, Span};

pub(crate) const MAX_PEERS_TO_SELECT_FROM: usize = 15;
pub(crate) const MAX_TX_PEERS_TO_TRY: usize = 7;

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
            .pull_block_from_network(block_hash, use_trusted_peers_only, &self.peer_list)
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
            peer_info.address.api,
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
            peer_info.address.api,
            peer_info.address.gossip,
        )
        .in_current_span()
        .await
    }

    #[instrument(skip_all, fields(block.hash = ?block_header_request.data.block_hash), parent = &self.span)]
    pub(crate) async fn handle_block_header(
        &self,
        block_header_request: GossipRequest<IrysBlockHeader>,
        source_api_address: SocketAddr,
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

        // Fetch all transactions using the unified method (cache + mempool + network)
        let block_transactions = self
            .fetch_and_build_block_transactions(
                &block_header,
                Some((source_api_address, source_miner_address)),
            )
            .await?;

        let is_syncing_from_a_trusted_peer = self.sync_state.is_syncing_from_a_trusted_peer();
        let is_in_the_trusted_sync_range = self
            .sync_state
            .is_in_trusted_sync_range(block_header.height as usize);

        let skip_block_validation = is_syncing_from_a_trusted_peer
            && is_in_the_trusted_sync_range
            && self
                .peer_list
                .is_a_trusted_peer(source_miner_address, data_source_ip.ip());

        self.block_pool
            .process_block::<A>(
                Arc::new(block_header),
                block_transactions,
                skip_block_validation,
            )
            .await?;
        Ok(())
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
            GossipDataRequest::Block(block_hash) => {
                let maybe_block = self.block_pool.get_block_data(&block_hash).await?;

                match maybe_block {
                    Some(block) => {
                        if block.poa.chunk.is_none() {
                            error!(
                                target = "p2p::gossip_data_handler::handle_get_data",
                                block.hash = ?block.block_hash,
                                "Block pool returned a block without a POA chunk"
                            );
                        }
                        let data = Arc::new(GossipData::Block(block));
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
            GossipDataRequest::Block(block_hash) => {
                let maybe_block = self.block_pool.get_block_data(&block_hash).await?;
                if let Some(block) = &maybe_block {
                    if block.poa.chunk.is_none() {
                        error!(
                            target = "p2p::gossip_data_handler::handle_get_data_sync",
                            block.hash = ?block.block_hash,
                            "Block pool returned a block without a POA chunk"
                        );
                    }
                }
                Ok(maybe_block.map(GossipData::Block))
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

    /// Fetch all transactions for a block, using cache, mempool, and network fallback.
    ///
    /// This method combines multiple data sources:
    /// 1. Cached transactions from the block pool
    /// 2. Mempool batch query via MempoolReadGuard
    /// 3. Network fetching for any still-missing transactions
    ///
    /// The `source_peer` parameter is an optional hint for which peer to try first
    /// when fetching missing transactions from the network.
    #[tracing::instrument(level = "trace", skip_all, err, fields(block.hash = ?block.block_hash))]
    pub async fn fetch_and_build_block_transactions(
        &self,
        block: &IrysBlockHeader,
        source_peer: Option<(SocketAddr, IrysAddress)>,
    ) -> GossipResult<BlockTransactions> {
        let block_hash = block.block_hash;

        // Take any cached transactions for this block
        let cached_txs = self.block_pool.take_cached_txs_for_block(&block_hash).await;

        // Separate cached txs by type
        let (cached_data_txs, cached_commitment_txs): (Vec<_>, Vec<_>) = cached_txs
            .into_iter()
            .partition(|tx| matches!(tx, IrysTransactionResponse::Storage(_)));

        let mut fetched_data_txs: Vec<DataTransactionHeader> = cached_data_txs
            .into_iter()
            .filter_map(|tx| match tx {
                IrysTransactionResponse::Storage(header) => Some(header),
                _ => None,
            })
            .collect();

        let mut fetched_commitment_txs: Vec<CommitmentTransaction> = cached_commitment_txs
            .into_iter()
            .filter_map(|tx| match tx {
                IrysTransactionResponse::Commitment(tx) => Some(tx),
                _ => None,
            })
            .collect();

        // Collect cached tx IDs for quick lookup
        let cached_data_ids: HashSet<H256> = fetched_data_txs.iter().map(|tx| tx.id).collect();
        let cached_commitment_ids: HashSet<H256> =
            fetched_commitment_txs.iter().map(|tx| tx.id).collect();

        // Collect required tx IDs from block header
        let data_tx_ids: Vec<H256> = block
            .data_ledgers
            .iter()
            .flat_map(|ledger| ledger.tx_ids.0.clone())
            .collect();
        let commitment_tx_ids: Vec<H256> = block
            .system_ledgers
            .iter()
            .flat_map(|ledger| ledger.tx_ids.0.clone())
            .collect();

        // Filter out already-cached IDs
        let data_ids_to_query: Vec<H256> = data_tx_ids
            .iter()
            .filter(|id| !cached_data_ids.contains(id))
            .copied()
            .collect();
        let commitment_ids_to_query: Vec<H256> = commitment_tx_ids
            .iter()
            .filter(|id| !cached_commitment_ids.contains(id))
            .copied()
            .collect();

        debug!(
            "Batch querying mempool for {} data txs and {} commitment txs for block {} height {} (cached: {} data, {} commitment)",
            data_ids_to_query.len(),
            commitment_ids_to_query.len(),
            block_hash,
            block.height,
            cached_data_ids.len(),
            cached_commitment_ids.len()
        );

        // Query mempool+DB in parallel for remaining transactions
        // Note: We query both sources in parallel and combine results, allowing partial matches
        // since any missing txs will be fetched from network in the next step.
        let mempool_guard = &self.block_pool.mempool_guard;
        let db = &self.block_pool.db;

        // Query mempool and DB in parallel for data transactions
        let (mempool_data, db_data) =
            tokio::join!(mempool_guard.get_data_txs(&data_ids_to_query), async {
                let db_tx = db.tx().ok()?;
                let mut results = std::collections::HashMap::new();
                for tx_id in &data_ids_to_query {
                    if let Ok(Some(header)) = irys_database::tx_header_by_txid(&db_tx, tx_id) {
                        results.insert(*tx_id, header);
                    }
                }
                Some(results)
            });

        // Query mempool and DB in parallel for commitment transactions
        let (mempool_commitment, db_commitment) = tokio::join!(
            mempool_guard.get_commitment_txs(&commitment_ids_to_query),
            async {
                let db_tx = db.tx().ok()?;
                let mut results = std::collections::HashMap::new();
                for tx_id in &commitment_ids_to_query {
                    if let Ok(Some(tx)) = irys_database::commitment_tx_by_txid(&db_tx, tx_id) {
                        results.insert(*tx_id, tx);
                    }
                }
                Some(results)
            }
        );

        // Combine results, preferring mempool (for promoted_height updates)
        let mut found_data_map = mempool_data;
        if let Some(db_results) = db_data {
            for (id, header) in db_results {
                found_data_map.entry(id).or_insert(header);
            }
        }

        let mut found_commitment_map = mempool_commitment;
        if let Some(db_results) = db_commitment {
            for (id, tx) in db_results {
                found_commitment_map.entry(id).or_insert(tx);
            }
        }

        debug!(
            "Found {} data txs and {} commitment txs in mempool+DB for block {} height {}",
            found_data_map.len(),
            found_commitment_map.len(),
            block_hash,
            block.height
        );

        // Add results to our collection
        fetched_data_txs.extend(found_data_map.into_values());
        fetched_commitment_txs.extend(found_commitment_map.into_values());

        // Build sets of what we have now
        let have_data_ids: HashSet<H256> = fetched_data_txs.iter().map(|tx| tx.id).collect();
        let have_commitment_ids: HashSet<H256> =
            fetched_commitment_txs.iter().map(|tx| tx.id).collect();

        // Step 4: Determine what's still missing (not in cache, mempool, or DB)
        let missing_data_ids: Vec<H256> = data_tx_ids
            .iter()
            .filter(|id| !have_data_ids.contains(id))
            .copied()
            .collect();
        let missing_commitment_ids: Vec<H256> = commitment_tx_ids
            .iter()
            .filter(|id| !have_commitment_ids.contains(id))
            .copied()
            .collect();

        let missing_tx_ids: Vec<H256> = missing_data_ids
            .iter()
            .chain(missing_commitment_ids.iter())
            .copied()
            .collect();

        // Step 5: Fetch missing transactions from network
        if !missing_tx_ids.is_empty() {
            debug!(
                "Missing transactions to fetch from network for block {} height {}: {} data, {} commitment",
                block_hash,
                block.height,
                missing_data_ids.len(),
                missing_commitment_ids.len()
            );

            // Remove them from the mempool's blacklist
            self.mempool
                .remove_from_blacklist(missing_tx_ids.clone())
                .await
                .map_err(|error| {
                    error!("Failed to remove txs from the mempool blacklist");
                    GossipError::unknown(&error)
                })?;

            // Fetch from network
            let (network_data_txs, network_commitment_txs) = self
                .fetch_missing_txs_from_network(missing_tx_ids, source_peer)
                .await?;

            fetched_data_txs.extend(network_data_txs);
            fetched_commitment_txs.extend(network_commitment_txs);
        }

        debug!(
            "Got all transactions for block {} height {}: {} data, {} commitment. Building BlockTransactions.",
            block_hash,
            block.height,
            fetched_data_txs.len(),
            fetched_commitment_txs.len()
        );

        // Step 6: Order transactions into BlockTransactions structure
        Ok(order_transactions_for_block(
            block,
            fetched_data_txs,
            fetched_commitment_txs,
        ))
    }

    /// Fetch missing transactions from the network with peer fallback.
    ///
    /// Tries the source peer first (if provided), then falls back to random selection
    /// from top active peers.
    async fn fetch_missing_txs_from_network(
        &self,
        missing_tx_ids: Vec<H256>,
        source_peer: Option<(SocketAddr, IrysAddress)>,
    ) -> GossipResult<(Vec<DataTransactionHeader>, Vec<CommitmentTransaction>)> {
        let mut fetched_data_txs = Vec::new();
        let mut fetched_commitment_txs = Vec::new();

        debug!(
            "Fetching {} missing transactions from the network",
            missing_tx_ids.len()
        );

        // Fetch missing transactions in parallel with a concurrency limit of 10
        let fetch_results: Vec<_> = stream::iter(missing_tx_ids)
            .map(|tx_id_to_fetch| {
                async move {
                    let mut fetched: Option<(IrysTransactionResponse, IrysAddress)> = None;
                    let mut last_err: Option<String> = None;

                    // Try source peer first if provided
                    if let Some((source_api_address, source_miner_address)) = source_peer {
                        match self
                            .api_client
                            .get_transaction(source_api_address, tx_id_to_fetch)
                            .await
                        {
                            Ok(resp) => {
                                fetched = Some((resp, source_miner_address));
                            }
                            Err(e) => {
                                warn!(
                                    "Failed to fetch tx {} from source peer {}: {}",
                                    tx_id_to_fetch, source_api_address, e
                                );
                                last_err = Some(e.to_string());
                            }
                        }
                    }

                    // If source failed or not provided, try random peers from top active
                    if fetched.is_none() {
                        let mut exclude = HashSet::new();
                        if let Some((_, source_miner_address)) = source_peer {
                            exclude.insert(source_miner_address);
                        }
                        let mut top_peers = self
                            .peer_list
                            .top_active_peers(Some(MAX_PEERS_TO_SELECT_FROM), Some(exclude));
                        top_peers.shuffle(&mut rand::thread_rng());
                        top_peers.truncate(MAX_TX_PEERS_TO_TRY);

                        for (peer_addr, peer_item) in top_peers {
                            match self
                                .api_client
                                .get_transaction(peer_item.address.api, tx_id_to_fetch)
                                .await
                            {
                                Ok(resp) => {
                                    fetched = Some((resp, peer_addr));
                                    break;
                                }
                                Err(e) => {
                                    warn!(
                                        "Failed to fetch tx {} from peer {}: {}",
                                        tx_id_to_fetch, peer_item.address.api, e
                                    );
                                    last_err = Some(e.to_string());
                                    continue;
                                }
                            }
                        }
                    }

                    match fetched {
                        Some((tx_response, from_miner_addr)) => Ok((tx_response, from_miner_addr)),
                        None => {
                            let source_info = source_peer
                                .map(|(addr, _)| format!(" from source {}", addr))
                                .unwrap_or_default();
                            let err_msg = format!(
                                "Failed to fetch transaction {:?}{} and top peers{}",
                                tx_id_to_fetch,
                                source_info,
                                last_err
                                    .as_ref()
                                    .map(|e| format!("; last error: {}", e))
                                    .unwrap_or_default()
                            );
                            Err(err_msg)
                        }
                    }
                }
            })
            .buffer_unordered(10)
            .collect()
            .await;

        // Process network fetch results
        for result in fetch_results {
            match result {
                Ok((tx_response, from_miner_addr)) => {
                    let tx_id = match &tx_response {
                        IrysTransactionResponse::Commitment(commitment_tx) => {
                            fetched_commitment_txs.push(commitment_tx.clone());
                            commitment_tx.id
                        }
                        IrysTransactionResponse::Storage(tx) => {
                            fetched_data_txs.push(tx.clone());
                            tx.id
                        }
                    };

                    debug!("Fetched transaction {:?} (unverified) from network", tx_id);
                    // Record that we have seen this transaction from the peer that served it
                    self.cache
                        .record_seen(from_miner_addr, GossipCacheKey::Transaction(tx_id))?;
                }
                Err(err_msg) => {
                    error!("{}", err_msg);
                    return Err(GossipError::Network(err_msg));
                }
            }
        }

        Ok((fetched_data_txs, fetched_commitment_txs))
    }
}
