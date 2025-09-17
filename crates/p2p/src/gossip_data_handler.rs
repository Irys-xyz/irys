use crate::{
    block_pool::BlockPool,
    cache::GossipCache,
    rate_limiting::DataRequestTracker,
    types::{InternalGossipError, InvalidDataError},
    GossipClient, GossipError, GossipResult,
};
use core::net::SocketAddr;
use irys_actors::{block_discovery::BlockDiscoveryFacade, ChunkIngressError, MempoolFacade};
use irys_api_client::ApiClient;
use irys_domain::chain_sync_state::ChainSyncState;
use irys_domain::{ExecutionPayloadCache, PeerList, ScoreDecreaseReason};
use irys_primitives::Address;
use irys_types::{
    BlockHash, CommitmentTransaction, DataTransactionHeader, EvmBlockHash, GossipCacheKey,
    GossipData, GossipDataRequest, GossipRequest, IngressProof, IrysBlockHeader,
    IrysTransactionResponse, PeerListItem, UnpackedChunk, H256,
};
use reth::builder::Block as _;
use reth::primitives::Block;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::log::warn;
use tracing::{debug, error, Span};

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
                    ChunkIngressError::UnknownTransaction => {
                        // TODO:
                        //  I suppose we have to ask the peer for transaction,
                        //  but what if it doesn't have one?
                        Ok(())
                    }
                    // ===== External invalid data errors
                    ChunkIngressError::InvalidProof => Err(GossipError::InvalidData(
                        InvalidDataError::ChunkInvalidProof,
                    )),
                    ChunkIngressError::InvalidDataHash => Err(GossipError::InvalidData(
                        InvalidDataError::ChinkInvalidDataHash,
                    )),
                    ChunkIngressError::InvalidChunkSize => Err(GossipError::InvalidData(
                        InvalidDataError::ChunkInvalidChunkSize,
                    )),
                    ChunkIngressError::InvalidDataSize => Err(GossipError::InvalidData(
                        InvalidDataError::ChunkInvalidDataSize,
                    )),
                    ChunkIngressError::PreHeaderOversizedBytes => Err(GossipError::InvalidData(
                        InvalidDataError::ChunkInvalidChunkSize,
                    )),
                    ChunkIngressError::PreHeaderOversizedDataPath => Err(GossipError::InvalidData(
                        InvalidDataError::ChunkInvalidProof,
                    )),
                    ChunkIngressError::PreHeaderOffsetExceedsCap => Err(GossipError::InvalidData(
                        InvalidDataError::ChunkInvalidChunkSize,
                    )),
                    // ===== Internal errors
                    ChunkIngressError::DatabaseError => {
                        Err(GossipError::Internal(InternalGossipError::Database))
                    }
                    ChunkIngressError::ServiceUninitialized => Err(GossipError::Internal(
                        InternalGossipError::ServiceUninitialized,
                    )),
                    ChunkIngressError::Other(other) => {
                        Err(GossipError::Internal(InternalGossipError::Unknown(other)))
                    }
                }
            }
        }
    }

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
            .is_known_transaction(tx_id)
            .await
            .map_err(|e| {
                GossipError::Internal(InternalGossipError::Unknown(format!(
                    "is_known_transaction() errored: {:?}",
                    e
                )))
            })?
        {
            debug!(
                "Node {}: Transaction has already been handled, skipping",
                self.gossip_client.mining_address
            );
            return Ok(());
        }

        match self
            .mempool
            .handle_data_transaction_ingress(tx)
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
                error!("Error when sending transaction to mempool: {:?}", error);
                Err(error)
            }
        }
    }

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
                error!("Error when sending ingress proof to mempool: {:?}", error);
                Err(error)
            }
        }
    }

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
            .is_known_transaction(tx_id)
            .await
            .map_err(|e| {
                GossipError::Internal(InternalGossipError::Unknown(format!(
                    "is_known_transaction() errored: {:?}",
                    e
                )))
            })?
        {
            debug!(
                "Node {}: Commitment Transaction has already been handled, skipping",
                self.gossip_client.mining_address
            );
            return Ok(());
        }

        match self
            .mempool
            .handle_commitment_transaction_ingress(tx)
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
                    "Error when sending commitment transaction to mempool: {:?}",
                    error
                );
                Err(error)
            }
        }
    }

    /// Pulls a block from the network and sends it to the BlockPool for processing
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
                data: irys_block.as_ref().clone(),
            },
            peer_info.address.api,
            peer_info.address.gossip,
        )
        .await
    }

    /// Pulls a block from a specific peer and sends it to the BlockPool for processing
    pub async fn pull_and_process_block_from_peer(
        &self,
        block_hash: BlockHash,
        peer: &(irys_types::Address, PeerListItem),
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
                data: irys_block.as_ref().clone(),
            },
            peer_info.address.api,
            peer_info.address.gossip,
        )
        .await
    }

    pub(crate) async fn handle_block_header(
        &self,
        block_header_request: GossipRequest<IrysBlockHeader>,
        source_api_address: SocketAddr,
        data_source_ip: SocketAddr,
    ) -> GossipResult<()> {
        let span = self.span.clone();
        let _span = span.enter();
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

        if self.sync_state.is_syncing()
            && block_header.height > (self.sync_state.sync_target_height() + 1) as u64
        {
            debug!(
                "Node {}: Block {} is out of the sync range, skipping",
                self.gossip_client.mining_address, block_hash
            );
            return Ok(());
        }

        let is_block_requested_by_the_pool = self.block_pool.is_block_requested(&block_hash).await;
        let has_block_already_been_received = self.cache.seen_block_from_any_peer(&block_hash)?;

        // This check must be after we've added the block to the cache, otherwise we won't be
        // able to keep track of which peers seen what
        if has_block_already_been_received && !is_block_requested_by_the_pool {
            debug!(
                "Node {}: Block {} already seen and not requested by the pool, skipping",
                self.gossip_client.mining_address, block_header.block_hash
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
            self.peer_list
                .decrease_peer_score(&source_miner_address, ScoreDecreaseReason::BogusData);

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
                "Node {}: Block {} has already been processed, skipping",
                self.gossip_client.mining_address, block_header.block_hash
            );
            return Ok(());
        }

        debug!(
            "Node {}: Block {} has not been processed yet, starting processing",
            self.gossip_client.mining_address, block_header.block_hash
        );

        let mut missing_tx_ids = Vec::new();

        for tx_id in block_header
            .data_ledgers
            .iter()
            .flat_map(|ledger| ledger.tx_ids.0.clone())
        {
            if !self.is_known_tx(tx_id).await? {
                missing_tx_ids.push(tx_id);
            }
        }

        for system_tx_id in block_header
            .system_ledgers
            .iter()
            .flat_map(|ledger| ledger.tx_ids.0.clone())
        {
            if !self.is_known_tx(system_tx_id).await? {
                missing_tx_ids.push(system_tx_id);
            }
        }

        if !missing_tx_ids.is_empty() {
            debug!("Missing transactions to fetch: {:?}", missing_tx_ids);
        }

        // remove them from the mempool's blacklist
        self.mempool
            .remove_from_blacklist(missing_tx_ids.clone())
            .await
            .map_err(|error| {
                error!("Failed to remove txs from mempool blacklist");
                GossipError::unknown(&error)
            })?;

        // TODO: make this parallel with a limited number of concurrent fetches, maybe 10?
        // Fetch and process each missing transaction one-by-one with retries
        for tx_id_to_fetch in missing_tx_ids {
            // Try source peer first
            let mut fetched: Option<(IrysTransactionResponse, irys_types::Address)> = None;
            let mut last_err: Option<String> = None;

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
                        "Failed to fetch tx {:?} from source peer {}: {:?}",
                        tx_id_to_fetch, source_api_address, e
                    );
                    last_err = Some(e.to_string());
                }
            }

            // If source failed, try top 5 active peers (excluding the source)
            if fetched.is_none() {
                let mut exclude = std::collections::HashSet::new();
                exclude.insert(source_miner_address);
                let top_peers = self.peer_list.top_active_peers(Some(5), Some(exclude));

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
                                "Failed to fetch tx {:?} from peer {}: {:?}",
                                tx_id_to_fetch, peer_item.address.api, e
                            );
                            last_err = Some(e.to_string());
                            continue;
                        }
                    }
                }
            }

            let Some((tx_response, from_miner_addr)) = fetched else {
                let err_msg = format!(
                    "Failed to fetch transaction {:?} from source {} and top peers{:?}",
                    tx_id_to_fetch,
                    source_api_address,
                    last_err
                        .as_ref()
                        .map(|e| format!("; last error: {}", e))
                        .unwrap_or_default()
                );
                error!("{:?}", err_msg);
                return Err(GossipError::Network(err_msg));
            };

            // Process the fetched transaction immediately
            let (tx_id, mempool_response) = match tx_response {
                IrysTransactionResponse::Commitment(commitment_tx) => {
                    let id = commitment_tx.id;
                    (
                        id,
                        self.mempool
                            .handle_commitment_transaction_ingress(commitment_tx)
                            .await,
                    )
                }
                IrysTransactionResponse::Storage(tx) => {
                    let id = tx.id;
                    (id, self.mempool.handle_data_transaction_ingress(tx).await)
                }
            };

            match mempool_response.map_err(GossipError::from) {
                Ok(()) | Err(GossipError::TransactionIsAlreadyHandled) => {
                    debug!("Transaction {:?} sent to mempool", tx_id);
                    // Record that we have seen this transaction from the peer that served it
                    self.cache
                        .record_seen(from_miner_addr, GossipCacheKey::Transaction(tx_id))?;
                }
                Err(error) => {
                    error!(
                        "Error when sending transaction {:?} to mempool: {:?}",
                        tx_id, error
                    );
                    return Err(error);
                }
            }
        }

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
            .process_block::<A>(Arc::new(block_header), skip_block_validation)
            .await
            .map_err(GossipError::BlockPool)?;
        Ok(())
    }

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

    async fn is_known_tx(&self, tx_id: H256) -> Result<bool, GossipError> {
        self.mempool.is_known_transaction(tx_id).await.map_err(|e| {
            GossipError::Internal(InternalGossipError::Unknown(format!(
                "is_known_transaction() errored: {:?}",
                e
            )))
        })
    }

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
                let block_result = self.block_pool.get_block_data(&block_hash).await;

                let maybe_block = block_result.map_err(GossipError::BlockPool)?;

                match maybe_block {
                    Some(block) => {
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

    pub(crate) async fn handle_get_data_sync(
        &self,
        request: GossipRequest<GossipDataRequest>,
    ) -> GossipResult<Option<GossipData>> {
        match request.data {
            GossipDataRequest::Block(block_hash) => {
                let maybe_block = self
                    .block_pool
                    .get_block_data(&block_hash)
                    .await
                    .map_err(GossipError::BlockPool)?;
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

    pub(crate) async fn handle_get_stake_and_pledge_whitelist(&self) -> Vec<Address> {
        self.mempool
            .get_stake_and_pledge_whitelist()
            .await
            .into_iter()
            .collect()
    }

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
                    "get_stake_and_pledge_whitelist() errored: {:?}",
                    e
                )))
            })
    }
}
