use crate::{
    block_pool_service::{BlockExists, BlockPoolService, GetBlockByHash, ProcessBlock},
    cache::{GossipCache, GossipCacheKey},
    peer_list::PeerListFacade,
    sync::SyncState,
    types::{GossipDataRequest, InternalGossipError, InvalidDataError},
    GossipClient, GossipError, GossipResult,
};
use actix::{Actor, Addr, Context, Handler};
use base58::ToBase58;
use core::net::SocketAddr;
use irys_actors::{
    block_discovery::BlockDiscoveryFacade,
    mempool_service::{ChunkIngressError, MempoolFacade},
};
use irys_api_client::ApiClient;
use irys_types::{
    CommitmentTransaction, GossipData, GossipRequest, IrysBlockHeader, IrysTransactionHeader,
    IrysTransactionResponse, RethPeerInfo, UnpackedChunk, H256,
};
use std::sync::Arc;
use tracing::{debug, error};

/// Handles data received by the `GossipServer`
#[derive(Debug)]
pub(crate) struct GossipServerDataHandler<TMempoolFacade, TBlockDiscovery, TApiClient, R>
where
    TMempoolFacade: MempoolFacade,
    TBlockDiscovery: BlockDiscoveryFacade,
    TApiClient: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    pub mempool: TMempoolFacade,
    pub block_pool: Addr<BlockPoolService<TApiClient, R, TBlockDiscovery>>,
    pub cache: Arc<GossipCache>,
    pub api_client: TApiClient,
    pub gossip_client: GossipClient,
    pub peer_list_service: PeerListFacade<TApiClient, R>,
    pub sync_state: SyncState,
}

impl<M, B, A, R> Clone for GossipServerDataHandler<M, B, A, R>
where
    M: MempoolFacade,
    B: BlockDiscoveryFacade,
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    fn clone(&self) -> Self {
        Self {
            mempool: self.mempool.clone(),
            block_pool: self.block_pool.clone(),
            cache: Arc::clone(&self.cache),
            api_client: self.api_client.clone(),
            gossip_client: self.gossip_client.clone(),
            peer_list_service: self.peer_list_service.clone(),
            sync_state: self.sync_state.clone(),
        }
    }
}

impl<M, B, A, R> GossipServerDataHandler<M, B, A, R>
where
    M: MempoolFacade,
    B: BlockDiscoveryFacade,
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    pub(crate) async fn handle_chunk(
        &self,
        chunk_request: GossipRequest<UnpackedChunk>,
    ) -> GossipResult<()> {
        let source_miner_address = chunk_request.miner_address;
        let chunk = chunk_request.data;
        let chunk_path_hash = chunk.chunk_path_hash();
        match self.mempool.handle_chunk(chunk).await {
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

        if self.mempool.is_known_tx(tx_id).await? {
            debug!(
                "Node {}: Transaction has already been handled, skipping",
                self.gossip_client.mining_address
            );
            return Ok(());
        }

        match self
            .mempool
            .handle_data_transaction(tx)
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

        if self.mempool.is_known_tx(tx_id).await? {
            debug!(
                "Node {}: Commitment Transaction has already been handled, skipping",
                self.gossip_client.mining_address
            );
            return Ok(());
        }

        match self
            .mempool
            .handle_commitment_transaction(tx)
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
        let source_miner_address = block_header_request.miner_address;
        let block_header = block_header_request.data;
        let block_hash = block_header.block_hash;
        debug!(
            "Node {}: Gossip block received from peer {}: {:?}",
            self.gossip_client.mining_address,
            source_miner_address,
            block_hash.0.to_base58()
        );

        if self.sync_state.is_syncing()
            && block_header.height > (self.sync_state.sync_target_height() + 1) as u64
        {
            debug!(
                "Node {}: Block {} is out of the sync range, skipping",
                self.gossip_client.mining_address,
                block_hash.0.to_base58()
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

        let has_block_already_been_processed = self
            .block_pool
            .send(BlockExists {
                block_hash: block_header.block_hash,
            })
            .await
            .map_err(|mailbox_error| GossipError::unknown(&mailbox_error))?
            .map_err(GossipError::BlockPool)?;

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
        for tx_response in missing_txs.into_iter() {
            let tx_id;
            let mempool_response = match tx_response {
                IrysTransactionResponse::Commitment(commitment_tx) => {
                    tx_id = commitment_tx.id;
                    self.mempool
                        .handle_commitment_transaction(commitment_tx)
                        .await
                }
                IrysTransactionResponse::Storage(tx) => {
                    tx_id = tx.id;
                    self.mempool.handle_data_transaction(tx).await
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
            .send(ProcessBlock {
                header: block_header,
            })
            .await
            .map_err(|mailbox_error| GossipError::unknown(&mailbox_error))?
            .map_err(GossipError::BlockPool)?;
        Ok(())
    }

    async fn is_known_tx(&self, tx_id: H256) -> Result<bool, GossipError> {
        Ok(self.mempool.is_known_tx(tx_id).await?)
    }

    pub(crate) async fn handle_get_data(
        &self,
        source_address: SocketAddr,
        request: GossipRequest<GossipDataRequest>,
    ) -> GossipResult<bool> {
        let peer_list_item = self
            .peer_list_service
            .peer_by_mining_address(request.miner_address)
            .await?;
        let Some(peer_info) = peer_list_item else {
            return Ok(false);
        };
        if source_address.ip() != peer_info.address.gossip.ip() {
            return Err(GossipError::InvalidPeer(
                "Requesting peer doesn't match the address of the source peer".to_string(),
            ));
        }

        match request.data {
            GossipDataRequest::Block(block_hash) => {
                let block_result = self
                    .block_pool
                    .send(GetBlockByHash { block_hash })
                    .await
                    .map_err(|mailbox_error| GossipError::unknown(&mailbox_error))?;

                let maybe_block = block_result.map_err(GossipError::BlockPool)?;

                match maybe_block {
                    Some(block) => {
                        match self
                            .gossip_client
                            .send_data_and_update_score(
                                (&request.miner_address, &peer_info),
                                &GossipData::Block(block),
                                &self.peer_list_service,
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
        }
    }
}
