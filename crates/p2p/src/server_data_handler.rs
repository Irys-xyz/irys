use crate::execution_payload_provider::ExecutionPayloadProvider;
use crate::peer_list::{PeerList, ScoreDecreaseReason};
use crate::{
    block_pool::BlockPool,
    cache::GossipCache,
    sync::SyncState,
    types::{GossipDataRequest, InternalGossipError, InvalidDataError},
    GossipClient, GossipError, GossipResult,
};
use alloy_core::primitives::keccak256;
use base58::ToBase58 as _;
use core::net::SocketAddr;
use irys_actors::{
    block_discovery::BlockDiscoveryFacade,
    mempool_service::{ChunkIngressError, MempoolFacade},
};
use irys_api_client::ApiClient;
use irys_types::{
    CommitmentTransaction, GossipCacheKey, GossipData, GossipRequest, IrysBlockHeader,
    IrysTransactionHeader, IrysTransactionResponse, PeerListItem, UnpackedChunk, H256,
};
use reth::builder::Block as _;
use reth::primitives::Block;
use std::sync::Arc;
use tracing::log::warn;
use tracing::{debug, error, Span};

/// Handles data received by the `GossipServer`
#[derive(Debug)]
pub(crate) struct GossipServerDataHandler<TMempoolFacade, TBlockDiscovery, TApiClient, TPeerList>
where
    TMempoolFacade: MempoolFacade,
    TBlockDiscovery: BlockDiscoveryFacade,
    TApiClient: ApiClient,
    TPeerList: PeerList,
{
    pub mempool: TMempoolFacade,
    pub block_pool: BlockPool<TPeerList, TBlockDiscovery>,
    pub cache: Arc<GossipCache>,
    pub api_client: TApiClient,
    pub gossip_client: GossipClient,
    pub peer_list: TPeerList,
    pub sync_state: SyncState,
    /// Tracing span
    pub span: Span,
    pub execution_payload_provider: ExecutionPayloadProvider<TPeerList>,
}

impl<M, B, A, P> Clone for GossipServerDataHandler<M, B, A, P>
where
    M: MempoolFacade,
    B: BlockDiscoveryFacade,
    A: ApiClient,
    P: PeerList,
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
            execution_payload_provider: self.execution_payload_provider.clone(),
        }
    }
}

