use crate::{
    GossipClient, GossipError, GossipResult,
    block_pool::BlockPool,
    cache::GossipCache,
    metrics::{
        record_gossip_chunk_processing_duration, record_gossip_chunk_received,
        record_gossip_inbound_error,
    },
    rate_limiting::{DataRequestTracker, RequestCheckResult},
    types::{AdvisoryGossipError, InternalGossipError, InvalidDataError},
};
use core::net::SocketAddr;
use irys_actors::block_discovery::{
    build_block_body_for_processed_block_header, get_commitment_tx_in_parallel,
    get_data_tx_in_parallel,
};
use irys_actors::{
    MempoolFacade, block_discovery::BlockDiscoveryFacade,
    chunk_ingress_service::facade::ChunkIngressFacadeImpl,
};
use irys_domain::chain_sync_state::ChainSyncState;
use irys_domain::{
    BlockIndexReadGuard, BlockTreeReadGuard, ExecutionPayloadCache, PeerList, ScoreDecreaseReason,
};
use irys_types::v2::{GossipDataRequestV2, GossipDataV2};
use irys_types::{BlockBody, Config, H256, IrysAddress, IrysPeerId, PeerNetworkError};
use irys_types::{
    BlockHash, CommitmentTransaction, DataTransactionHeader, EvmBlockHash, GossipCacheKey,
    GossipRequestV2, IngressProof, IrysBlockHeader, PeerListItem, SealedBlock, UnpackedChunk,
};
use irys_utils::ElapsedMs as _;
use reth::builder::Block as _;
use reth_ethereum_primitives::Block;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{Instrument as _, debug, error, warn};

const HEADER_AND_BODY_RETRIES: usize = 3;

/// A failed header fetch for a pushed body is not repeated for this long. Short, so a
/// block that peers did not have yet is fetched on a later push.
const FAILED_HEADER_FETCH_TTL: Duration = Duration::from_secs(5);
/// Upper bound on the number of remembered failed header fetches.
const FAILED_HEADER_FETCH_CAPACITY: u64 = 10_000;
/// Header fetches for pushed bodies above this number wait for a permit.
const MAX_CONCURRENT_HEADER_FETCHES: usize = 16;

/// Decide whether the outer body-pull retry should continue after an error.
/// `NoPeersAvailable` means no candidate is reachable right now; the CB
/// cooldown is 30s, so retrying within milliseconds is pure overhead.
fn should_continue_outer_retry(err: &PeerNetworkError) -> bool {
    !matches!(err, PeerNetworkError::NoPeersAvailable)
}

/// Variant of [`should_continue_outer_retry`] for header retries, where the
/// inner returns `GossipError` (wrapping `PeerNetworkError`) rather than
/// `PeerNetworkError` directly.
fn should_continue_header_outer_retry(err: &GossipError) -> bool {
    !matches!(
        err,
        GossipError::PeerNetwork(PeerNetworkError::NoPeersAvailable)
    )
}

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

pub(crate) fn block_body_serve_semaphore(limit: usize) -> Arc<tokio::sync::Semaphore> {
    let permits = if limit == 0 {
        warn!("max_concurrent_block_body_serves is 0, treating as unlimited");
        tokio::sync::Semaphore::MAX_PERMITS
    } else {
        limit
    };
    Arc::new(tokio::sync::Semaphore::new(permits))
}

pub(crate) fn failed_header_fetch_cache() -> moka::sync::Cache<BlockHash, ()> {
    moka::sync::Cache::builder()
        .time_to_live(FAILED_HEADER_FETCH_TTL)
        .max_capacity(FAILED_HEADER_FETCH_CAPACITY)
        .build()
}

