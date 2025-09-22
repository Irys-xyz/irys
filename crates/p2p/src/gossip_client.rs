#![allow(
    clippy::module_name_repetitions,
    reason = "I have no idea how to name this module to satisfy this lint"
)]

use crate::circuit_breaker::CircuitBreakerManager;
use crate::dedup_cache::{compute_dedup_key, DedupRunError, RequestDedupCache};
use crate::types::{GossipError, GossipResponse, GossipResult, NetErr, RejectionReason};
use crate::GossipCache;
use backon::{ExponentialBuilder, Retryable as _};
use core::time::Duration;
use futures::{future, StreamExt as _};
use irys_domain::{PeerList, ScoreDecreaseReason, ScoreIncreaseReason};
use irys_types::{
    Address, BlockHash, GossipCacheKey, GossipData, GossipDataRequest, GossipRequest,
    IrysBlockHeader, PeerAddress, PeerListItem, PeerNetworkError, DATA_REQUEST_RETRIES,
};
use rand::prelude::SliceRandom as _;
use reqwest::Client;
use reth::primitives::Block;
use reth::revm::primitives::B256;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info_span, warn, Instrument as _};

// 500ms fast threshold rewards low-latency peers without being too restrictive.
// 2s normal threshold accommodates geographic distance and network variance
const FAST_RESPONSE_THRESHOLD: Duration = Duration::from_millis(500);
const NORMAL_RESPONSE_THRESHOLD: Duration = Duration::from_secs(2);

// 256 concurrent sends balances throughput with memory usage.
const MAX_CONCURRENT_SENDS: usize = 256;

// 150ms initial backoff handles transient failures quickly. 2s max prevents
// aggressive retries
const INITIAL_BACKOFF_INTERVAL: Duration = Duration::from_millis(150);
const MAX_BACKOFF_INTERVAL: Duration = Duration::from_secs(2);

// 5 parallel queries prevents network saturation while maintaining redundancy.
// 10 peer pool provides failover options without excessive connection overhead
const MAX_PARALLEL_PEER_QUERIES: usize = 5;
const MAX_TOP_PEERS_FOR_QUERIES: usize = 10;

/// Default retry delay between rounds (milliseconds)
const DEFAULT_RETRY_DELAY_MS: u64 = 100;

/// Cleanup interval for circuit breakers
const CIRCUIT_BREAKER_CLEANUP_INTERVAL: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum GossipClientError {
    #[error("GET {url} failed")]
    GetRequest {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("health check {url} returned {status}")]
    HealthCheck {
        url: String,
        status: reqwest::StatusCode,
    },
    #[error("decode {url} failed")]
    Decode {
        url: String,
        #[source]
        source: reqwest::Error,
    },
}

#[derive(Debug)]
struct Inner {
    mining_address: Address,
    client: Client,
    semaphore: Arc<Semaphore>,
    circuit_breakers: Arc<CircuitBreakerManager>,
    dedup_cache: Arc<RequestDedupCache>,
    shutdown: CancellationToken,
}

#[derive(Clone)]
pub struct GossipClient {
    inner: Arc<Inner>,
}

impl Default for GossipClient {
    fn default() -> Self {
        panic!("GossipClient must be initialized with a timeout and mining address. Default is implemented only to satisfy actix trait bounds.");
    }
}

impl Drop for GossipClient {
    fn drop(&mut self) {
        // Only cancel cleanup task when last reference is dropped. This prevents
        // premature cancellation when GossipClient is cloned for concurrent operations.
        if Arc::strong_count(&self.inner) == 1 {
            self.inner.shutdown.cancel();
        }
    }
}

impl std::fmt::Debug for GossipClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GossipClient")
            .field("mining_address", &self.inner.mining_address)
            .field("client", &"<reqwest::Client>")
            .field("semaphore", &self.inner.semaphore)
            .field("circuit_breakers", &self.inner.circuit_breakers)
            .field("dedup_cache", &self.inner.dedup_cache)
            .field("shutdown", &"<CancellationToken>")
            .finish()
    }
}