impl<M, B, A, P> GossipServerDataHandler<M, B, A, P>
where
    M: MempoolFacade,
    B: BlockDiscoveryFacade,
    A: ApiClient,
    P: PeerList,
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
        transaction_request: GossipRequest<IrysTransactionHeader>,
    ) -> GossipResult<()> {
        debug!(
            "Node {}: Gossip transaction received from peer {}: {:?}",
            self.gossip_client.mining_address,
            transaction_request.miner_address,
            transaction_request.data.id.0.to_base58()
        );
        let tx = transaction_request.data;
        let source_miner_address = transaction_request.miner_address;
        let tx_id = tx.id;

        let already_seen = self.cache.seen_transaction_from_any_peer(&tx_id)?;
        self.cache
            .record_seen(source_miner_address, GossipCacheKey::Transaction(tx_id))?;

        if already_seen {
            debug!(
                "Node {}: Transaction {} is already recorded in the cache, skipping",
                self.gossip_client.mining_address,
                tx_id.0.to_base58()
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
                Ok(())
            }
            Err(error) => {
                error!("Error when sending transaction to mempool: {:?}", error);
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
            transaction_request.data.id.0.to_base58()
        );
        let tx = transaction_request.data;
        let source_miner_address = transaction_request.miner_address;
        let tx_id = tx.id;

        let already_seen = self.cache.seen_transaction_from_any_peer(&tx_id)?;
        self.cache
            .record_seen(source_miner_address, GossipCacheKey::Transaction(tx_id))?;

        if already_seen {
            debug!(
                "Node {}: Commitment Transaction {} is already recorded in the cache, skipping",
                self.gossip_client.mining_address,
                tx_id.0.to_base58()
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

    pub(crate) async fn handle_block_header_request(
        &self,
        block_header_request: GossipRequest<IrysBlockHeader>,
        source_api_address: SocketAddr,
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

        let block_seen = self.cache.seen_block_from_any_peer(&block_hash)?;

        // Record block in cache
        self.cache
            .record_seen(source_miner_address, GossipCacheKey::Block(block_hash))?;

        // This check must be after we've added the block to the cache, otherwise we won't be
        // able to keep track of which peers seen what
        if block_seen {
            debug!(
                "Node {}: Block {} already seen, skipping",
                self.gossip_client.mining_address,
                block_header.block_hash.0.to_base58()
            );
            return Ok(());
        }

        let expected_block_hash: [u8; 32] = keccak256(block_header.signature.as_bytes()).into();
        let is_block_hash_is_valid = block_header.block_hash.0 == expected_block_hash;
        if !is_block_hash_is_valid || !block_header.is_signature_valid() {
            warn!(
                "Node: {}: Block {} has an invalid signature",
                self.gossip_client.mining_address,
                block_header.block_hash.0.to_base58()
            );
            if let Err(peer_list_err) = self
                .peer_list
                .decrease_peer_score(&source_miner_address, ScoreDecreaseReason::BogusData)
                .await
            {
                error!("Failed to decrease peer score: {:?}", peer_list_err);
            }
            return Err(GossipError::InvalidData(
                InvalidDataError::InvalidBlockSignature,
            ));
        }

        let has_block_already_been_processed = self
            .block_pool
            .is_block_processing_or_processed(&block_header.block_hash, block_header.height)
            .await;

        if has_block_already_been_processed {
            debug!(
                "Node {}: Block {} has already been processed, skipping",
                self.gossip_client.mining_address,
                block_header.block_hash.0.to_base58()
            );
            return Ok(());
        }

        debug!(
            "Node {}: Block {} has not been processed yet, starting processing",
            self.gossip_client.mining_address,
            block_header.block_hash.0.to_base58()
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

        // Fetch missing transactions from the source peer
        let missing_txs = self
            .api_client
            .get_transactions(source_api_address, &missing_tx_ids)
            .await
            .map_err(|error| {
                error!(
                    "Failed to fetch transactions from peer {}: {}",
                    source_api_address, error
                );
                GossipError::unknown(&error)
            })?;

        // Process each transaction
        for tx_response in missing_txs {
            let tx_id;
            let mempool_response = match tx_response {
                IrysTransactionResponse::Commitment(commitment_tx) => {
                    tx_id = commitment_tx.id;
                    self.mempool
                        .handle_commitment_transaction_ingress(commitment_tx)
                        .await
                }
                IrysTransactionResponse::Storage(tx) => {
                    tx_id = tx.id;
                    self.mempool.handle_data_transaction_ingress(tx).await
                }
            };

            match mempool_response.map_err(GossipError::from) {
                Ok(()) | Err(GossipError::TransactionIsAlreadyHandled) => {
                    debug!("Transaction sent to mempool");
                    self.cache
                        .record_seen(source_miner_address, GossipCacheKey::Transaction(tx_id))?
                }
                Err(error) => {
                    error!("Error when sending transaction to mempool: {:?}", error);
                    return Err(error);
                }
            }
        }

        self.block_pool
            .process_block(block_header)
            .await
            .map_err(GossipError::BlockPool)?;
        Ok(())
    }

    pub(crate) async fn handle_execution_payload(
        &self,
        execution_payload_request: GossipRequest<Block>,
    ) -> GossipResult<()> {
        let source_miner_address = execution_payload_request.miner_address;
        let evm_block = execution_payload_request.data;
        let sealed_block = evm_block.seal_slow();

        let evm_block_hash = sealed_block.hash();
        let payload_already_seen_before = self
            .cache
            .seen_execution_payload_from_any_peer(&evm_block_hash)?;
        let expecting_payload = self
            .execution_payload_provider
            .is_waiting_for_payload(&evm_block_hash)
            .await;

        // Record payload as seen from the source peer
        self.cache.record_seen(
            source_miner_address,
            GossipCacheKey::ExecutionPayload(evm_block_hash),
        )?;

        if payload_already_seen_before && !expecting_payload {
            debug!(
                "Node {}: Execution payload for EVM block {:?} already seen, and no service requested it to be fetched again, skipping",
                self.gossip_client.mining_address,
                evm_block_hash
            );
            return Ok(());
        }

        self.execution_payload_provider
            .add_payload_to_cache(sealed_block)
            .await;
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
    ) -> GossipResult<bool> {
        match request.data {
            GossipDataRequest::Block(block_hash) => {
                let block_result = self.block_pool.get_block_data(&block_hash).await;

                let maybe_block = block_result.map_err(GossipError::BlockPool)?;

                match maybe_block {
                    Some(block) => {
                        match self
                            .gossip_client
                            .send_data_and_update_score(
                                (&request.miner_address, peer_info),
                                &GossipData::Block(block),
                                &self.peer_list,
                            )
                            .await
                        {
                            Ok(()) => {}
                            Err(error) => {
                                error!("Failed to send block to peer: {}", error);
                            }
                        }
                        Ok(true)
                    }
                    None => Ok(false),
                }
            }
            GossipDataRequest::Transaction(_tx_hash) => Ok(false),
            GossipDataRequest::ExecutionPayload(evm_block_hash) => {
                debug!(
                    "Node {}: Handling execution payload request for block {:?}",
                    self.gossip_client.mining_address, evm_block_hash
                );
                let maybe_evm_block = self
                    .execution_payload_provider
                    .get_locally_stored_evm_block(&evm_block_hash)
                    .await;

                match maybe_evm_block {
                    Some(evm_block) => self
                        .gossip_client
                        .send_data_and_update_score(
                            (&request.miner_address, peer_info),
                            &GossipData::ExecutionPayload(evm_block),
                            &self.peer_list,
                        )
                        .await
                        .map(|()| true)
                        .map_err(|error| {
                            error!("Failed to send execution payload to peer: {}", error);
                            error
                        }),
                    None => Ok(false),
                }
            }
        }
    }
}