pub(crate) fn header_fetch_semaphore() -> Arc<tokio::sync::Semaphore> {
    Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_HEADER_FETCHES))
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
    pub(crate) block_body_semaphore: Arc<tokio::sync::Semaphore>,
    /// Hashes whose header fetch for a pushed body failed recently.
    pub(crate) failed_header_fetches: moka::sync::Cache<BlockHash, ()>,
    pub(crate) header_fetch_semaphore: Arc<tokio::sync::Semaphore>,
    pub block_index: BlockIndexReadGuard,
    pub block_tree: BlockTreeReadGuard,
    pub config: Config,
    pub started_at: Instant,
    /// Precomputed hash of the consensus config to avoid recomputing on every handshake
    pub consensus_config_hash: H256,
    pub runtime_handle: tokio::runtime::Handle,
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
            block_body_semaphore: Arc::clone(&self.block_body_semaphore),
            failed_header_fetches: self.failed_header_fetches.clone(),
            header_fetch_semaphore: Arc::clone(&self.header_fetch_semaphore),
            block_index: self.block_index.clone(),
            block_tree: self.block_tree.clone(),
            config: self.config.clone(),
            started_at: self.started_at,
            consensus_config_hash: self.consensus_config_hash,
            runtime_handle: self.runtime_handle.clone(),
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
                // The conversion lives on `impl From<ChunkIngressError> for
                // GossipError` (types.rs) so the Advisory(Overloaded) ->
                // RateLimited mapping is testable in isolation and shared
                // with any future caller.
                Err(error.into())
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
        let miner_address = peer_info.mining_address;
        self.handle_block_header(
            source_peer_id,
            miner_address,
            irys_block,
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

        let miner_address = peer_info.mining_address;
        self.handle_block_header(
            source_peer_id,
            miner_address,
            irys_block,
            peer_info.address.gossip,
        )
        .in_current_span()
        .await
    }

    /// Seen is the broadcast-exclusion set. Skip only when the block is
    /// already here and making progress: processing, recently processed, or
    /// in the tree.
    ///
    /// A parked cache entry is not skipped. The caller falls through to the
    /// signature check and `dedup_and_maybe_emit_reprocess`, which re-emits
    /// `AttemptReprocessingBlock` once the per-block cooldown has elapsed.
    /// A hash that is seen but not stored still needs a pull.
    async fn skip_seen_block(&self, block_hash: &BlockHash) -> GossipResult<bool> {
        if self.block_pool.is_block_requested(block_hash).await {
            return Ok(false);
        }
        if !self.cache.seen_block_from_any_peer(block_hash)? {
            return Ok(false);
        }
        // Parked means the body is cached and nothing is moving it forward.
        // Skipping here holds the block for the seen TTL.
        if self.block_pool.is_block_parked(block_hash).await {
            return Ok(false);
        }
        Ok(self.block_pool.is_block_known_locally(block_hash).await)
    }

    fn ensure_header_fetch_not_recently_failed(&self, block_hash: &BlockHash) -> GossipResult<()> {
        if self.failed_header_fetches.contains_key(block_hash) {
            return Err(GossipError::Advisory(
                AdvisoryGossipError::HeaderFetchRecentlyFailed(*block_hash),
            ));
        }
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(block.hash = ?block_header.block_hash))]
    pub(crate) async fn handle_block_header(
        &self,
        source_peer_id: IrysPeerId,
        source_miner_address: IrysAddress,
        block_header: Arc<IrysBlockHeader>,
        data_source_ip: SocketAddr,
    ) -> GossipResult<()> {
        let block_hash = block_header.block_hash;
        if block_header.poa.chunk.is_none() {
            error!("received a block without a POA chunk");
        }
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
                self.gossip_client.mining_address,
                block_hash,
                block_header.height,
                &sync_target,
                &self.sync_state.highest_processed_block()
            );
            return Ok(());
        }

        if self.skip_seen_block(&block_hash).await? {
            debug!(
                "Node {}: Block {} height {} seen, not requested, and already known locally, skipping",
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
                    e, block_header
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

        // Dedup against a block the pool has already seen. The helper also
        // re-emits `AttemptReprocessingBlock` for a parked block whose
        // cooldown has elapsed. `skip_seen_block` does not swallow a parked
        // entry, so a re-gossip or a sync pull reaches this helper. The
        // cooldown is what bounds the retry rate.
        let has_block_already_been_processed = self
            .block_pool
            .dedup_and_maybe_emit_reprocess(&block_header.block_hash, block_header.height)
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

        // Duplicate pulls are accepted. The block pool's dedup stops a second process_block.
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

        if self.skip_seen_block(&block_hash).await? {
            debug!(
                "Node {}: Block {} seen, not requested, and already known locally, skipping",
                self.gossip_client.mining_address, block_hash
            );
            return Ok(());
        }

        let maybe_header = self.block_pool.get_block_header(&block_hash).await?;

        let block_header = match maybe_header {
            Some(header) => header,
            None => {
                // A hash that failed recently does not wait for a permit.
                self.ensure_header_fetch_not_recently_failed(&block_hash)?;
                let _permit = self
                    .header_fetch_semaphore
                    .acquire()
                    .await
                    .map_err(|e| GossipError::unknown(&e))?;
                // Rechecked after the permit: pushes queued behind a failed fetch of the
                // same hash do not repeat it, and a header that arrived while waiting
                // is used instead of fetched.
                self.ensure_header_fetch_not_recently_failed(&block_hash)?;
                match self.block_pool.get_block_header(&block_hash).await? {
                    Some(header) => header,
                    None => {
                        // When syncing from a trusted peer, we MUST only fetch from trusted peers —
                        // this is a hard security invariant.
                        let fetched = self
                            .fetch_header_with_retries(
                                block_hash,
                                self.sync_state.is_syncing_from_a_trusted_peer(),
                            )
                            .await;
                        if fetched.is_err() {
                            self.failed_header_fetches.insert(block_hash, ());
                        }
                        fetched?
                    }
                }
            }
        };

        // Check sync range
        let sync_target = self.sync_state.sync_target_height();
        if self.sync_state.is_syncing() && block_header.height > (sync_target + 1) as u64 {
            debug!(
                "Node {}: Block {} height {} is out of the sync range (target: {}, highest processed: {}), skipping",
                self.gossip_client.mining_address,
                block_hash,
                block_header.height,
                &sync_target,
                &self.sync_state.highest_processed_block()
            );
            return Ok(());
        }

        // Same dedup call as `handle_block_header`. See that comment.
        let has_block_already_been_processed = self
            .block_pool
            .dedup_and_maybe_emit_reprocess(&block_header.block_hash, block_header.height)
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
        // The pulled payload is requested: it goes to the requested tier of
        // the cache, and a copy already marked as seen is not skipped.
        self.execution_payload_cache
            .mark_payload_as_requested(evm_block_hash)
            .await;
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
                        continue;
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
                self.gossip_client.mining_address, evm_block_hash
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

        // The cache rejects a body that does not match the header. Such a
        // copy is not recorded as seen, so a correct copy with the same hash
        // is still accepted.
        self.execution_payload_cache
            .add_payload_to_cache(sealed_block)
            .await
            .map_err(|err| {
                GossipError::InvalidData(InvalidDataError::ExecutionPayloadBodyMismatch(
                    err.to_string(),
                ))
            })?;

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
        duplicate_request_window: Duration,
    ) -> GossipResult<bool> {
        // Check rate limiting and score cap
        let check_result = self
            .data_request_tracker
            .check_request(&request.peer_id, duplicate_request_window);

        // If rate limited, don't serve data
        if !check_result.should_serve() {
            debug!(
                "Node {}: Rate limiting peer {:?} for data request",
                self.gossip_client.mining_address, request.miner_address
            );
            return Err(GossipError::RateLimited);
        }

        let resolved = {
            let _permit = self.acquire_block_body_permit(&request.data)?;
            self.resolve_data_request(&request.data, request.miner_address)
                .await?
        };
        match resolved {
            Some(data) => {
                self.send_gossip_data((&request.peer_id, peer_info), Arc::new(data), &check_result);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Synchronous PullData path used by `handle_pull_data` / `handle_pull_data_v1`.
    /// Consults the rate limiter before resolving so PullData honours the same
    /// `RateLimited` contract as the gossip POST path; without this gate,
    /// PullData would be unthrottled even though the HTTP layer advertises
    /// `RejectionReason::RateLimited`.
    #[tracing::instrument(level = "trace", skip_all, err)]
    pub(crate) async fn handle_get_data_sync(
        &self,
        request: GossipRequestV2<GossipDataRequestV2>,
        duplicate_request_window: Duration,
    ) -> GossipResult<Option<GossipDataV2>> {
        let check_result = self
            .data_request_tracker
            .check_request(&request.peer_id, duplicate_request_window);
        if !check_result.should_serve() {
            debug!(
                "Node {}: Rate limiting peer {:?} for pull-data request",
                self.gossip_client.mining_address, request.miner_address
            );
            return Err(GossipError::RateLimited);
        }
        let _permit = self.acquire_block_body_permit(&request.data)?;
        self.resolve_data_request(&request.data, request.miner_address)
            .await
    }

    pub(crate) fn acquire_block_body_permit(
        &self,
        request: &GossipDataRequestV2,
    ) -> GossipResult<Option<tokio::sync::OwnedSemaphorePermit>> {
        if !matches!(request, GossipDataRequestV2::BlockBody(_)) {
            return Ok(None);
        }
        Arc::clone(&self.block_body_semaphore)
            .try_acquire_owned()
            .map(Some)
            .map_err(|_| GossipError::RateLimited)
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
                if let Some(block) = &maybe_block
                    && block.poa.chunk.is_none()
                {
                    error!(
                        block.hash = ?block.block_hash,
                        "Block pool returned a block without a POA chunk"
                    );
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
                            self.gossip_client.mining_address,
                            header.block_hash,
                            source_peer_id,
                            attempt,
                            HEADER_AND_BODY_RETRIES
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
                    let should_retry = should_continue_header_outer_retry(&e);
                    debug!(
                        "Node {}: Failed to pull block header for {} (attempt {}/{}): {}",
                        self.gossip_client.mining_address,
                        block_hash,
                        attempt,
                        HEADER_AND_BODY_RETRIES,
                        e
                    );
                    failed_attempts.push((None, e));
                    if !should_retry {
                        debug!(
                            "Node {}: short-circuiting outer header retry for block {} (no peers available)",
                            self.gossip_client.mining_address, block_hash
                        );
                        break;
                    }
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
        header: &Arc<IrysBlockHeader>,
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
        header: &Arc<IrysBlockHeader>,
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
        header: &Arc<IrysBlockHeader>,
        use_trusted_peers_only: bool,
    ) -> GossipResult<SealedBlock> {
        let block_hash = header.block_hash;
        let mut failed_attempts: Vec<(Option<IrysPeerId>, GossipError)> = Vec::new();

        for attempt in 1..=HEADER_AND_BODY_RETRIES {
            match self
                .gossip_client
                .pull_block_body_from_network(
                    Arc::clone(header),
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
                        self.gossip_client.mining_address,
                        peer_id,
                        block_hash,
                        header.height,
                        attempt,
                        HEADER_AND_BODY_RETRIES,
                        reason
                    );
                    self.peer_list.decrease_peer_score_by_peer_id(
                        &peer_id,
                        ScoreDecreaseReason::BogusData(reason.clone()),
                    );
                    let error = GossipError::Internal(InternalGossipError::Unknown(reason));
                    failed_attempts.push((Some(peer_id), error));
                }
                Err(e) => {
                    let should_retry = should_continue_outer_retry(&e);
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
                    if !should_retry {
                        debug!(
                            "Node {}: short-circuiting outer body-pull retry for block {} (no peers available)",
                            self.gossip_client.mining_address, block_hash
                        );
                        break;
                    }
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

#[cfg(test)]
mod fetch_body_retries_tests {
    use super::*;
    use irys_types::PeerNetworkError;

    #[test]
    fn no_peers_available_should_short_circuit_outer_retry() {
        let err = PeerNetworkError::NoPeersAvailable;
        assert!(
            !should_continue_outer_retry(&err),
            "NoPeersAvailable must short-circuit the outer body-pull retry"
        );
    }

    #[test]
    fn other_errors_should_continue_outer_retry() {
        let err = PeerNetworkError::FailedToRequestData("transient".to_string());
        assert!(
            should_continue_outer_retry(&err),
            "transient errors must allow outer body-pull retries"
        );
    }

    #[test]
    fn no_peers_available_should_short_circuit_outer_header_retry() {
        let err = GossipError::PeerNetwork(PeerNetworkError::NoPeersAvailable);
        assert!(!should_continue_header_outer_retry(&err));
    }

    #[test]
    fn other_errors_should_continue_outer_header_retry() {
        let err = GossipError::Network("transient".to_string());
        assert!(should_continue_header_outer_retry(&err));
    }
}

#[cfg(test)]
mod block_body_serve_cap_tests {
    use super::*;
    use crate::tests::util::data_handler_stub;
    use irys_domain::PeerList;
    use irys_domain::chain_sync_state::ChainSyncState;
    use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{
        BlockHash, Config, DatabaseProvider, DbSyncMode, H256, IrysAddress, IrysPeerId, NodeConfig,
        PeerAddress, PeerListItem, PeerScore,
    };
    use std::sync::Arc;
    use std::time::Duration;

    const WINDOW: Duration = Duration::from_secs(10);

    struct Fixture {
        _dir: irys_testing_utils::tempfile::TempDir,
        handler: Arc<
            GossipDataHandler<
                crate::tests::util::MempoolStub,
                crate::tests::util::BlockDiscoveryStub,
            >,
        >,
    }

    fn fixture(limit: usize) -> Fixture {
        let dir = TempDirBuilder::new().with_tracing().build();
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = dir.path().to_path_buf();
        node_config.p2p_gossip.max_concurrent_block_body_serves = limit;
        let config = Config::new_with_random_peer_id(node_config);
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(
                &config.node_config.base_directory,
                DbSyncMode::UtterlyNoSync,
            )
            .expect("open test db"),
        ));
        let peer_list = PeerList::test_mock().expect("peer list");
        let sync_state = ChainSyncState::new(false, false);
        let handler = data_handler_stub(&config, &peer_list, db, sync_state);
        Fixture { _dir: dir, handler }
    }

    fn peer() -> PeerListItem {
        let mining_address = IrysAddress::repeat_byte(9);
        PeerListItem {
            peer_id: IrysPeerId::from(mining_address),
            mining_address,
            reputation_score: PeerScore::new(100),
            response_time: 0,
            address: PeerAddress {
                gossip: "127.0.0.1:1".parse().expect("gossip addr"),
                api: "127.0.0.1:1".parse().expect("api addr"),
                execution: Default::default(),
            },
            last_seen: 0,
            is_online: true,
            protocol_version: Default::default(),
            ..Default::default()
        }
    }

    fn body_request() -> GossipRequestV2<GossipDataRequestV2> {
        let miner = IrysAddress::repeat_byte(9);
        GossipRequestV2 {
            peer_id: IrysPeerId::from(miner),
            miner_address: miner,
            data: GossipDataRequestV2::BlockBody(BlockHash::repeat_byte(0xAB)),
        }
    }

    fn tx_request() -> GossipRequestV2<GossipDataRequestV2> {
        let miner = IrysAddress::repeat_byte(9);
        GossipRequestV2 {
            peer_id: IrysPeerId::from(miner),
            miner_address: miner,
            data: GossipDataRequestV2::Transaction(H256::repeat_byte(0x22)),
        }
    }

    #[test]
    fn zero_block_body_serve_limit_is_unlimited() {
        let semaphore = block_body_serve_semaphore(0);
        assert_eq!(
            semaphore.available_permits(),
            tokio::sync::Semaphore::MAX_PERMITS
        );
    }

    #[tokio::test]
    async fn exhausted_cap_rejects_block_body_before_resolve() {
        let fixture = fixture(1);
        let _held = fixture
            .handler
            .block_body_semaphore
            .clone()
            .try_acquire_owned()
            .expect("test holds the only permit");
        let peer = peer();

        let sync = fixture
            .handler
            .handle_get_data_sync(body_request(), WINDOW)
            .await;
        assert!(
            matches!(sync, Err(GossipError::RateLimited)),
            "sync path must refuse before resolve, got {sync:?}"
        );

        let get = fixture
            .handler
            .handle_get_data(&peer, body_request(), WINDOW)
            .await;
        assert!(
            matches!(get, Err(GossipError::RateLimited)),
            "get path must refuse before resolve, got {get:?}"
        );
    }

    #[tokio::test]
    async fn exhausted_cap_still_serves_a_non_body_request() {
        let fixture = fixture(1);
        let _held = fixture
            .handler
            .block_body_semaphore
            .clone()
            .try_acquire_owned()
            .expect("test holds the only permit");
        let peer = peer();

        let sync = fixture
            .handler
            .handle_get_data_sync(tx_request(), WINDOW)
            .await
            .expect("transaction lookup is outside the body cap");
        assert!(sync.is_none());

        let served = fixture
            .handler
            .handle_get_data(&peer, tx_request(), WINDOW)
            .await
            .expect("transaction lookup is outside the body cap");
        assert!(!served);
        assert_eq!(fixture.handler.block_body_semaphore.available_permits(), 0);
    }

    #[tokio::test]
    async fn releasing_the_permit_serves_the_next_body() {
        let fixture = fixture(1);
        let held = fixture
            .handler
            .block_body_semaphore
            .clone()
            .try_acquire_owned()
            .expect("test holds the only permit");
        drop(held);

        let sync = fixture
            .handler
            .handle_get_data_sync(body_request(), WINDOW)
            .await
            .expect("body serve under the cap");
        assert!(sync.is_none(), "unknown hash has no body");
        assert_eq!(fixture.handler.block_body_semaphore.available_permits(), 1);
    }

    #[tokio::test]
    async fn permit_is_released_when_the_caller_returns_err() {
        let fixture = fixture(1);
        let request = body_request();
        let result: GossipResult<()> = async {
            let _permit = fixture.handler.acquire_block_body_permit(&request.data)?;
            Err(GossipError::Internal(InternalGossipError::Unknown(
                "boom".to_string(),
            )))
        }
        .await;
        assert!(matches!(result, Err(GossipError::Internal(_))));
        assert_eq!(fixture.handler.block_body_semaphore.available_permits(), 1);
    }

    /// The write lock is held on its own thread. `get_block_body` then blocks
    /// in `block_tree.read()` on a runtime worker, so this test needs a second
    /// worker and must not hold that guard on the test task.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn admitted_body_serve_holds_the_permit_until_resolve_finishes() {
        let fixture = fixture(1);
        let tree = fixture.handler.block_tree.clone();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let locker = std::thread::spawn(move || {
            let _guard = tree.write();
            locked_tx.send(()).expect("test is waiting");
            release_rx.recv().ok();
        });
        locked_rx.recv().expect("lock taken");

        let handler = Arc::clone(&fixture.handler);
        let in_flight =
            tokio::spawn(async move { handler.handle_get_data_sync(body_request(), WINDOW).await });

        tokio::time::timeout(Duration::from_secs(5), async {
            while fixture.handler.block_body_semaphore.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("admitted request keeps the permit");

        let peer = peer();
        let rejected = fixture
            .handler
            .handle_get_data(&peer, body_request(), WINDOW)
            .await;
        assert!(matches!(rejected, Err(GossipError::RateLimited)));

        let cloned = fixture.handler.as_ref().clone();
        let cloned_rejected = cloned.handle_get_data_sync(body_request(), WINDOW).await;
        assert!(
            matches!(cloned_rejected, Err(GossipError::RateLimited)),
            "Clone must share the semaphore, got {cloned_rejected:?}"
        );

        release_tx.send(()).expect("locker alive");
        let body = tokio::time::timeout(Duration::from_secs(5), in_flight)
            .await
            .expect("request finishes once the tree lock is released")
            .expect("task did not panic")
            .expect("unknown hash is not a handler error");
        assert!(body.is_none());
        assert_eq!(fixture.handler.block_body_semaphore.available_permits(), 1);
        locker.join().expect("locker thread");
    }

    /// `block_tree.read()` is a blocking lock, not a Tokio yield. After it
    /// returns, `get_block_header` is immediately ready, so the same poll
    /// finishes the handler and `abort` never becomes a `JoinError`. Hold the
    /// real permit across `pending` instead, which is a cancellation point.
    #[tokio::test]
    async fn aborting_an_admitted_body_serve_returns_the_permit() {
        let fixture = fixture(1);
        let handler = Arc::clone(&fixture.handler);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let in_flight = tokio::spawn(async move {
            let _permit = handler
                .acquire_block_body_permit(&body_request().data)
                .expect("acquire")
                .expect("body request takes a permit");
            started_tx.send(()).expect("test is waiting");
            std::future::pending::<()>().await;
        });
        started_rx.await.expect("permit held");
        assert_eq!(fixture.handler.block_body_semaphore.available_permits(), 0);

        in_flight.abort();
        let join = in_flight.await.expect_err("aborted task");
        assert!(join.is_cancelled());
        assert_eq!(fixture.handler.block_body_semaphore.available_permits(), 1);
    }
}

#[cfg(test)]
mod body_fetch_recovery_tests {
    use super::*;
    use crate::SyncChainServiceMessage;
    use crate::block_pool::BlockRemovalReason;
    use crate::tests::util::{FakeGossipServer, data_handler_stub_with_sync, peer_list_item};
    use crate::types::GossipResponse;
    use irys_domain::PeerList;
    use irys_domain::chain_sync_state::ChainSyncState;
    use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
    use irys_testing_utils::IrysBlockHeaderTestExt as _;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::SealedBlock;
    use irys_types::v2::GossipDataV2;
    use irys_types::{
        BlockBody, Config, DatabaseProvider, DbSyncMode, H256, IrysAddress, IrysPeerId, NodeConfig,
        PeerScore,
    };
    use irys_utils::circuit_breaker::CircuitBreakerConfig;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::mpsc::UnboundedReceiver;

    type TestHandler =
        GossipDataHandler<crate::tests::util::MempoolStub, crate::tests::util::BlockDiscoveryStub>;

    fn test_handler() -> (
        irys_testing_utils::tempfile::TempDir,
        PeerList,
        Arc<TestHandler>,
    ) {
        let (dir, peer_list, handler, _sync_rx) = test_handler_with_sync();
        (dir, peer_list, handler)
    }

    fn test_handler_with_sync() -> (
        irys_testing_utils::tempfile::TempDir,
        PeerList,
        Arc<TestHandler>,
        UnboundedReceiver<SyncChainServiceMessage>,
    ) {
        let dir = TempDirBuilder::new().with_tracing().build();
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = dir.path().to_path_buf();
        let config = Config::new_with_random_peer_id(node_config);
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(
                &config.node_config.base_directory,
                DbSyncMode::UtterlyNoSync,
            )
            .expect("open test db"),
        ));
        let peer_list = PeerList::test_mock().expect("peer list");
        let (handler, sync_rx) =
            data_handler_stub_with_sync(&config, &peer_list, db, ChainSyncState::new(false, false));
        (dir, peer_list, handler, sync_rx)
    }

    fn signed_orphan(config: &Config) -> Arc<IrysBlockHeader> {
        let mut chain = crate::BlockStatusProvider::produce_mock_chain(1, None, &config.consensus);
        Arc::new(chain.pop().expect("one header"))
    }

    fn seal(header: &Arc<IrysBlockHeader>) -> Arc<SealedBlock> {
        Arc::new(
            SealedBlock::new(
                Arc::clone(header),
                BlockBody {
                    block_hash: header.block_hash,
                    ..Default::default()
                },
            )
            .expect("signed header seals"),
        )
    }

    fn record_block_seen(handler: &TestHandler, block_hash: BlockHash) {
        handler
            .cache
            .record_seen(
                IrysPeerId::from(IrysAddress::repeat_byte(0x11)),
                GossipCacheKey::Block(block_hash),
            )
            .expect("record seen");
    }

    fn assert_reprocess_for(message: SyncChainServiceMessage, block_hash: BlockHash) {
        assert!(
            matches!(
                message,
                SyncChainServiceMessage::AttemptReprocessingBlock(hash) if hash == block_hash
            ),
            "expected AttemptReprocessingBlock for {block_hash:?}, got {message:?}"
        );
    }

    fn dummy_addr() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 9))
    }

    fn orphan_header() -> Arc<IrysBlockHeader> {
        let mut header = irys_testing_utils::new_mock_signed_header();
        header.height = 1;
        header.previous_block_hash = H256::random();
        header.test_sign();
        Arc::new(header)
    }

    fn body_for(header: &IrysBlockHeader) -> GossipResponse<Option<GossipDataV2>> {
        GossipResponse::Accepted(Some(GossipDataV2::BlockBody(Arc::new(BlockBody {
            block_hash: header.block_hash,
            data_transactions: vec![],
            commitment_transactions: vec![],
        }))))
    }

    /// A failed body pull records the hash in the seen cache. That cache is the
    /// broadcast-exclusion set, so the hash stays, but a later arrival must
    /// still be able to fetch the body. Once the block is in the pool, a
    /// further arrival is skipped and does not pull.
    #[tokio::test]
    async fn failed_body_pull_does_not_block_a_later_arrival() {
        let (_dir, peer_list, handler) = test_handler();
        let header = orphan_header();
        let block_hash = header.block_hash;

        let server_a = FakeGossipServer::new();
        server_a.set_on_pull_data_request(|_| GossipResponse::Accepted(None));
        let addr_a = server_a.spawn();
        let peer_a = peer_list_item(0x41, addr_a, true, PeerScore::INITIAL);
        let peer_a_id = peer_a.peer_id;
        peer_list.add_or_update_peer(peer_a, true);

        let first = handler
            .handle_block_header(
                peer_a_id,
                IrysAddress::repeat_byte(0x41),
                Arc::clone(&header),
                addr_a,
            )
            .await;
        assert!(first.is_err(), "the first body pull fails: {first:?}");
        assert!(
            !handler.block_pool.contains_block(&block_hash).await,
            "a failed pull must not leave the block in the pool"
        );

        let b_pulls = Arc::new(AtomicUsize::new(0));
        let pulls = Arc::clone(&b_pulls);
        let served = Arc::clone(&header);
        let server_b = FakeGossipServer::new();
        server_b.set_on_pull_data_request(move |_| {
            pulls.fetch_add(1, Ordering::SeqCst);
            body_for(&served)
        });
        let addr_b = server_b.spawn();
        let peer_b = peer_list_item(0x42, addr_b, true, PeerScore::INITIAL);
        let peer_b_id = peer_b.peer_id;
        peer_list.add_or_update_peer(peer_b, true);

        let second = handler
            .handle_block_header(
                peer_b_id,
                IrysAddress::repeat_byte(0x42),
                Arc::clone(&header),
                addr_b,
            )
            .await;
        assert!(
            second.is_ok(),
            "the second arrival must be processed: {second:?}"
        );
        assert_eq!(
            b_pulls.load(Ordering::SeqCst),
            1,
            "a later arrival must pull the body again"
        );
        assert!(handler.block_pool.contains_block(&block_hash).await);

        let c_pulls = Arc::new(AtomicUsize::new(0));
        let pulls = Arc::clone(&c_pulls);
        let server_c = FakeGossipServer::new();
        server_c.set_on_pull_data_request(move |_| {
            pulls.fetch_add(1, Ordering::SeqCst);
            GossipResponse::Accepted(None)
        });
        let addr_c = server_c.spawn();
        let peer_c = peer_list_item(0x43, addr_c, true, PeerScore::INITIAL);
        let peer_c_id = peer_c.peer_id;
        peer_list.add_or_update_peer(peer_c, true);

        let third = handler
            .handle_block_header(peer_c_id, IrysAddress::repeat_byte(0x43), header, addr_c)
            .await;
        assert!(third.is_ok(), "{third:?}");
        assert_eq!(
            c_pulls.load(Ordering::SeqCst),
            0,
            "a block already in the pool must not be pulled again"
        );
    }

    /// Wall-clock bound for a body pull that runs on the server runtime.
    ///
    /// Under a full-suite load the pull can sit in the queue longer than a
    /// few seconds. Coverage multiplies the same way as the chain-test waits.
    /// A gate that blocks a pull must use a larger bound than the wait for
    /// that pull to start.
    fn loaded_wait(base_secs: u64) -> Duration {
        let scale = if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
            3
        } else {
            1
        };
        Duration::from_secs(base_secs.saturating_mul(scale))
    }

    /// An authenticated header proves the sender can reach us. The body pull
    /// is the proof that we can reach them, so it runs while the breaker is
    /// open. A served body closes the breaker and marks the peer online.
    #[tokio::test]
    async fn open_breaker_source_peer_body_pull_closes_the_breaker() {
        let (_dir, peer_list, mut handler) = test_handler();
        {
            let unique = Arc::get_mut(&mut handler).expect("unique handler");
            let mining_address = unique.gossip_client.mining_address;
            let local_peer_id = unique.gossip_client.peer_id;
            unique.gossip_client = GossipClient::with_circuit_breaker_config(
                Duration::from_secs(5),
                mining_address,
                local_peer_id,
                CircuitBreakerConfig::p2p_defaults(),
                tokio::runtime::Handle::current(),
                irys_types::DEFAULT_MAX_PEER_RESPONSE_BYTES,
            );
        }

        let header = orphan_header();
        let served = Arc::clone(&header);
        let pulls = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&pulls);
        let server = FakeGossipServer::new();
        server.set_on_pull_data_request(move |_| {
            hits.fetch_add(1, Ordering::SeqCst);
            body_for(&served)
        });
        let addr = server.spawn();
        let peer = peer_list_item(0x51, addr, false, PeerScore::INITIAL);
        let peer_id = peer.peer_id;
        peer_list.add_or_update_peer(peer, true);

        for _ in 0..5 {
            handler
                .gossip_client
                .circuit_breaker()
                .record_failure(&peer_id);
        }
        assert!(
            !handler
                .gossip_client
                .circuit_breaker()
                .would_allow(&peer_id),
            "the open cooldown has not elapsed, so a normal pull must be refused"
        );

        let result = handler
            .handle_block_header(peer_id, IrysAddress::repeat_byte(0x51), header, addr)
            .await;

        assert!(
            result.is_ok(),
            "the source body pull must run while the breaker is open: {result:?}"
        );
        assert!(
            pulls.load(Ordering::SeqCst) >= 1,
            "the source peer must be dialed"
        );
        let metrics = handler.gossip_client.circuit_breaker().metrics();
        assert_eq!(metrics.open_count, 0, "a served body closes the breaker");
        assert_eq!(metrics.half_open_count, 0);
        assert!(
            peer_list.get_peer(&peer_id).expect("peer exists").is_online,
            "a served body marks the peer online"
        );
    }

    fn server_holding_first_body() -> (
        FakeGossipServer,
        std::sync::mpsc::SyncSender<()>,
        Arc<AtomicUsize>,
    ) {
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = Arc::new(std::sync::Mutex::new(Some(release_rx)));
        let pulls = Arc::new(AtomicUsize::new(0));
        let pulls_for_cb = Arc::clone(&pulls);
        let gate = Arc::clone(&release_rx);
        let server = FakeGossipServer::new();
        server.set_on_pull_data_request(move |request| {
            if !matches!(request, GossipDataRequestV2::BlockBody(_)) {
                return GossipResponse::Accepted(None);
            }
            let n = pulls_for_cb.fetch_add(1, Ordering::SeqCst);
            if n == 0
                && let Some(rx) = gate.lock().expect("gate").take()
            {
                let _ = rx.recv_timeout(loaded_wait(90));
            }
            GossipResponse::Accepted(None)
        });
        (server, release_tx, pulls)
    }

    fn add_peer(peer_list: &PeerList, byte: u8, addr: SocketAddr, is_online: bool) -> IrysPeerId {
        let peer = peer_list_item(byte, addr, is_online, PeerScore::INITIAL);
        let peer_id = peer.peer_id;
        peer_list.add_or_update_peer(peer, true);
        peer_id
    }

    async fn wait_for_a_pull(
        pulls: &AtomicUsize,
        task: &tokio::task::JoinHandle<GossipResult<()>>,
    ) {
        let started = tokio::time::Instant::now();
        loop {
            if pulls.load(Ordering::SeqCst) > 0 {
                return;
            }
            assert!(
                !task.is_finished(),
                "the pull finished before the peer was dialed"
            );
            assert!(
                started.elapsed() < loaded_wait(30),
                "the body pull did not start"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A body that arrives in a POST is processed even while a pull for that
    /// hash is running. The pool rejects a duplicate process.
    #[tokio::test]
    async fn body_posted_during_an_in_flight_pull_is_processed() {
        let (_dir, peer_list, handler) = test_handler();
        let header = orphan_header();
        let block_hash = header.block_hash;

        let (server_a, release_tx, a_pulls) = server_holding_first_body();
        let addr_a = server_a.spawn();
        let peer_a_id = add_peer(&peer_list, 0x91, addr_a, true);

        let served_header = Arc::clone(&header);
        let header_server = FakeGossipServer::new();
        header_server.set_on_pull_data_request(move |request| match request {
            GossipDataRequestV2::BlockHeader(hash) if hash == served_header.block_hash => {
                GossipResponse::Accepted(Some(GossipDataV2::BlockHeader(Arc::clone(
                    &served_header,
                ))))
            }
            _ => GossipResponse::Accepted(None),
        });
        let addr_h = header_server.spawn();
        let _peer_h_id = add_peer(&peer_list, 0x92, addr_h, true);

        let first_handler = Arc::clone(&handler);
        let first_header = Arc::clone(&header);
        let first = tokio::spawn(async move {
            first_handler
                .handle_block_header(
                    peer_a_id,
                    IrysAddress::repeat_byte(0x91),
                    first_header,
                    addr_a,
                )
                .await
        });
        wait_for_a_pull(&a_pulls, &first).await;

        let posted = tokio::time::timeout(
            loaded_wait(30),
            handler.handle_block_body(
                GossipRequestV2 {
                    peer_id: IrysPeerId::from(IrysAddress::repeat_byte(0x92)),
                    miner_address: IrysAddress::repeat_byte(0x92),
                    data: BlockBody {
                        block_hash,
                        data_transactions: vec![],
                        commitment_transactions: vec![],
                    },
                },
                addr_h,
            ),
        )
        .await
        .expect("the posted body is handled while the pull is in flight");
        assert!(posted.is_ok(), "a posted body is not an error: {posted:?}");
        assert!(
            handler.block_pool.contains_block(&block_hash).await,
            "a body posted while a pull is in flight must be processed"
        );

        release_tx.send(()).expect("A is waiting");
        let first = tokio::time::timeout(loaded_wait(30), first)
            .await
            .expect("A finishes after the gate opens")
            .expect("A task");
        let _ = first;
    }

    /// Serves `header` to header pulls once `available` is set. Counts the
    /// header pulls.
    fn header_server(
        header: &Arc<IrysBlockHeader>,
        available: Arc<AtomicBool>,
    ) -> (FakeGossipServer, Arc<AtomicUsize>) {
        let pulls = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&pulls);
        let served = Arc::clone(header);
        let server = FakeGossipServer::new();
        server.set_on_pull_data_request(move |request| match request {
            GossipDataRequestV2::BlockHeader(hash) if hash == served.block_hash => {
                hits.fetch_add(1, Ordering::SeqCst);
                if available.load(Ordering::SeqCst) {
                    GossipResponse::Accepted(Some(GossipDataV2::BlockHeader(Arc::clone(&served))))
                } else {
                    GossipResponse::Accepted(None)
                }
            }
            _ => GossipResponse::Accepted(None),
        });
        (server, pulls)
    }

    fn pushed_body(block_hash: BlockHash, byte: u8) -> GossipRequestV2<BlockBody> {
        GossipRequestV2 {
            peer_id: IrysPeerId::from(IrysAddress::repeat_byte(byte)),
            miner_address: IrysAddress::repeat_byte(byte),
            data: BlockBody {
                block_hash,
                data_transactions: vec![],
                commitment_transactions: vec![],
            },
        }
    }

    fn with_failed_header_fetch_ttl(handler: &mut Arc<TestHandler>, ttl: Duration) {
        Arc::get_mut(handler)
            .expect("unique handler")
            .failed_header_fetches = moka::sync::Cache::builder().time_to_live(ttl).build();
    }

    /// A pushed body for an unknown hash fetches the header once. A second
    /// push within the TTL does not fetch again. A push after the TTL does.
    #[tokio::test]
    async fn failed_header_fetch_is_not_repeated_within_the_ttl() {
        let ttl = Duration::from_secs(1);
        let (_dir, peer_list, mut handler) = test_handler();
        with_failed_header_fetch_ttl(&mut handler, ttl);
        let header = orphan_header();
        let block_hash = header.block_hash;
        let (server, pulls) = header_server(&header, Arc::new(AtomicBool::new(false)));
        let addr = server.spawn();
        add_peer(&peer_list, 0xC1, addr, true);

        let first = handler
            .handle_block_body(pushed_body(block_hash, 0xC1), addr)
            .await;
        assert!(first.is_err(), "no peer has the header");
        let pulls_after_first = pulls.load(Ordering::SeqCst);
        assert!(pulls_after_first > 0, "the first push fetches the header");

        let second = handler
            .handle_block_body(pushed_body(block_hash, 0xC1), addr)
            .await;
        assert!(second.is_err());
        assert_eq!(
            pulls.load(Ordering::SeqCst),
            pulls_after_first,
            "a push within the TTL does not fetch the header"
        );

        tokio::time::sleep(ttl + Duration::from_millis(200)).await;
        let third = handler
            .handle_block_body(pushed_body(block_hash, 0xC1), addr)
            .await;
        assert!(third.is_err());
        assert!(
            pulls.load(Ordering::SeqCst) > pulls_after_first,
            "a push after the TTL fetches the header again"
        );
    }

    /// A header that peers serve only after a failed fetch is fetched and the
    /// block is processed once the TTL expires.
    #[tokio::test]
    async fn block_is_processed_when_its_header_becomes_available_after_the_ttl() {
        let ttl = Duration::from_secs(1);
        let (_dir, peer_list, mut handler) = test_handler();
        with_failed_header_fetch_ttl(&mut handler, ttl);
        let header = orphan_header();
        let block_hash = header.block_hash;
        let available = Arc::new(AtomicBool::new(false));
        let (server, pulls) = header_server(&header, Arc::clone(&available));
        let addr = server.spawn();
        add_peer(&peer_list, 0xC2, addr, true);

        let first = handler
            .handle_block_body(pushed_body(block_hash, 0xC2), addr)
            .await;
        assert!(first.is_err(), "no peer has the header");

        available.store(true, Ordering::SeqCst);
        let pulls_before = pulls.load(Ordering::SeqCst);
        let within_ttl = handler
            .handle_block_body(pushed_body(block_hash, 0xC2), addr)
            .await;
        assert!(within_ttl.is_err());
        assert_eq!(pulls.load(Ordering::SeqCst), pulls_before);
        assert!(!handler.block_pool.contains_block(&block_hash).await);

        tokio::time::sleep(ttl + Duration::from_millis(200)).await;
        let after_ttl = handler
            .handle_block_body(pushed_body(block_hash, 0xC2), addr)
            .await;
        assert!(after_ttl.is_ok(), "the header is served now: {after_ttl:?}");
        assert!(
            handler.block_pool.contains_block(&block_hash).await,
            "the block is processed once the header is fetched"
        );
    }

    /// Pushes that wait for a header-fetch permit check the failed-fetch
    /// cache after they get it, so only the first one fetches.
    #[tokio::test]
    async fn queued_pushes_do_not_repeat_a_failed_header_fetch() {
        let (_dir, peer_list, mut handler) = test_handler();
        Arc::get_mut(&mut handler)
            .expect("unique handler")
            .header_fetch_semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let header = orphan_header();
        let block_hash = header.block_hash;
        let (server, pulls) = header_server(&header, Arc::new(AtomicBool::new(false)));
        let addr = server.spawn();
        add_peer(&peer_list, 0xC3, addr, true);

        let pushes = (0..4).map(|_| {
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                handler
                    .handle_block_body(pushed_body(block_hash, 0xC3), addr)
                    .await
            })
        });
        for result in futures::future::join_all(pushes).await {
            assert!(
                result.expect("push task").is_err(),
                "no peer has the header"
            );
        }

        let single_fetch = {
            let (_dir, peer_list, handler) = test_handler();
            let (server, pulls) = header_server(&header, Arc::new(AtomicBool::new(false)));
            let addr = server.spawn();
            add_peer(&peer_list, 0xC4, addr, true);
            let _ = handler
                .handle_block_body(pushed_body(block_hash, 0xC4), addr)
                .await;
            pulls.load(Ordering::SeqCst)
        };
        assert_eq!(
            pulls.load(Ordering::SeqCst),
            single_fetch,
            "the queued pushes reuse the first failed fetch"
        );
    }

    /// A hash that failed recently is rejected before the permit wait, with an
    /// advisory error.
    #[tokio::test]
    async fn recently_failed_header_fetch_does_not_wait_for_a_permit() {
        let (_dir, _peer_list, mut handler) = test_handler();
        Arc::get_mut(&mut handler)
            .expect("unique handler")
            .header_fetch_semaphore = Arc::new(tokio::sync::Semaphore::new(0));
        let block_hash = orphan_header().block_hash;
        handler.failed_header_fetches.insert(block_hash, ());

        let result = tokio::time::timeout(
            loaded_wait(5),
            handler.handle_block_body(pushed_body(block_hash, 0xC5), dummy_addr()),
        )
        .await
        .expect("no permit wait for a recently failed hash");
        assert!(
            matches!(
                result,
                Err(GossipError::Advisory(
                    AdvisoryGossipError::HeaderFetchRecentlyFailed(hash)
                )) if hash == block_hash
            ),
            "{result:?}"
        );
    }

    /// A header that arrives while a push waits for the permit is used, not
    /// fetched.
    #[tokio::test]
    async fn header_that_arrives_during_the_permit_wait_is_not_fetched() {
        let (_dir, peer_list, mut handler) = test_handler();
        Arc::get_mut(&mut handler)
            .expect("unique handler")
            .header_fetch_semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let header = orphan_header();
        let block_hash = header.block_hash;
        let pulls = Arc::new(AtomicUsize::new(0));
        let header_pulls = Arc::clone(&pulls);
        let served = Arc::clone(&header);
        let server = FakeGossipServer::new();
        server.set_on_pull_data_request(move |request| match request {
            GossipDataRequestV2::BlockHeader(hash) if hash == served.block_hash => {
                header_pulls.fetch_add(1, Ordering::SeqCst);
                GossipResponse::Accepted(Some(GossipDataV2::BlockHeader(Arc::clone(&served))))
            }
            GossipDataRequestV2::BlockBody(hash) if hash == served.block_hash => body_for(&served),
            _ => GossipResponse::Accepted(None),
        });
        let addr = server.spawn();
        add_peer(&peer_list, 0xC6, addr, true);

        let held = Arc::clone(&handler.header_fetch_semaphore)
            .acquire_owned()
            .await
            .expect("permit");
        let waiting = {
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                handler
                    .handle_block_body(pushed_body(block_hash, 0xC6), addr)
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;

        handler
            .handle_block_header(
                IrysPeerId::from(IrysAddress::repeat_byte(0xC6)),
                IrysAddress::repeat_byte(0xC6),
                Arc::clone(&header),
                addr,
            )
            .await
            .expect("the header is processed");
        assert!(
            handler
                .block_pool
                .get_block_header(&block_hash)
                .await
                .expect("lookup")
                .is_some(),
            "the header is in the pool"
        );
        let pulls_before_release = pulls.load(Ordering::SeqCst);

        drop(held);
        let _ = tokio::time::timeout(loaded_wait(30), waiting)
            .await
            .expect("the waiting push finishes")
            .expect("push task");
        assert_eq!(
            pulls.load(Ordering::SeqCst),
            pulls_before_release,
            "the header that arrived meanwhile is not fetched"
        );
    }

    /// Two peers push the same header at the same time. Each arrival pulls
    /// the body. The pool keeps one block, so a later arrival does not pull.
    #[tokio::test]
    async fn concurrent_header_arrivals_both_pull_and_the_block_is_processed_once() {
        let (_dir, peer_list, handler) = test_handler();
        let header = orphan_header();
        let block_hash = header.block_hash;

        let (release_a_tx, release_a_rx) = std::sync::mpsc::sync_channel(1);
        let (release_b_tx, release_b_rx) = std::sync::mpsc::sync_channel(1);
        let release_a_rx = Arc::new(std::sync::Mutex::new(Some(release_a_rx)));
        let release_b_rx = Arc::new(std::sync::Mutex::new(Some(release_b_rx)));

        let a_pulls = Arc::new(AtomicUsize::new(0));
        let pulls = Arc::clone(&a_pulls);
        let gate = Arc::clone(&release_a_rx);
        let served = Arc::clone(&header);
        let server_a = FakeGossipServer::new();
        server_a.set_on_pull_data_request(move |request| {
            if !matches!(request, GossipDataRequestV2::BlockBody(_)) {
                return GossipResponse::Accepted(None);
            }
            let n = pulls.fetch_add(1, Ordering::SeqCst);
            if n == 0
                && let Some(rx) = gate.lock().expect("gate").take()
            {
                let _ = rx.recv_timeout(loaded_wait(90));
            }
            body_for(&served)
        });
        let addr_a = server_a.spawn();
        let peer_a_id = add_peer(&peer_list, 0xB1, addr_a, true);

        let b_pulls = Arc::new(AtomicUsize::new(0));
        let pulls = Arc::clone(&b_pulls);
        let gate = Arc::clone(&release_b_rx);
        let served = Arc::clone(&header);
        let server_b = FakeGossipServer::new();
        server_b.set_on_pull_data_request(move |request| {
            if !matches!(request, GossipDataRequestV2::BlockBody(_)) {
                return GossipResponse::Accepted(None);
            }
            let n = pulls.fetch_add(1, Ordering::SeqCst);
            if n == 0
                && let Some(rx) = gate.lock().expect("gate").take()
            {
                let _ = rx.recv_timeout(loaded_wait(90));
            }
            body_for(&served)
        });
        let addr_b = server_b.spawn();
        let peer_b_id = add_peer(&peer_list, 0xB2, addr_b, true);

        let handler_a = Arc::clone(&handler);
        let header_a = Arc::clone(&header);
        let arrival_a = tokio::spawn(async move {
            handler_a
                .handle_block_header(peer_a_id, IrysAddress::repeat_byte(0xB1), header_a, addr_a)
                .await
        });
        let handler_b = Arc::clone(&handler);
        let header_b = Arc::clone(&header);
        let arrival_b = tokio::spawn(async move {
            handler_b
                .handle_block_header(peer_b_id, IrysAddress::repeat_byte(0xB2), header_b, addr_b)
                .await
        });

        let started = tokio::time::Instant::now();
        loop {
            if a_pulls.load(Ordering::SeqCst) > 0 && b_pulls.load(Ordering::SeqCst) > 0 {
                break;
            }
            let a_dropped = arrival_a.is_finished() && a_pulls.load(Ordering::SeqCst) == 0;
            let b_dropped = arrival_b.is_finished() && b_pulls.load(Ordering::SeqCst) == 0;
            assert!(
                !a_dropped && !b_dropped,
                "an arrival returned without pulling: a={} b={}",
                a_pulls.load(Ordering::SeqCst),
                b_pulls.load(Ordering::SeqCst)
            );
            assert!(
                started.elapsed() < loaded_wait(30),
                "both arrivals must pull the body"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        release_a_tx.send(()).expect("A is waiting");
        release_b_tx.send(()).expect("B is waiting");
        let result_a = tokio::time::timeout(loaded_wait(30), arrival_a)
            .await
            .expect("A finishes after the gate opens")
            .expect("A task");
        let result_b = tokio::time::timeout(loaded_wait(30), arrival_b)
            .await
            .expect("B finishes after the gate opens")
            .expect("B task");
        assert!(
            result_a.is_ok() || result_b.is_ok(),
            "one arrival processes the block: {result_a:?} {result_b:?}"
        );
        assert!(
            handler.block_pool.contains_block(&block_hash).await,
            "the block is in the pool"
        );

        let c_pulls = Arc::new(AtomicUsize::new(0));
        let pulls = Arc::clone(&c_pulls);
        let server_c = FakeGossipServer::new();
        server_c.set_on_pull_data_request(move |_| {
            pulls.fetch_add(1, Ordering::SeqCst);
            GossipResponse::Accepted(None)
        });
        let addr_c = server_c.spawn();
        let peer_c_id = add_peer(&peer_list, 0xB3, addr_c, true);
        let third = tokio::time::timeout(
            loaded_wait(30),
            handler.handle_block_header(peer_c_id, IrysAddress::repeat_byte(0xB3), header, addr_c),
        )
        .await
        .expect("the later arrival finishes");
        assert!(third.is_ok(), "{third:?}");
        assert_eq!(
            c_pulls.load(Ordering::SeqCst),
            0,
            "the pool already has the block, so a later arrival must not pull"
        );
    }

    /// A parked block is in the cache and not processing. After the reprocess
    /// cooldown, a re-gossip must emit `AttemptReprocessingBlock`. The seen
    /// TTL must not swallow that retry. The helper still suppresses a
    /// re-gossip that arrives inside the cooldown.
    #[tokio::test(start_paused = true)]
    async fn parked_block_regossip_after_cooldown_emits_reprocess() {
        let (_dir, _peer_list, handler, mut sync_rx) = test_handler_with_sync();
        let header = signed_orphan(&handler.config);
        let block_hash = header.block_hash;
        handler
            .block_pool
            .test_insert_block_into_cache(seal(&header), false)
            .await;
        assert!(
            handler
                .block_pool
                .dedup_and_maybe_emit_reprocess(&block_hash, header.height)
                .await,
            "the first observation of a parked block arms the cooldown"
        );
        assert_reprocess_for(
            sync_rx
                .try_recv()
                .expect("priming observation emits AttemptReprocessingBlock"),
            block_hash,
        );
        record_block_seen(&handler, block_hash);

        tokio::time::advance(Duration::from_secs(9)).await;
        let during = handler
            .handle_block_header(
                IrysPeerId::from(IrysAddress::repeat_byte(0x21)),
                IrysAddress::repeat_byte(0x21),
                Arc::clone(&header),
                dummy_addr(),
            )
            .await;
        assert!(during.is_ok(), "{during:?}");
        assert!(
            sync_rx.try_recv().is_err(),
            "a re-gossip inside the cooldown must not emit again"
        );

        tokio::time::advance(Duration::from_secs(2)).await;
        let after = handler
            .handle_block_header(
                IrysPeerId::from(IrysAddress::repeat_byte(0x22)),
                IrysAddress::repeat_byte(0x22),
                header,
                dummy_addr(),
            )
            .await;
        assert!(after.is_ok(), "{after:?}");
        assert_reprocess_for(
            sync_rx
                .try_recv()
                .expect("a re-gossip after the cooldown must emit AttemptReprocessingBlock"),
            block_hash,
        );
    }

    /// A sync pull of a parked block uses `pull_and_process_block`, which
    /// calls `handle_block_header`. That path must reach the reprocess helper
    /// instead of returning success at the seen-skip.
    #[tokio::test]
    async fn sync_pull_of_parked_block_emits_reprocess() {
        let (_dir, peer_list, handler, mut sync_rx) = test_handler_with_sync();
        let header = signed_orphan(&handler.config);
        let block_hash = header.block_hash;
        handler
            .block_pool
            .test_insert_block_into_cache(seal(&header), false)
            .await;
        record_block_seen(&handler, block_hash);

        let served = Arc::clone(&header);
        let server = FakeGossipServer::new();
        server.set_on_pull_data_request(move |request| match request {
            GossipDataRequestV2::BlockHeader(hash) if hash == served.block_hash => {
                GossipResponse::Accepted(Some(GossipDataV2::BlockHeader(Arc::clone(&served))))
            }
            _ => GossipResponse::Accepted(None),
        });
        let addr = server.spawn();
        add_peer(&peer_list, 0x61, addr, true);

        let pulled = handler.pull_and_process_block(block_hash, false).await;
        assert!(pulled.is_ok(), "{pulled:?}");
        assert_reprocess_for(
            sync_rx
                .try_recv()
                .expect("a sync pull of a parked block must emit AttemptReprocessingBlock"),
            block_hash,
        );
    }

    /// A block that is being processed is still skipped. A re-gossip must not
    /// emit `AttemptReprocessingBlock` and must not pull the body.
    #[tokio::test]
    async fn processing_block_seen_again_is_skipped() {
        let (_dir, peer_list, handler, mut sync_rx) = test_handler_with_sync();
        let header = signed_orphan(&handler.config);
        let block_hash = header.block_hash;
        handler
            .block_pool
            .test_insert_block_into_cache(seal(&header), true)
            .await;
        record_block_seen(&handler, block_hash);

        let pulls = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&pulls);
        let server = FakeGossipServer::new();
        server.set_on_pull_data_request(move |_| {
            hits.fetch_add(1, Ordering::SeqCst);
            GossipResponse::Accepted(None)
        });
        let addr = server.spawn();
        let peer_id = add_peer(&peer_list, 0x71, addr, true);

        assert!(
            handler
                .skip_seen_block(&block_hash)
                .await
                .expect("skip check"),
            "a block that is being processed must still be skipped"
        );
        let result = handler
            .handle_block_header(peer_id, IrysAddress::repeat_byte(0x71), header, addr)
            .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(
            sync_rx.try_recv().is_err(),
            "no reprocess for an in-flight block"
        );
        assert_eq!(pulls.load(Ordering::SeqCst), 0, "no body pull");
    }

    /// A recently processed block has left the in-flight cache. A re-gossip
    /// must still be skipped, and must not pull the body again.
    #[tokio::test]
    async fn recently_processed_block_seen_again_is_skipped() {
        let (_dir, peer_list, handler, mut sync_rx) = test_handler_with_sync();
        let header = signed_orphan(&handler.config);
        let block_hash = header.block_hash;
        handler
            .block_pool
            .test_insert_block_into_cache(seal(&header), true)
            .await;
        handler
            .block_pool
            .remove_block_from_cache(&block_hash, BlockRemovalReason::SuccessfullyProcessed)
            .await;
        assert!(!handler.block_pool.contains_block(&block_hash).await);
        assert!(
            handler.block_pool.is_block_known_locally(&block_hash).await,
            "recently processed is still held locally"
        );
        record_block_seen(&handler, block_hash);

        let pulls = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&pulls);
        let server = FakeGossipServer::new();
        server.set_on_pull_data_request(move |_| {
            hits.fetch_add(1, Ordering::SeqCst);
            GossipResponse::Accepted(None)
        });
        let addr = server.spawn();
        let peer_id = add_peer(&peer_list, 0x72, addr, true);

        assert!(
            handler
                .skip_seen_block(&block_hash)
                .await
                .expect("skip check"),
            "a recently processed block must still be skipped"
        );
        let result = handler
            .handle_block_header(peer_id, IrysAddress::repeat_byte(0x72), header, addr)
            .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(
            sync_rx.try_recv().is_err(),
            "no reprocess for a recently processed block"
        );
        assert_eq!(
            pulls.load(Ordering::SeqCst),
            0,
            "a recently processed block must not be pulled again"
        );
    }
}