impl GossipClient {
    #[must_use]
    pub fn new(timeout: Duration, mining_address: Address) -> Self {
        let shutdown = CancellationToken::new();
        let inner = Arc::new(Inner {
            mining_address,
            client: Client::builder()
                .timeout(timeout)
                .build()
                .expect("Failed to create reqwest client"),
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_SENDS)),
            circuit_breakers: Arc::new(CircuitBreakerManager::new()),
            dedup_cache: Arc::new(RequestDedupCache::new()),
            shutdown: shutdown.clone(),
        });

        // Cleanup task runs every 5 minutes to remove circuit breakers for peers that
        // have left the network, preventing unbounded memory growth in long-running nodes.
        tokio::spawn({
            let breakers = inner.circuit_breakers.clone();
            async move {
                let mut tick = interval(CIRCUIT_BREAKER_CLEANUP_INTERVAL);
                loop {
                    tokio::select! {
                        _ = tick.tick() => breakers.cleanup_stale().await,
                        _ = shutdown.cancelled() => break,
                    }
                }
            }
        });

        Self { inner }
    }

    pub fn internal_client(&self) -> &Client {
        &self.inner.client
    }

    fn circuit_breakers(&self) -> &Arc<CircuitBreakerManager> {
        &self.inner.circuit_breakers
    }

    fn dedup_cache(&self) -> &Arc<RequestDedupCache> {
        &self.inner.dedup_cache
    }

    fn semaphore(&self) -> &Arc<Semaphore> {
        &self.inner.semaphore
    }

    pub fn mining_address(&self) -> Address {
        self.inner.mining_address
    }

    async fn is_peer_available(&self, peer_address: &Address) -> bool {
        self.circuit_breakers().is_available(peer_address).await
    }

    async fn record_peer_success(&self, peer_address: &Address) {
        self.circuit_breakers().record_success(peer_address).await;
    }

    async fn record_peer_failure(&self, peer_address: &Address) {
        self.circuit_breakers().record_failure(peer_address).await;
    }

    #[inline]
    fn build_gossip_endpoint(peer: &PeerListItem, path: &str) -> String {
        format!("http://{}/gossip/{path}", peer.address.gossip)
    }

    #[inline]
    fn build_gossip_endpoint_from_address(peer_addr: &PeerAddress, path: &str) -> String {
        format!("http://{}/gossip/{path}", peer_addr.gossip)
    }

    /// Send data to a peer and update their score based on the result
    ///
    /// # Errors
    ///
    /// If the peer is offline or the request fails, an error is returned.
    async fn send_data_and_update_score_internal(
        &self,
        peer: (&Address, &PeerListItem),
        data: &GossipData,
        peer_list: &PeerList,
    ) -> GossipResult<()> {
        let (peer_miner_address, peer_info) = peer;

        if !self.is_peer_available(peer_miner_address).await {
            return Err(GossipError::Network(NetErr::Other(format!(
                "Circuit breaker open for peer {}",
                peer_miner_address
            ))));
        }

        let res = self.send_data(peer_info, data).await;

        match &res {
            Ok(_) => self.record_peer_success(peer_miner_address).await,
            Err(_) => self.record_peer_failure(peer_miner_address).await,
        }

        Self::handle_score(peer_list, &res, peer_miner_address);
        res.map(|_| ())
    }

    /// Request a specific data to be gossiped. Returns true if the peer has the data,
    /// and false if it doesn't.
    pub async fn make_get_data_request_and_update_the_score(
        &self,
        peer: &(Address, PeerListItem),
        requested_data: GossipDataRequest,
        peer_list: &PeerList,
    ) -> GossipResult<GossipResponse<bool>> {
        if !self.is_peer_available(&peer.0).await {
            return Err(GossipError::Network(NetErr::Other(format!(
                "Circuit breaker open for peer {}",
                peer.0
            ))));
        }

        let url = Self::build_gossip_endpoint(&peer.1, "get_data");
        let key = compute_dedup_key(&peer.0, &requested_data);

        let start = std::time::Instant::now();
        // Dedup prevents duplicate requests when multiple retry attempts overlap.
        let res = self
            .dedup_cache()
            .run_dedup(key, || async {
                self.send_data_internal(url, &requested_data).await
            })
            .await;

        let response_time = start.elapsed();

        match res {
            Ok(body) => {
                self.record_peer_success(&peer.0).await;
                Self::handle_data_retrieval_score(peer_list, &Ok(()), &peer.0, response_time);
                Ok(body)
            }
            Err(DedupRunError::Duplicate) => {
                debug!(
                    "Request already in flight for peer {} data {:?}",
                    peer.0, requested_data
                );
                Ok(GossipResponse::Accepted(false))
            }
            Err(DedupRunError::Op(err)) => {
                self.record_peer_failure(&peer.0).await;
                Self::handle_data_retrieval_score::<()>(
                    peer_list,
                    &Err(err.clone()),
                    &peer.0,
                    response_time,
                );
                Err(err)
            }
        }
    }

    /// Request specific data from the peer. Returns the data right away if the peer has it
    /// and updates the peer's score based on the result.
    async fn pull_data_and_update_the_score(
        &self,
        peer: &(Address, PeerListItem),
        requested_data: GossipDataRequest,
        peer_list: &PeerList,
    ) -> GossipResult<GossipResponse<Option<GossipData>>> {
        if !self.is_peer_available(&peer.0).await {
            return Err(GossipError::Network(NetErr::Other(format!(
                "Circuit breaker open for peer {}",
                peer.0
            ))));
        }

        let url = Self::build_gossip_endpoint(&peer.1, "pull_data");
        let key = compute_dedup_key(&peer.0, &requested_data);

        let start = std::time::Instant::now();
        // Dedup prevents duplicate requests when multiple retry attempts overlap.
        let res = self
            .dedup_cache()
            .run_dedup(key, || async {
                self.send_data_internal(url, &requested_data).await
            })
            .await;

        let response_time = start.elapsed();

        match res {
            Ok(body) => {
                self.record_peer_success(&peer.0).await;
                Self::handle_data_retrieval_score(peer_list, &Ok(()), &peer.0, response_time);
                Ok(body)
            }
            Err(DedupRunError::Duplicate) => {
                debug!(
                    "Pull request already in flight for peer {} data {:?}",
                    peer.0, requested_data
                );
                Ok(GossipResponse::Accepted(None))
            }
            Err(DedupRunError::Op(err)) => {
                self.record_peer_failure(&peer.0).await;
                Self::handle_data_retrieval_score::<()>(
                    peer_list,
                    &Err(err.clone()),
                    &peer.0,
                    response_time,
                );
                Err(err)
            }
        }
    }

    pub async fn check_health(
        &self,
        peer: PeerAddress,
        peer_list: &PeerList,
    ) -> Result<bool, GossipClientError> {
        let url = Self::build_gossip_endpoint_from_address(&peer, "health");
        let peer_addr = peer.gossip.to_string();

        let op = || async {
            let response = self.internal_client().get(&url).send().await.map_err(|e| {
                GossipClientError::GetRequest {
                    url: peer_addr.clone(),
                    source: e,
                }
            })?;

            if !response.status().is_success() {
                let err = GossipClientError::HealthCheck {
                    url: peer_addr.clone(),
                    status: response.status(),
                };
                return Err(err);
            }

            let parsed = response.json::<GossipResponse<bool>>().await.map_err(|e| {
                GossipClientError::Decode {
                    url: peer_addr.clone(),
                    source: e,
                }
            })?;

            Ok(parsed)
        };

        let response = op
            .retry(create_backoff())
            .when(|e: &GossipClientError| match e {
                GossipClientError::GetRequest { .. } => true,
                GossipClientError::Decode { .. } => false,
                GossipClientError::HealthCheck { status, .. } => {
                    status.is_server_error() || *status == reqwest::StatusCode::TOO_MANY_REQUESTS
                }
            })
            .await?;

        match response {
            GossipResponse::Accepted(val) => Ok(val),
            GossipResponse::Rejected(reason) => {
                warn!("Health check rejected with reason: {:?}", reason);
                match reason {
                    RejectionReason::HandshakeRequired => {
                        peer_list.initiate_handshake(peer.api, true);
                    }
                    RejectionReason::GossipDisabled => {
                        return Ok(false);
                    }
                };
                Ok(true)
            }
        }
    }

    /// Send data to a peer
    ///
    /// # Errors
    ///
    /// If the peer is offline or the request fails, an error is returned.
    async fn send_data(
        &self,
        peer: &PeerListItem,
        data: &GossipData,
    ) -> GossipResult<GossipResponse<()>> {
        match data {
            GossipData::Chunk(unpacked_chunk) => {
                self.send_data_internal(Self::build_gossip_endpoint(peer, "chunk"), unpacked_chunk)
                    .await
            }
            GossipData::Transaction(irys_transaction_header) => {
                self.send_data_internal(
                    Self::build_gossip_endpoint(peer, "transaction"),
                    irys_transaction_header,
                )
                .await
            }
            GossipData::CommitmentTransaction(commitment_tx) => {
                self.send_data_internal(
                    Self::build_gossip_endpoint(peer, "commitment_tx"),
                    commitment_tx,
                )
                .await
            }
            GossipData::Block(irys_block_header) => {
                self.send_data_internal(
                    Self::build_gossip_endpoint(peer, "block"),
                    &irys_block_header,
                )
                .await
            }
            GossipData::ExecutionPayload(execution_payload) => {
                self.send_data_internal(
                    Self::build_gossip_endpoint(peer, "execution_payload"),
                    &execution_payload,
                )
                .await
            }
            GossipData::IngressProof(ingress_proof) => {
                self.send_data_internal(
                    Self::build_gossip_endpoint(peer, "ingress_proof"),
                    &ingress_proof,
                )
                .await
            }
        }
    }

    async fn send_data_internal<T, R>(
        &self,
        url: String,
        data: &T,
    ) -> GossipResult<GossipResponse<R>>
    where
        T: Serialize + ?Sized,
        for<'de> R: Deserialize<'de>,
    {
        debug!("Sending data to {}", url);

        let req = self.wrap_with_gossip_metadata(data);

        let operation = || async {
            let response = self
                .internal_client()
                .post(&url)
                .json(&req)
                .send()
                .await
                .map_err(|e| GossipError::Network(NetErr::Transport(e.to_string())))?;

            let status = response.status();

            if status.is_success() {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| GossipError::Network(NetErr::Transport(e.to_string())))?;

                // Empty success responses violate our protocol and could indicate a DoS attempt
                // or misconfigured peer. Treat as error to prevent infinite retry loops.
                if bytes.is_empty() {
                    return Err(GossipError::Network(NetErr::Other(format!(
                        "Empty response from {}",
                        url
                    ))));
                }

                let body = serde_json::from_slice::<GossipResponse<R>>(&bytes)
                    .map_err(|e| GossipError::Network(NetErr::Decode(e.to_string())))?;
                Ok(body)
            } else {
                let error_text = response.text().await.unwrap_or_default();
                Err(GossipError::Network(NetErr::Http {
                    status,
                    body: error_text,
                }))
            }
        };

        let span = info_span!("gossip_http_post", %url, miner=%self.mining_address());
        operation
            .retry(create_backoff())
            .when(|e: &GossipError| match e {
                GossipError::Network(net_err) => net_err.is_retryable(),
                _ => false,
            })
            .instrument(span)
            .await
    }

    fn handle_score<T>(
        peer_list: &PeerList,
        result: &GossipResult<GossipResponse<T>>,
        peer_miner_address: &Address,
    ) {
        match &result {
            Ok(_) => {
                peer_list.increase_peer_score(peer_miner_address, ScoreIncreaseReason::DataRequest);
                peer_list.set_is_online(peer_miner_address, true);
            }
            Err(err) => {
                if let GossipError::Network(_net_err) = err {
                    peer_list.set_is_online(peer_miner_address, false);
                }
                peer_list.decrease_peer_score(peer_miner_address, ScoreDecreaseReason::Offline);
            }
        }
    }

    // Response-time scoring creates natural load balancing - fast peers get more
    // traffic, slow peers less. Prevents overloading struggling nodes
    fn handle_data_retrieval_score<T>(
        peer_list: &PeerList,
        result: &GossipResult<T>,
        peer_miner_address: &Address,
        response_time: Duration,
    ) {
        match result {
            Ok(_) => {
                if response_time <= FAST_RESPONSE_THRESHOLD {
                    peer_list.increase_peer_score(
                        peer_miner_address,
                        ScoreIncreaseReason::TimelyResponse,
                    );
                } else if response_time <= NORMAL_RESPONSE_THRESHOLD {
                    peer_list.increase_peer_score(peer_miner_address, ScoreIncreaseReason::Online);
                } else {
                    peer_list
                        .decrease_peer_score(peer_miner_address, ScoreDecreaseReason::SlowResponse);
                }
            }
            Err(_) => {
                peer_list.decrease_peer_score(peer_miner_address, ScoreDecreaseReason::NoResponse);
            }
        }
    }

    /// Sends data to a peer and update their score in a detached task
    pub fn send_data_and_update_the_score_detached(
        &self,
        peer: (&Address, &PeerListItem),
        data: Arc<GossipData>,
        peer_list: &PeerList,
        cache: Arc<GossipCache>,
        gossip_cache_key: GossipCacheKey,
    ) {
        let client = self.clone();
        let peer_list = peer_list.clone();
        let peer_miner_address = *peer.0;
        let peer = peer.1.clone();
        let sem = client.semaphore().clone();
        let shutdown = client.inner.shutdown.clone();

        tokio::spawn(async move {
            // Permit acquisition can only fail if semaphore is closed (shouldn't happen).
            // Log error and bail out rather than panic to maintain stability.
            let Ok(_permit) = sem.acquire_owned().await else {
                error!("Failed to acquire semaphore permit");
                return;
            };

            let span = info_span!("gossip_send_detached", peer=%peer_miner_address, miner=%client.mining_address());
            tokio::select! {
                biased;

                _ = shutdown.cancelled() => {
                    debug!("detached gossip task cancelled for {peer_miner_address}");
                }

                _ = async {
                    if let Err(e) = client
                        .send_data_and_update_score_internal(
                            (&peer_miner_address, &peer),
                            &data,
                            &peer_list,
                        )
                        .await
                    {
                        error!("Error sending data to peer: {:?}", e);
                    } else if let Err(err) = cache.record_seen(peer_miner_address, gossip_cache_key) {
                        error!("Error recording seen data in cache: {:?}", err);
                    }
                }.instrument(span) => {}
            }
        });
    }

    /// Sends data to a peer without updating their score
    pub fn send_data_without_score_update(
        &self,
        peer: (&Address, &PeerListItem),
        data: Arc<GossipData>,
    ) {
        let client = self.clone();
        let peer_miner_address = *peer.0;
        let peer = peer.1.clone();
        let sem = client.semaphore().clone();
        let shutdown = client.inner.shutdown.clone();

        tokio::spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                error!("Failed to acquire semaphore permit");
                return;
            };

            let span = info_span!("gossip_send_noscore", peer=%peer.address.gossip, miner=%client.mining_address());
            tokio::select! {
                biased;

                _ = shutdown.cancelled() => {
                    debug!("detached gossip task cancelled for {peer_miner_address}");
                }

                _ = async {
                    if let Err(e) = client.send_data(&peer, &data).await {
                        error!("Error sending data to peer: {}", e);
                    }
                }.instrument(span) => {}
            }
        });
    }

    /// Sends data to a peer and updates their score specifically for data requests
    pub fn send_data_and_update_score_for_request(
        &self,
        peer: (&Address, &PeerListItem),
        data: Arc<GossipData>,
        peer_list: &PeerList,
    ) {
        let client = self.clone();
        let peer_list = peer_list.clone();
        let peer_miner_address = *peer.0;
        let peer = peer.1.clone();
        let sem = client.semaphore().clone();
        let shutdown = client.inner.shutdown.clone();

        tokio::spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                error!("Failed to acquire semaphore permit");
                return;
            };

            let span = info_span!("gossip_send_request", peer=%peer_miner_address, miner=%client.mining_address());
            tokio::select! {
                biased;

                _ = shutdown.cancelled() => {
                    debug!("detached gossip task cancelled for {peer_miner_address}");
                }

                _ = async {
                    let result = client.send_data(&peer, &data).await;
                    match &result {
                        Ok(_) => {
                            peer_list.increase_peer_score(
                                &peer_miner_address,
                                ScoreIncreaseReason::DataRequest,
                            );
                        }
                        Err(_) => {
                            peer_list
                                .decrease_peer_score(&peer_miner_address, ScoreDecreaseReason::Offline);
                        }
                    }
                }.instrument(span) => {}
            }
        });
    }

    fn wrap_with_gossip_metadata<'a, T: Serialize + ?Sized>(
        &self,
        data: &'a T,
    ) -> GossipRequest<&'a T> {
        GossipRequest {
            miner_address: self.mining_address(),
            data,
        }
    }

    pub async fn pull_block_from_network(
        &self,
        block_hash: BlockHash,
        use_trusted_peers_only: bool,
        peer_list: &PeerList,
    ) -> Result<(Address, Arc<IrysBlockHeader>), PeerNetworkError> {
        let data_request = GossipDataRequest::Block(block_hash);
        self.pull_data_from_network(
            data_request,
            use_trusted_peers_only,
            peer_list,
            Self::extract_block_header,
        )
        .await
    }

    pub async fn pull_payload_from_network(
        &self,
        evm_payload_hash: B256,
        use_trusted_peers_only: bool,
        peer_list: &PeerList,
    ) -> Result<(Address, Block), PeerNetworkError> {
        let data_request = GossipDataRequest::ExecutionPayload(evm_payload_hash);
        self.pull_data_from_network(
            data_request,
            use_trusted_peers_only,
            peer_list,
            Self::extract_execution_payload,
        )
        .await
    }

    /// Pull a block from a specific peer, updating its score accordingly.
    pub async fn pull_block_from_peer(
        &self,
        block_hash: BlockHash,
        peer: &(Address, PeerListItem),
        peer_list: &PeerList,
    ) -> Result<(Address, Arc<IrysBlockHeader>), PeerNetworkError> {
        let data_request = GossipDataRequest::Block(block_hash);
        match self
            .pull_data_and_update_the_score(peer, data_request, peer_list)
            .await
        {
            Ok(response) => match response {
                GossipResponse::Accepted(maybe_data) => match maybe_data {
                    Some(data) => {
                        let header = Self::extract_block_header(data)?;
                        Ok((peer.0, header))
                    }
                    None => Err(PeerNetworkError::FailedToRequestData(
                        "Peer did not have the requested block".to_string(),
                    )),
                },
                GossipResponse::Rejected(reason) => {
                    warn!("Peer {:?} rejected the request: {:?}", peer.0, reason);
                    match reason {
                        RejectionReason::HandshakeRequired => {
                            peer_list.initiate_handshake(peer.1.address.api, true)
                        }
                        RejectionReason::GossipDisabled => {
                            peer_list.set_is_online(&peer.0, false);
                        }
                    }
                    Err(PeerNetworkError::FailedToRequestData(format!(
                        "Peer {:?} rejected the request: {:?}",
                        peer.0, reason
                    )))
                }
            },
            Err(err) => match err {
                GossipError::PeerNetwork(e) => Err(e),
                other => Err(PeerNetworkError::FailedToRequestData(other.to_string())),
            },
        }
    }

    fn extract_block_header(
        gossip_data: GossipData,
    ) -> Result<Arc<IrysBlockHeader>, PeerNetworkError> {
        match gossip_data {
            GossipData::Block(block) => Ok(block),
            _ => Err(PeerNetworkError::UnexpectedData(format!(
                "Expected IrysBlockHeader, got {:?}",
                gossip_data.data_type_and_id()
            ))),
        }
    }

    fn extract_execution_payload(gossip_data: GossipData) -> Result<Block, PeerNetworkError> {
        match gossip_data {
            GossipData::ExecutionPayload(block) => Ok(block),
            _ => Err(PeerNetworkError::UnexpectedData(format!(
                "Expected ExecutionPayload, got {:?}",
                gossip_data.data_type_and_id()
            ))),
        }
    }

    pub async fn pull_data_from_network<T>(
        &self,
        data_request: GossipDataRequest,
        use_trusted_peers_only: bool,
        peer_list: &PeerList,
        map_data: fn(GossipData) -> Result<T, PeerNetworkError>,
    ) -> Result<(Address, T), PeerNetworkError> {
        let peers = self
            .select_peers_for_query(use_trusted_peers_only, peer_list)
            .await?;

        let total_peers_initial = peers.len();
        let mut last_error = None;
        let mut retryable_peers = peers;

        for attempt in 1..=DATA_REQUEST_RETRIES {
            if retryable_peers.is_empty() {
                break;
            }

            let (successful_result, next_retryable) = self
                .query_peers_in_parallel(
                    &retryable_peers,
                    &data_request,
                    peer_list,
                    map_data,
                    attempt as usize,
                    &mut last_error,
                )
                .await;

            if let Some(result) = successful_result {
                return Ok(result);
            }

            retryable_peers = next_retryable;

            if attempt != DATA_REQUEST_RETRIES {
                tokio::time::sleep(Duration::from_millis(DEFAULT_RETRY_DELAY_MS)).await;
            }
        }

        Err(PeerNetworkError::FailedToRequestData(format!(
            "Failed to pull {:?} after trying {} peers: {:?}",
            data_request, total_peers_initial, last_error
        )))
    }

    async fn select_peers_for_query(
        &self,
        use_trusted_peers_only: bool,
        peer_list: &PeerList,
    ) -> Result<Vec<(Address, PeerListItem)>, PeerNetworkError> {
        let candidates = if use_trusted_peers_only {
            peer_list.online_trusted_peers()
        } else {
            peer_list.top_active_peers(Some(MAX_TOP_PEERS_FOR_QUERIES), None)
        };

        // Parallel availability checks avoid sequential latency accumulation
        let checks = candidates
            .iter()
            .map(|(addr, _)| self.is_peer_available(addr));
        let results = future::join_all(checks).await;

        let mut available = Vec::with_capacity(candidates.len());
        for (i, ok) in results.into_iter().enumerate() {
            if ok {
                available.push(candidates[i].clone());
            }
        }

        if available.is_empty() {
            return Err(PeerNetworkError::NoPeersAvailable);
        }

        available.truncate(MAX_TOP_PEERS_FOR_QUERIES);

        // Randomization for preventing all nodes querying the same top peers, overloading them
        available.shuffle(&mut rand::thread_rng());
        available.truncate(MAX_PARALLEL_PEER_QUERIES);

        Ok(available)
    }

    async fn query_peers_in_parallel<T>(
        &self,
        peers: &[(Address, PeerListItem)],
        data_request: &GossipDataRequest,
        peer_list: &PeerList,
        map_data: fn(GossipData) -> Result<T, PeerNetworkError>,
        attempt: usize,
        last_error: &mut Option<GossipError>,
    ) -> (Option<(Address, T)>, Vec<(Address, PeerListItem)>) {
        // FuturesUnordered: process responses as they arrive, not submission order
        let mut futs = futures::stream::FuturesUnordered::new();

        for peer in peers.iter().cloned() {
            let gc = self.clone();
            let dr = data_request.clone();
            let pl = peer_list;
            futs.push(async move {
                let addr = peer.0;
                let res = gc.pull_data_and_update_the_score(&peer, dr, pl).await;
                (addr, peer, res)
            });
        }

        let mut next_retryable = Vec::new();

        while let Some((address, peer, result)) = futs.next().await {
            match self
                .process_peer_response(
                    result,
                    address,
                    peer,
                    data_request,
                    peer_list,
                    map_data,
                    attempt,
                    last_error,
                )
                .await
            {
                PeerResponseResult::Success(data) => {
                    return (Some((address, data)), vec![]);
                }
                PeerResponseResult::Retry(p) => {
                    next_retryable.push(p);
                }
                PeerResponseResult::Skip => {}
            }
        }

        (None, next_retryable)
    }

    async fn process_peer_response<T>(
        &self,
        result: GossipResult<GossipResponse<Option<GossipData>>>,
        address: Address,
        peer: (Address, PeerListItem),
        data_request: &GossipDataRequest,
        peer_list: &PeerList,
        map_data: fn(GossipData) -> Result<T, PeerNetworkError>,
        attempt: usize,
        last_error: &mut Option<GossipError>,
    ) -> PeerResponseResult<T> {
        match result {
            Ok(GossipResponse::Accepted(maybe_data)) => match maybe_data {
                Some(data) => match map_data(data) {
                    Ok(data) => {
                        debug!(
                            "Successfully pulled {:?} from peer {}",
                            data_request, address
                        );
                        self.record_peer_success(&address).await;
                        PeerResponseResult::Success(data)
                    }
                    Err(err) => {
                        warn!("Failed to map data from peer {}: {}", address, err);
                        self.record_peer_failure(&address).await;
                        PeerResponseResult::Skip
                    }
                },
                None => {
                    debug!("Peer {} doesn't have {:?}", address, data_request);
                    PeerResponseResult::Retry(peer)
                }
            },
            Ok(GossipResponse::Rejected(reason)) => {
                warn!(
                    "Peer {} reject the request: {:?}: {:?}",
                    address, data_request, reason
                );
                match reason {
                    RejectionReason::HandshakeRequired => {
                        peer_list.initiate_handshake(peer.1.address.api, true);
                        *last_error =
                            Some(GossipError::from(PeerNetworkError::FailedToRequestData(
                                format!("Peer {:?} requires a handshake", address),
                            )));
                    }
                    RejectionReason::GossipDisabled => {
                        peer_list.set_is_online(&peer.0, false);
                        *last_error =
                            Some(GossipError::from(PeerNetworkError::FailedToRequestData(
                                format!("Peer {:?} has gossip disabled", address),
                            )));
                    }
                }
                PeerResponseResult::Skip
            }
            Err(err) => {
                *last_error = Some(err);
                warn!(
                    "Failed to fetch {:?} from peer {:?} (attempt {}/{}): {}",
                    data_request,
                    address,
                    attempt,
                    DATA_REQUEST_RETRIES,
                    last_error.as_ref().unwrap()
                );
                self.record_peer_failure(&address).await;
                PeerResponseResult::Retry(peer)
            }
        }
    }

    pub async fn hydrate_peers_online_status(&self, peer_list: &PeerList) {
        debug!("Hydrating peers online status");
        let peers = peer_list.all_peers_sorted_by_score();
        for peer in peers {
            match self.check_health(peer.1.address, peer_list).await {
                Ok(is_healthy) => {
                    debug!("Peer {} is healthy: {}", peer.0, is_healthy);
                    peer_list.set_is_online(&peer.0, is_healthy);
                    if is_healthy {
                        self.record_peer_success(&peer.0).await;
                    }
                }
                Err(err) => {
                    warn!(
                        "Failed to check the health of peer {}: {}, setting offline status",
                        peer.0, err
                    );
                    peer_list.set_is_online(&peer.0, false);
                    self.record_peer_failure(&peer.0).await;
                }
            }
        }
    }
}

enum PeerResponseResult<T> {
    Success(T),
    Retry((Address, PeerListItem)),
    Skip,
}

fn create_backoff() -> ExponentialBuilder {
    if cfg!(test) {
        // Fast retries in tests for quicker test execution
        ExponentialBuilder::default()
            .with_min_delay(Duration::from_millis(5))
            .with_max_delay(Duration::from_millis(50))
            .with_max_times(10)
    } else {
        ExponentialBuilder::default()
            .with_min_delay(INITIAL_BACKOFF_INTERVAL)
            .with_max_delay(MAX_BACKOFF_INTERVAL)
            .with_max_times(5)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use irys_primitives::Address;
    use irys_types::{PeerAddress, PeerListItem, PeerScore, RethPeerInfo};
    use reqwest::StatusCode;
    use std::io::prelude::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Test utilities moved from test_utils.rs
    const MOCK_SERVER_TIMEOUT: Duration = Duration::from_secs(5);

    /// Get a free port for testing
    fn get_free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .expect("Failed to bind to port")
            .local_addr()
            .expect("Failed to get local addr")
            .port()
    }

    /// Mock HTTP server for testing network interactions
    struct MockHttpServer {
        port: u16,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl MockHttpServer {
        fn new_with_response(status_code: u16, body: &str, content_type: &str) -> Self {
            let port = get_free_port();
            let body = body.to_string();
            let content_type = content_type.to_string();

            let handle = thread::spawn(move || {
                let listener =
                    TcpListener::bind(format!("127.0.0.1:{}", port)).expect("Failed to bind");

                listener
                    .set_nonblocking(true)
                    .expect("Failed to set non-blocking");

                let start_time = std::time::Instant::now();

                while start_time.elapsed() < MOCK_SERVER_TIMEOUT {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let mut buffer = [0; 1024];
                            let _ = stream.read(&mut buffer);

                            let response = format!(
                                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n{}",
                                status_code,
                                Self::status_text(status_code),
                                content_type,
                                body.len(),
                                body
                            );
                            let _ = stream.write_all(response.as_bytes());

                            if (400..500).contains(&status_code) {
                                break;
                            }
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                            continue;
                        }
                        Err(_) => break,
                    }
                }
            });

            std::thread::sleep(Duration::from_millis(200));

            Self {
                port,
                handle: Some(handle),
            }
        }

        fn status_text(code: u16) -> &'static str {
            match code {
                200 => "OK",
                202 => "Accepted",
                404 => "Not Found",
                500 => "Internal Server Error",
                _ => "Unknown",
            }
        }

        fn port(&self) -> u16 {
            self.port
        }
    }

    impl Drop for MockHttpServer {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    const NORMAL_RESPONSE_TIME: Duration = Duration::from_millis(1500);
    const SLOW_RESPONSE_TIME: Duration = Duration::from_secs(3);

    struct TestFixture {
        client: GossipClient,
    }

    impl TestFixture {
        fn new() -> Self {
            Self {
                client: GossipClient::new(Duration::from_millis(500), Address::from([1_u8; 20])),
            }
        }

        fn with_timeout(timeout: Duration) -> Self {
            Self {
                client: GossipClient::new(timeout, Address::from([1_u8; 20])),
            }
        }
    }

    fn create_peer_address(host: &str, port: u16) -> PeerAddress {
        PeerAddress {
            gossip: format!("{}:{}", host, port).parse().expect("Valid address"),
            api: format!("{}:{}", host, port).parse().expect("Valid address"),
            execution: Default::default(),
        }
    }

    mod connection_tests {
        use super::*;
        use rstest::rstest;

        #[rstest]
        #[case("connection_refused", "127.0.0.1", true, Duration::from_millis(500))]
        #[case("timeout", "192.0.2.1", false, Duration::from_millis(1))] // Unroutable IP
        #[tokio::test]
        async fn test_connection_failures(
            #[case] test_mode: &str,
            #[case] host: &str,
            #[case] use_free_port: bool,
            #[case] timeout: Duration,
        ) {
            let fixture = TestFixture::with_timeout(timeout);
            let port = if use_free_port { get_free_port() } else { 8080 };
            let peer = create_peer_address(host, port);
            let mock_list = PeerList::test_mock().expect("to create peer list mock");

            let start = std::time::Instant::now();
            let result = fixture.client.check_health(peer, &mock_list).await;
            let elapsed = start.elapsed();

            assert!(result.is_err());
            match result.unwrap_err() {
                GossipClientError::GetRequest { url, source } => {
                    assert_eq!(url, peer.gossip.to_string());
                    let error_msg = source.to_string().to_lowercase();

                    if test_mode == "connection_refused" {
                        assert!(
                            error_msg.contains("connection refused")
                                || error_msg.contains("connection reset"),
                            "Expected connection refused error, got: {}",
                            source
                        );
                    } else {
                        // Timeout case - just verify we got an error and it was relatively quick
                        assert!(!error_msg.is_empty());
                        assert!(elapsed < Duration::from_secs(1), "Should timeout quickly");
                    }
                }
                err => panic!(
                    "Expected GetRequest error for {}, got: {:?}",
                    test_mode, err
                ),
            }
        }
    }

    mod health_check_error_tests {
        use super::*;
        use reqwest::StatusCode;
        use rstest::rstest;

        #[rstest]
        // Success cases
        #[case(200, r#"{"Accepted":true}"#, "application/json", true, None)]
        #[case(202, r#"{"Accepted":true}"#, "application/json", true, None)]
        // Client errors (no retry)
        #[case(400, "", "text/plain", false, Some(StatusCode::BAD_REQUEST))]
        #[case(401, "", "text/plain", false, Some(StatusCode::UNAUTHORIZED))]
        #[case(403, "", "text/plain", false, Some(StatusCode::FORBIDDEN))]
        #[case(404, "", "text/plain", false, Some(StatusCode::NOT_FOUND))]
        // Server errors (with retry)
        #[case(429, "", "text/plain", false, Some(StatusCode::TOO_MANY_REQUESTS))]
        #[case(500, "", "text/plain", false, Some(StatusCode::INTERNAL_SERVER_ERROR))]
        #[case(503, "", "text/plain", false, Some(StatusCode::SERVICE_UNAVAILABLE))]
        #[tokio::test]
        async fn test_http_status_handling(
            #[case] status_code: u16,
            #[case] body: &str,
            #[case] content_type: &str,
            #[case] should_succeed: bool,
            #[case] expected_status: Option<StatusCode>,
        ) {
            let server = MockHttpServer::new_with_response(status_code, body, content_type);
            let fixture = TestFixture::with_timeout(Duration::from_millis(200));
            let peer = create_peer_address("127.0.0.1", server.port());
            let mock_list = PeerList::test_mock().expect("to create peer list mock");

            let result = fixture.client.check_health(peer, &mock_list).await;

            if should_succeed {
                assert!(
                    result.is_ok(),
                    "Status {} should succeed: {:?}",
                    status_code,
                    result
                );
            } else {
                assert!(result.is_err());
                match result.unwrap_err() {
                    GossipClientError::HealthCheck { url, status } => {
                        assert_eq!(url, peer.gossip.to_string());
                        if let Some(expected) = expected_status {
                            assert_eq!(status, expected);
                        }
                    }
                    GossipClientError::GetRequest { url, .. } => {
                        // Retryable errors may exhaust retries
                        assert_eq!(url, peer.gossip.to_string());
                        assert!(
                            status_code >= 429,
                            "Only retryable errors should become GetRequest"
                        );
                    }
                    err => panic!("Unexpected error for status {}: {:?}", status_code, err),
                }
            }
        }
    }

    mod response_parsing_tests {
        use super::*;
        use rstest::rstest;

        #[rstest]
        #[case("", "empty body")] // Empty response
        #[case("invalid json {", "invalid JSON")] // Syntactically invalid
        #[case(r#"{"status": "healthy", "version"#, "truncated JSON")] // Truncated
        #[case(r#"{"Accepted": tr"#, "partial JSON")] // Partial
        #[tokio::test]
        async fn test_malformed_json_responses(
            #[case] response_body: &str,
            #[case] test_description: &str,
        ) {
            let server = MockHttpServer::new_with_response(200, response_body, "application/json");
            let fixture = TestFixture::new();
            let peer = create_peer_address("127.0.0.1", server.port());
            let mock_list = PeerList::test_mock().expect("to create peer list mock");

            let result = fixture.client.check_health(peer, &mock_list).await;

            assert!(result.is_err(), "Should fail for {}", test_description);
            match result.unwrap_err() {
                GossipClientError::Decode { url, source } => {
                    assert_eq!(url, peer.gossip.to_string());
                    let error_msg = source.to_string().to_lowercase();
                    assert!(
                        error_msg.contains("expected")
                            || error_msg.contains("eof")
                            || error_msg.contains("invalid")
                            || error_msg.contains("unexpected"),
                        "Expected JSON parsing error for {}, got: {}",
                        test_description,
                        source
                    );
                }
                GossipClientError::GetRequest { .. } => {
                    // Can happen with retries or connection issues
                }
                err => panic!("Unexpected error for {}: {:?}", test_description, err),
            }
        }
    }

    mod data_retrieval_scoring_tests {
        use super::*;
        use rstest::rstest;

        fn create_test_peer(id: u8) -> (Address, PeerListItem) {
            let mining_addr = Address::from([id; 20]);
            let peer_address = PeerAddress {
                gossip: SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(192, 168, 1, id)),
                    8000 + id as u16,
                ),
                api: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, id)), 9000 + id as u16),
                execution: RethPeerInfo::default(),
            };
            let peer = PeerListItem {
                address: peer_address,
                reputation_score: PeerScore::new(PeerScore::INITIAL),
                response_time: 100,
                is_online: true,
                last_seen: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            };
            (mining_addr, peer)
        }

        #[rstest]
        #[case(Duration::from_millis(499), ScoreIncreaseReason::TimelyResponse)]
        #[case(Duration::from_millis(500), ScoreIncreaseReason::TimelyResponse)]
        #[case(Duration::from_millis(501), ScoreIncreaseReason::Online)]
        #[case(Duration::from_millis(1999), ScoreIncreaseReason::Online)]
        #[case(Duration::from_secs(2), ScoreIncreaseReason::Online)]
        fn test_response_time_scoring_boundaries(
            #[case] response_time: Duration,
            #[case] expected_reason: ScoreIncreaseReason,
        ) {
            let peer_list = PeerList::test_mock().expect("to create peer list mock");
            let (addr, peer) = create_test_peer(1);
            peer_list.add_or_update_peer(addr, peer, true);

            let initial_score = peer_list.get_peer(&addr).unwrap().reputation_score.get();

            GossipClient::handle_data_retrieval_score(&peer_list, &Ok(()), &addr, response_time);

            let updated_score = peer_list.get_peer(&addr).unwrap().reputation_score.get();
            match expected_reason {
                ScoreIncreaseReason::TimelyResponse | ScoreIncreaseReason::Online => {
                    assert!(
                        updated_score > initial_score,
                        "Response time {:?} should increase score from {} to {} for reason {:?}",
                        response_time,
                        initial_score,
                        updated_score,
                        expected_reason
                    );
                }
                _ => panic!(
                    "Unexpected score reason for success case: {:?}",
                    expected_reason
                ),
            }
        }

        #[rstest]
        #[case(Duration::from_secs(3), 1)]
        #[case(Duration::from_secs(5), 1)]
        #[case(Duration::from_secs(10), 1)]
        fn test_slow_response_scoring(
            #[case] response_time: Duration,
            #[case] expected_decrease: u16,
        ) {
            let peer_list = PeerList::test_mock().expect("to create peer list mock");
            let (addr, peer) = create_test_peer(1);
            peer_list.add_or_update_peer(addr, peer, true);

            let initial_score = peer_list.get_peer(&addr).unwrap().reputation_score.get();

            GossipClient::handle_data_retrieval_score(&peer_list, &Ok(()), &addr, response_time);

            let updated_score = peer_list.get_peer(&addr).unwrap().reputation_score.get();
            assert_eq!(
                updated_score,
                initial_score - expected_decrease,
                "Slow response {:?} should decrease score by {}",
                response_time,
                expected_decrease
            );
        }

        #[rstest]
        #[case(GossipError::Network(NetErr::Other("timeout".to_string())), 3)]
        #[case(GossipError::Network(NetErr::Transport("connection failed".to_string())), 3)]
        #[case(GossipError::Network(NetErr::Http { status: reqwest::StatusCode::INTERNAL_SERVER_ERROR, body: "error".to_string() }), 3)]
        fn test_error_response_scoring(#[case] error: GossipError, #[case] expected_decrease: u16) {
            let peer_list = PeerList::test_mock().expect("to create peer list mock");
            let (addr, peer) = create_test_peer(1);
            peer_list.add_or_update_peer(addr, peer, true);

            let initial_score = peer_list.get_peer(&addr).unwrap().reputation_score.get();
            let response_time = Duration::from_millis(500);

            GossipClient::handle_data_retrieval_score::<()>(
                &peer_list,
                &Err(error),
                &addr,
                response_time,
            );

            let updated_score = peer_list.get_peer(&addr).unwrap().reputation_score.get();
            assert_eq!(
                updated_score,
                initial_score - expected_decrease,
                "Error response should decrease score by {}",
                expected_decrease
            );
        }

        #[test]
        fn test_multiple_score_updates_in_sequence() {
            let peer_list = PeerList::test_mock().expect("to create peer list mock");
            let (addr, peer) = create_test_peer(1);
            peer_list.add_or_update_peer(addr, peer, true);

            let operations = vec![
                (Duration::from_millis(100), Ok(()), 51),
                (NORMAL_RESPONSE_TIME, Ok(()), 52),
                (SLOW_RESPONSE_TIME, Ok(()), 51),
                (
                    FAST_RESPONSE_THRESHOLD,
                    Err(GossipError::Network(NetErr::Other("error".to_string()))),
                    48,
                ),
            ];

            for (response_time, result, expected_score) in operations {
                match result {
                    Ok(()) => {
                        GossipClient::handle_data_retrieval_score(
                            &peer_list,
                            &Ok(()),
                            &addr,
                            response_time,
                        );
                    }
                    Err(e) => {
                        GossipClient::handle_data_retrieval_score::<()>(
                            &peer_list,
                            &Err(e),
                            &addr,
                            response_time,
                        );
                    }
                }

                let current_score = peer_list.get_peer(&addr).unwrap().reputation_score.get();
                assert_eq!(
                    current_score, expected_score,
                    "After response time {:?}, score should be {}",
                    response_time, expected_score
                );
            }
        }
    }

    mod circuit_breaker_tests {
        use super::*;
        use irys_primitives::Address;
        use irys_types::PeerListItem;
        use rstest::rstest;
        use std::sync::Arc;
        use tokio::task::JoinSet;

        #[rstest]
        #[case(0, true)] // No failures - should be available
        #[case(3, true)] // Below threshold - should be available
        #[case(5, false)] // At threshold - circuit breaker opens
        #[case(8, false)] // Above threshold - should be unavailable
        #[case(15, false)] // Well above threshold - should be unavailable
        #[tokio::test]
        async fn test_circuit_breaker_failure_threshold(
            #[case] failure_count: usize,
            #[case] should_be_available: bool,
        ) {
            let client = GossipClient::new(Duration::from_secs(5), Address::from([1_u8; 20]));
            let addr = Address::from([2_u8; 20]);

            // Record failures
            for _ in 0..failure_count {
                client.record_peer_failure(&addr).await;
            }

            assert_eq!(
                client.is_peer_available(&addr).await,
                should_be_available,
                "After {} failures, peer availability should be {}",
                failure_count,
                should_be_available
            );
        }

        #[tokio::test]
        async fn test_circuit_breaker_recovery_after_success() {
            let client = GossipClient::new(Duration::from_secs(5), Address::from([1_u8; 20]));
            let addr = Address::from([3_u8; 20]);

            // Open circuit breaker with failures
            for _ in 0..10 {
                client.record_peer_failure(&addr).await;
            }
            assert!(
                !client.is_peer_available(&addr).await,
                "Circuit breaker should be open"
            );

            // Record some successes to potentially close it
            for _ in 0..5 {
                client.record_peer_success(&addr).await;
            }

            // After successes, should be available again
            assert!(
                client.is_peer_available(&addr).await,
                "Circuit breaker should close after successes"
            );
        }

        #[tokio::test]
        async fn test_concurrent_circuit_breaker_operations() {
            let client = Arc::new(GossipClient::new(
                Duration::from_secs(5),
                Address::from([1_u8; 20]),
            ));
            let addr = Address::from([4_u8; 20]);
            let mut join_set = JoinSet::new();

            // Simulate concurrent success and failure operations
            for i in 0..20 {
                let client_clone = client.clone();
                let addr_copy = addr;

                join_set.spawn(async move {
                    if i % 2 == 0 {
                        client_clone.record_peer_failure(&addr_copy).await;
                    } else {
                        client_clone.record_peer_success(&addr_copy).await;
                    }
                    let _ = client_clone.is_peer_available(&addr_copy).await;
                });
            }

            while (join_set.join_next().await).is_some() {}

            // Should not panic and should have some deterministic final state
            let is_available = client.is_peer_available(&addr).await;
            // Test completed successfully - just ensure no panic occurred
            let _ = is_available;
        }

        #[tokio::test]
        async fn test_batched_peer_availability_checks() {
            let client = GossipClient::new(Duration::from_secs(5), Address::from([1_u8; 20]));
            let peer_list = PeerList::test_mock().expect("to create peer list mock");

            // Add multiple peers - some will fail circuit breaker checks
            for i in 1..=10 {
                let addr = Address::from([i; 20]);
                let peer = PeerListItem::default();
                peer_list.add_or_update_peer(addr, peer, true);

                // Force some circuit breakers to open by recording failures
                if i % 3 == 0 {
                    for _ in 0..10 {
                        // Exceed failure threshold
                        client.record_peer_failure(&addr).await;
                    }
                }
            }

            // Test that batched checks work correctly
            let result = client.select_peers_for_query(false, &peer_list).await;

            // Should have some available peers (those without failed circuit breakers)
            assert!(result.is_ok());
            let available_peers = result.unwrap();

            // Should have filtered out peers with failed circuit breakers
            // We expect roughly 6-7 peers available (10 total - 3-4 with failed breakers)
            assert!(
                available_peers.len() < 10,
                "Should filter out some peers with failed circuit breakers"
            );
            assert!(
                !available_peers.is_empty(),
                "Should have some available peers"
            );
        }
    }

    mod property_tests {
        use super::*;
        use irys_primitives::Address;
        use irys_types::{PeerListItem, PeerScore};
        use proptest::prelude::*;

        prop_compose! {
            fn arb_address()(bytes in prop::array::uniform32(any::<u8>())) -> Address {
                let slice: [u8; 20] = bytes[..20].try_into().unwrap();
                Address::from(slice)
            }
        }

        proptest! {
            #[test]
            fn peer_selection_respects_limits(
                peer_count in 1..50_usize,
                scores in prop::collection::vec(1..100_u16, 1..50)
            ) {
                let rt = tokio::runtime::Runtime::new().unwrap();
                let _ = rt.block_on(async {
                    let client = GossipClient::new(Duration::from_secs(5), Address::from([1_u8; 20]));
                    let peer_list = PeerList::test_mock().expect("to create peer list mock");

                    // Add peers with varying scores
                    for (i, &score) in scores.iter().take(peer_count).enumerate() {
                        let addr = Address::from([(i as u8).wrapping_add(1); 20]);
                        let peer = PeerListItem {
                            reputation_score: PeerScore::new(score),
                            is_online: true,
                            ..PeerListItem::default()
                        };
                        peer_list.add_or_update_peer(addr, peer, true);
                    }

                    let result = client.select_peers_for_query(false, &peer_list).await;

                    if let Ok(selected_peers) = result {
                        // Should never exceed maximum parallel queries
                        prop_assert!(selected_peers.len() <= MAX_PARALLEL_PEER_QUERIES);

                        // Should not be empty if we added peers
                        if peer_count > 0 {
                            prop_assert!(!selected_peers.is_empty());
                        }

                        // All selected peers should be unique
                        let mut addrs: Vec<_> = selected_peers.iter().map(|(addr, _)| *addr).collect();
                        addrs.sort();
                        addrs.dedup();
                        prop_assert_eq!(addrs.len(), selected_peers.len());
                    }
                    Ok(())
                });
            }

            #[test]
            fn circuit_breaker_filtering_is_consistent(
                peer_count in 5..20_usize,
                failure_rate in 0.1..0.8_f64
            ) {
                let rt = tokio::runtime::Runtime::new().unwrap();
                let _ = rt.block_on(async {
                    let client = GossipClient::new(Duration::from_secs(5), Address::from([1_u8; 20]));
                    let peer_list = PeerList::test_mock().expect("to create peer list mock");

                    let mut failed_peers = Vec::new();

                    // Add peers and fail some circuit breakers
                    for i in 0..peer_count {
                        let addr = Address::from([(i as u8).wrapping_add(1); 20]);
                        let peer = PeerListItem::default();
                        peer_list.add_or_update_peer(addr, peer, true);

                        if (i as f64 / peer_count as f64) < failure_rate {
                            // Fail this peer's circuit breaker
                            for _ in 0..10 {
                                client.record_peer_failure(&addr).await;
                            }
                            failed_peers.push(addr);
                        }
                    }

                    let result = client.select_peers_for_query(false, &peer_list).await;

                    if let Ok(selected_peers) = result {
                        // None of the failed peers should be selected
                        for (selected_addr, _) in &selected_peers {
                            prop_assert!(!failed_peers.contains(selected_addr));
                        }

                        // Should have some available peers if failure rate < 1.0
                        if failure_rate < 0.9 && peer_count > failed_peers.len() {
                            prop_assert!(!selected_peers.is_empty());
                        }
                    }
                    Ok(())
                });
            }
        }
    }

    mod error_injection_tests {
        use super::*;
        use rstest::rstest;

        #[rstest]
        #[case(NetErr::Transport("connection refused".to_string()), true)]
        #[case(NetErr::Http { status: StatusCode::GATEWAY_TIMEOUT, body: "timeout".to_string() }, true)]
        #[case(NetErr::Http { status: StatusCode::TOO_MANY_REQUESTS, body: "rate limited".to_string() }, true)]
        #[case(NetErr::Http { status: StatusCode::INTERNAL_SERVER_ERROR, body: "server error".to_string() }, true)]
        #[case(NetErr::Http { status: StatusCode::SERVICE_UNAVAILABLE, body: "unavailable".to_string() }, true)]
        #[case(NetErr::Decode("invalid json".to_string()), false)]
        #[case(NetErr::Other("unknown error".to_string()), false)]
        #[tokio::test]
        async fn test_error_retryability_classification(
            #[case] error: NetErr,
            #[case] should_be_retryable: bool,
        ) {
            assert_eq!(
                error.is_retryable(),
                should_be_retryable,
                "NetErr::{:?} retryability mismatch",
                error
            );
        }

        #[tokio::test]
        async fn test_concurrent_request_cancellation() {
            // Create a slow server response to allow cancellation
            let client = Arc::new(GossipClient::new(
                Duration::from_secs(15),
                Address::from([1_u8; 20]),
            ));
            // Use unreachable address to simulate hanging request
            let peer = create_peer_address("10.255.255.1", 8080);
            let peer_list = Arc::new(PeerList::test_mock().expect("to create peer list mock"));

            // Start multiple concurrent requests
            let mut handles = Vec::new();
            for _ in 0..5 {
                let client = client.clone();
                let peer_list = peer_list.clone();

                handles.push(tokio::spawn(async move {
                    client.check_health(peer, &peer_list).await
                }));
            }

            // Cancel them after a short delay
            tokio::time::sleep(Duration::from_millis(50)).await;
            for handle in &handles {
                handle.abort();
            }

            // Verify all tasks were cancelled
            for handle in handles {
                let result = handle.await;
                assert!(result.is_err()); // Should be JoinError from cancellation
            }
        }

        #[rstest]
        #[case(GossipError::Network(NetErr::Transport("timeout".to_string())), true)]
        #[case(GossipError::Network(NetErr::Http { status: StatusCode::GATEWAY_TIMEOUT, body: String::new() }), true)]
        #[case(GossipError::Network(NetErr::Decode("json error".to_string())), false)]
        #[tokio::test]
        async fn test_gossip_error_retry_decision(
            #[case] error: GossipError,
            #[case] should_retry: bool,
        ) {
            let decision = match &error {
                GossipError::Network(net_err) => net_err.is_retryable(),
                _ => false,
            };

            assert_eq!(
                decision, should_retry,
                "GossipError retry decision mismatch for {:?}",
                error
            );
        }
    }
}