#[cfg(test)]
mod execution_payload_tests {
    use super::*;
    use crate::tests::util::{FakeGossipServer, data_handler_stub, peer_list_item};
    use crate::types::GossipResponse;
    use irys_domain::PeerList;
    use irys_domain::chain_sync_state::ChainSyncState;
    use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::v2::GossipDataV2;
    use irys_types::{DatabaseProvider, DbSyncMode, NodeConfig, PeerScore};
    use reth::primitives::Header;
    use reth::primitives::proofs::calculate_ommers_root;
    use reth_ethereum_primitives::BlockBody as EvmBlockBody;

    type TestHandler =
        GossipDataHandler<crate::tests::util::MempoolStub, crate::tests::util::BlockDiscoveryStub>;

    fn test_handler() -> (irys_testing_utils::tempfile::TempDir, Arc<TestHandler>) {
        let dir = TempDirBuilder::new().with_tracing().build();
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = dir.path().to_path_buf();
        let config = Config::new_with_random_peer_id(node_config);
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(
                &config.node_config.base_directory,
                DbSyncMode::UtterlyNoSync,
            )
            .expect("open test db"),
        ));
        let peer_list = PeerList::test_mock().expect("peer list");
        let handler = data_handler_stub(&config, &peer_list, db, ChainSyncState::new(false, false));
        (dir, handler)
    }

    fn payload_request(block: Block) -> GossipRequestV2<Block> {
        let miner_address = IrysAddress::repeat_byte(0x33);
        GossipRequestV2 {
            peer_id: IrysPeerId::from(miner_address),
            miner_address,
            data: block,
        }
    }

    /// Returns a block whose header commits to one ommer, and a copy with
    /// the same header (so the same hash) but an empty body.
    fn matching_and_mismatched_blocks() -> (Block, Block) {
        let ommers = vec![Header::default()];
        let header = Header {
            number: 1,
            ommers_hash: calculate_ommers_root(&ommers),
            ..Default::default()
        };
        let matching = Block {
            header: header.clone(),
            body: EvmBlockBody {
                ommers,
                ..Default::default()
            },
        };
        let mismatched = Block {
            header,
            body: EvmBlockBody::default(),
        };
        (matching, mismatched)
    }

    #[tokio::test]
    async fn mismatched_payload_is_rejected_and_matching_copy_is_accepted() {
        let (_dir, handler) = test_handler();
        let (matching, mismatched) = matching_and_mismatched_blocks();
        let evm_block_hash = matching.hash_slow();
        assert_eq!(mismatched.hash_slow(), evm_block_hash);

        let result = handler
            .handle_execution_payload(payload_request(mismatched))
            .await;
        assert!(
            matches!(
                result,
                Err(GossipError::InvalidData(
                    InvalidDataError::ExecutionPayloadBodyMismatch(_)
                ))
            ),
            "{result:?}"
        );
        assert!(
            !handler
                .execution_payload_cache
                .is_payload_in_cache(&evm_block_hash)
                .await
        );
        assert!(
            !handler
                .cache
                .seen_execution_payload_from_any_peer(&evm_block_hash)
                .expect("seen check"),
            "a rejected copy must not be recorded as seen"
        );

        handler
            .handle_execution_payload(payload_request(matching.clone()))
            .await
            .expect("matching copy is accepted");
        let cached = handler
            .execution_payload_cache
            .get_sealed_block_from_cache(&evm_block_hash)
            .await
            .expect("matching copy is cached");
        assert_eq!(cached.into_block(), matching);
        assert!(
            handler
                .cache
                .seen_execution_payload_from_any_peer(&evm_block_hash)
                .expect("seen check")
        );
    }

    /// A peer that serves a payload the handler rejects on every attempt
    /// must surface as an error to the caller, not as success.
    #[tokio::test]
    async fn pull_fails_when_every_served_payload_is_rejected() {
        let (_dir, handler) = test_handler();
        let (matching, mismatched) = matching_and_mismatched_blocks();
        let evm_block_hash = matching.hash_slow();

        let server = FakeGossipServer::new();
        server.set_on_pull_data_request(move |_| {
            GossipResponse::Accepted(Some(GossipDataV2::ExecutionPayload(mismatched.clone())))
        });
        let addr = server.spawn();
        let peer = peer_list_item(0x41, addr, true, PeerScore::INITIAL);
        handler.peer_list.add_or_update_peer(peer, true);

        let result = handler
            .pull_and_add_execution_payload_to_cache(evm_block_hash, false)
            .await;
        assert!(
            matches!(
                result,
                Err(GossipError::InvalidData(
                    InvalidDataError::ExecutionPayloadBodyMismatch(_)
                ))
            ),
            "{result:?}"
        );
        assert!(
            !handler
                .execution_payload_cache
                .is_payload_in_cache(&evm_block_hash)
                .await
        );
    }

    /// An EVM payload gossiped before its Irys block must be served when
    /// validation of that block waits for it.
    #[tokio::test]
    async fn early_gossiped_payload_is_served_to_validation() {
        let (_dir, handler) = test_handler();
        let (matching, _) = matching_and_mismatched_blocks();
        let evm_block_hash = matching.hash_slow();

        handler
            .handle_execution_payload(payload_request(matching.clone()))
            .await
            .expect("payload is accepted");

        let payload = tokio::time::timeout(
            Duration::from_secs(5),
            handler
                .execution_payload_cache
                .wait_for_sealed_block(&evm_block_hash, false),
        )
        .await
        .expect("served from the cache without a network request")
        .expect("payload is in the cache");
        assert_eq!(payload.into_block(), matching);
    }
}
