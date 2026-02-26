#![allow(
    clippy::module_name_repetitions,
    reason = "I have no idea how to name this module to satisfy this lint"
)]
use crate::metrics::record_gossip_outbound_error;
use crate::types::{GossipError, GossipResponse, GossipResult, GossipRoutes, RejectionReason};
use crate::GossipCache;
use core::time::Duration;
use futures::StreamExt as _;
use irys_domain::{PeerList, ScoreDecreaseReason, ScoreIncreaseReason};
use irys_types::v2::{GossipDataRequestV2, GossipDataV2};
use irys_types::{
    BlockBody, BlockHash, BlockIndexItem, BlockIndexQuery, GossipCacheKey, HandshakeRequest,
    HandshakeRequestV2, HandshakeResponseV1, HandshakeResponseV2, IrysAddress, IrysBlockHeader,
    IrysPeerId, IrysTransactionResponse, NodeInfo, PeerAddress, PeerListItem, PeerNetworkError,
    PeerResponse, ProtocolVersion, SealedBlock, DATA_REQUEST_RETRIES, H256,
};
use irys_utils::circuit_breaker::{CircuitBreakerConfig, CircuitBreakerManager};
use opentelemetry::propagation::Injector;
use rand::prelude::SliceRandom as _;
use reqwest::{Client, StatusCode};
use reth::primitives::Block;
use reth::revm::primitives::B256;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{debug, error, instrument, warn, Instrument as _};

/// Maximum number of protocol versions a peer can advertise to prevent DDoS attacks
const MAX_PROTOCOL_VERSIONS: usize = 20;

/// Builds the base gossip URL for a given peer address and protocol version,
/// e.g. `http://<addr>/gossip` for V1 or `http://<addr>/gossip/v2` for V2.
fn gossip_base_url(addr: &SocketAddr, version: ProtocolVersion) -> String {
    match version {
        ProtocolVersion::V1 => format!("http://{}/gossip", addr),
        ProtocolVersion::V2 => format!("http://{}/gossip/v2", addr),
    }
}

struct HeaderInjector<'a>(&'a mut reqwest::header::HeaderMap);

impl Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let Ok(name) = reqwest::header::HeaderName::from_bytes(key.as_bytes()) {
            if let Ok(val) = reqwest::header::HeaderValue::from_str(&value) {
                self.0.insert(name, val);
            }
        }
    }
}

fn inject_trace_context(headers: &mut reqwest::header::HeaderMap) {
    let cx = tracing_opentelemetry::OpenTelemetrySpanExt::context(&tracing::Span::current());
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&cx, &mut HeaderInjector(headers));
    });
}

fn traced_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    inject_trace_context(&mut headers);
    headers
}

/// Response time threshold for fast responses (deserving extra reward)
const FAST_RESPONSE_THRESHOLD: Duration = Duration::from_millis(500);

/// Response time threshold for normal responses (standard reward)
const NORMAL_RESPONSE_THRESHOLD: Duration = Duration::from_secs(2);

/// Timeout to wait for handshake completion before retrying
const HANDSHAKE_WAIT_TIMEOUT: Duration = Duration::from_millis(1000);

/// Control flow outcome for `handle_peer_pull_response`.
#[derive(Debug)]
enum PeerPullOutcome<T> {
    Ok((IrysPeerId, T)),
    Err(PeerNetworkError),
    RetryAfterHandshake,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum GossipClientError {
    #[error("Get request to {0} failed with reason {1}")]
    GetRequest(String, String),
    #[error("Health check to {0} failed with status code {1}")]
    HealthCheck(String, reqwest::StatusCode),
    #[error("Failed to get json response payload from {0} with reason {1}")]
    GetJsonResponsePayload(String, String),
    #[error("Circuit breaker open for peer {0}")]
    CircuitBreakerOpen(IrysPeerId),
}

#[derive(Debug, Clone)]
pub struct GossipClient {
    pub mining_address: IrysAddress,
    pub peer_id: IrysPeerId,
    client: Client,
    circuit_breaker: CircuitBreakerManager<IrysPeerId>,
}

fn gossip_error_type(err: &GossipError) -> &'static str {
    match err {
        GossipError::Network(_) => "network",
        GossipError::InvalidPeer(_) => "invalid_peer",
        GossipError::Cache(_) => "cache",
        GossipError::Internal(_) => "internal",
        GossipError::InvalidData(_) => "invalid_data",
        GossipError::BlockPool(_) => "block_pool",
        GossipError::TransactionIsAlreadyHandled => "already_handled",
        GossipError::CommitmentValidation(_) => "commitment_validation",
        GossipError::PeerNetwork(_) => "peer_network",
        GossipError::RateLimited => "rate_limited",
        GossipError::Advisory(_) => "advisory",
        GossipError::CircuitBreakerOpen(_) => "circuit_breaker_open",
    }
}

impl GossipClient {
    pub const CURRENT_PROTOCOL_VERSION: u32 = ProtocolVersion::current() as u32;

    #[must_use]
    pub fn new(timeout: Duration, mining_address: IrysAddress, peer_id: IrysPeerId) -> Self {
        Self::with_circuit_breaker_config(
            timeout,
            mining_address,
            peer_id,
            CircuitBreakerConfig::p2p_defaults(),
        )
    }

    #[must_use]
    pub fn with_circuit_breaker_config(
        timeout: Duration,
        mining_address: IrysAddress,
        peer_id: IrysPeerId,
        circuit_config: CircuitBreakerConfig,
    ) -> Self {
        let circuit_breaker = CircuitBreakerManager::new(circuit_config);

        Self {
            mining_address,
            peer_id,
            client: Client::builder()
                .timeout(timeout)
                .build()
                .expect("Failed to create reqwest client"),
            circuit_breaker,
        }
    }

    pub fn internal_client(&self) -> &Client {
        &self.client
    }

    /// Get circuit breaker metrics for monitoring
    pub fn circuit_breaker_metrics(
        &self,
    ) -> irys_utils::circuit_breaker::CircuitBreakerMetrics<IrysPeerId> {
        self.circuit_breaker.metrics()
    }

    /// Cleanup stale circuit breakers that haven't been accessed recently
    pub fn cleanup_stale_circuit_breakers(&self) {
        self.circuit_breaker.cleanup_stale();
    }

    fn check_circuit_breaker(&self, peer: &IrysPeerId) -> GossipResult<()> {
        if self.circuit_breaker.is_available(peer) {
            return Ok(());
        }
        tracing::debug!(?peer, "circuit breaker open, skipping request");
        Err(GossipError::CircuitBreakerOpen(*peer))
    }

    /// Send data to a peer and update their score based on the result
    ///
    /// # Errors
    ///
    /// If the peer is offline or the request fails, an error is returned.
    async fn send_data_and_update_score_internal(
        &self,
        peer: (&IrysPeerId, &PeerListItem),
        data: &GossipDataV2,
        peer_list: &PeerList,
    ) -> GossipResult<()> {
        let peer_id = peer.0;
        let peer = peer.1;

        self.check_circuit_breaker(peer_id)?;

        let res = self.send_data(peer, data).await;
        Self::handle_score(peer_list, &res, peer_id, &self.circuit_breaker);
        res.map(|_| ())
    }

    /// Request a specific data to be gossiped. Returns true if the peer has the data,
    /// and false if it doesn't.
    pub async fn make_get_data_request_and_update_the_score(
        &self,
        peer: &(IrysPeerId, PeerListItem),
        requested_data: GossipDataRequestV2,
        peer_list: &PeerList,
    ) -> GossipResult<GossipResponse<bool>> {
        self.check_circuit_breaker(&peer.0)?;

        let start_time = std::time::Instant::now();

        let res = if peer.1.protocol_version == irys_types::ProtocolVersion::V1 {
            if let GossipDataRequestV2::BlockBody(_) = requested_data {
                return Ok(GossipResponse::Rejected(
                    RejectionReason::UnsupportedFeature,
                ));
            }

            if let Some(req_v1) = requested_data.to_v1() {
                self.send_data_internal(
                    &peer.1.address.gossip,
                    GossipRoutes::GetData,
                    &req_v1,
                    ProtocolVersion::V1,
                )
                .await
            } else {
                Ok(GossipResponse::Rejected(
                    RejectionReason::UnsupportedFeature,
                ))
            }
        } else {
            self.send_data_internal(
                &peer.1.address.gossip,
                GossipRoutes::GetData,
                &requested_data,
                ProtocolVersion::V2,
            )
            .await
        };

        let response_time = start_time.elapsed();
        Self::handle_data_retrieval_score(
            peer_list,
            &res,
            &peer.0,
            response_time,
            &self.circuit_breaker,
        );
        res
    }

    async fn pull_block_body_from_v1_peer(
        &self,
        peer: &(IrysPeerId, PeerListItem),
        header: &IrysBlockHeader,
        peer_list: &PeerList,
    ) -> GossipResult<GossipResponse<Option<GossipDataV2>>> {
        let commitment_tx_ids = header.commitment_tx_ids();
        let data_tx_ids_map = header.get_data_ledger_tx_ids();
        let data_tx_ids: Vec<H256> = data_tx_ids_map.values().flatten().copied().collect();

        let mut commitment_transactions = Vec::new();
        let mut data_transactions = Vec::new();

        // Pull commitment transactions
        for &tx_id in commitment_tx_ids {
            match self
                .pull_transaction_from_peer(tx_id, peer, peer_list)
                .await
            {
                Ok((_, IrysTransactionResponse::Commitment(tx))) => {
                    commitment_transactions.push(tx)
                }
                Ok((_, IrysTransactionResponse::Storage(_))) => {
                    return Ok(GossipResponse::Rejected(RejectionReason::InvalidData))
                }
                Err(err) => {
                    debug!("Failed to pull commitment tx {}: {:?}", tx_id, err);
                    return Ok(GossipResponse::Accepted(None));
                }
            }
        }

        // Pull data transactions
        for tx_id in data_tx_ids {
            match self
                .pull_transaction_from_peer(tx_id, peer, peer_list)
                .await
            {
                Ok((_, IrysTransactionResponse::Storage(tx))) => data_transactions.push(tx),
                Ok((_, IrysTransactionResponse::Commitment(_))) => {
                    return Ok(GossipResponse::Rejected(RejectionReason::InvalidData))
                }
                Err(err) => {
                    debug!("Failed to pull data tx {}: {:?}", tx_id, err);
                    return Ok(GossipResponse::Accepted(None));
                }
            }
        }

        let block_body = BlockBody {
            block_hash: header.block_hash,
            data_transactions,
            commitment_transactions,
        };

        Ok(GossipResponse::Accepted(Some(GossipDataV2::BlockBody(
            Arc::new(block_body),
        ))))
    }

    /// Request specific data from the peer. Returns the data right away if the peer has it
    /// and updates the peer's score based on the result.
    async fn pull_primitive_data_and_update_the_score(
        &self,
        peer: &(IrysPeerId, PeerListItem),
        requested_data: GossipDataRequestV2,
        peer_list: &PeerList,
    ) -> GossipResult<GossipResponse<Option<GossipDataV2>>> {
        self.check_circuit_breaker(&peer.0)?;
        let start_time = std::time::Instant::now();

        let res: GossipResult<GossipResponse<Option<GossipDataV2>>> =
            if peer.1.protocol_version == irys_types::ProtocolVersion::V1 {
                // BlockBody does not exist in V1 - reject early for safety
                if matches!(requested_data, GossipDataRequestV2::BlockBody(_)) {
                    return Ok(GossipResponse::Rejected(
                        RejectionReason::UnsupportedFeature,
                    ));
                }
                if let Some(req_v1) = requested_data.to_v1() {
                    let res_v1: GossipResult<GossipResponse<Option<irys_types::v1::GossipDataV1>>> =
                        self.send_data_internal(
                            &peer.1.address.gossip,
                            GossipRoutes::PullData,
                            &req_v1,
                            ProtocolVersion::V1,
                        )
                        .await;

                    res_v1.map(|response| match response {
                        GossipResponse::Accepted(maybe_data) => {
                            GossipResponse::Accepted(maybe_data.map(GossipDataV2::from))
                        }
                        GossipResponse::Rejected(reason) => GossipResponse::Rejected(reason),
                    })
                } else {
                    Ok(GossipResponse::Rejected(
                        RejectionReason::UnsupportedFeature,
                    ))
                }
            } else {
                self.send_data_internal(
                    &peer.1.address.gossip,
                    GossipRoutes::PullData,
                    &requested_data,
                    ProtocolVersion::V2,
                )
                .await
            };

        let response_time = start_time.elapsed();
        Self::handle_data_retrieval_score(
            peer_list,
            &res,
            &peer.0,
            response_time,
            &self.circuit_breaker,
        );

        res
    }

    /// Request specific data from the peer. Returns the data right away if the peer has it
    /// and updates the peer's score based on the result.
    async fn pull_data_and_update_the_score(
        &self,
        peer: &(IrysPeerId, PeerListItem),
        requested_data: GossipDataRequestV2,
        fallback_header: Option<&IrysBlockHeader>,
        peer_list: &PeerList,
    ) -> GossipResult<GossipResponse<Option<GossipDataV2>>> {
        self.check_circuit_breaker(&peer.0)?;

        if peer.1.protocol_version == irys_types::ProtocolVersion::V1 {
            if let GossipDataRequestV2::BlockBody(_block_hash) = requested_data {
                let header = fallback_header.expect(
                    "BlockBody request must have fallback header for v1 peer compatibility",
                );
                return self
                    .pull_block_body_from_v1_peer(peer, header, peer_list)
                    .await;
            }
        }
        self.pull_primitive_data_and_update_the_score(peer, requested_data, peer_list)
            .await
    }

    pub async fn get_info(&self, peer: PeerAddress) -> Result<NodeInfo, GossipClientError> {
        let url = format!(
            "{}{}",
            gossip_base_url(&peer.gossip, ProtocolVersion::V1),
            GossipRoutes::Info
        );
        let headers = traced_headers();
        let response = self
            .internal_client()
            .get(&url)
            .headers(headers)
            .send()
            .await;

        let gossip_result = match response {
            Ok(resp) => {
                if !resp.status().is_success() {
                    Err(GossipClientError::GetRequest(
                        peer.gossip.to_string(),
                        resp.status().to_string(),
                    ))
                } else {
                    match resp.json::<GossipResponse<NodeInfo>>().await {
                        Ok(GossipResponse::Accepted(info)) => Ok(info),
                        Ok(GossipResponse::Rejected(reason)) => Err(GossipClientError::GetRequest(
                            peer.gossip.to_string(),
                            format!("Request rejected: {:?}", reason),
                        )),
                        Err(e) => Err(GossipClientError::GetJsonResponsePayload(
                            peer.gossip.to_string(),
                            e.to_string(),
                        )),
                    }
                }
            }
            Err(e) => Err(GossipClientError::GetRequest(
                peer.gossip.to_string(),
                e.to_string(),
            )),
        };

        gossip_result
    }

    pub async fn get_peer_list(
        &self,
        peer: SocketAddr,
    ) -> Result<Vec<PeerAddress>, GossipClientError> {
        let url = format!(
            "{}{}",
            gossip_base_url(&peer, ProtocolVersion::V1),
            GossipRoutes::PeerList
        );
        let headers = traced_headers();
        let response = self
            .internal_client()
            .get(&url)
            .headers(headers)
            .send()
            .await
            .map_err(|error| GossipClientError::GetRequest(peer.to_string(), error.to_string()))?;

        if !response.status().is_success() {
            return Err(GossipClientError::GetRequest(
                peer.to_string(),
                response.status().to_string(),
            ));
        }

        let response: GossipResponse<Vec<PeerAddress>> =
            response.json().await.map_err(|error| {
                GossipClientError::GetJsonResponsePayload(peer.to_string(), error.to_string())
            })?;

        match response {
            GossipResponse::Accepted(peers) => Ok(peers),
            GossipResponse::Rejected(reason) => Err(GossipClientError::GetRequest(
                peer.to_string(),
                format!("Request rejected: {:?}", reason),
            )),
        }
    }

    pub async fn post_handshake_v1(
        &self,
        peer: SocketAddr,
        version: HandshakeRequest,
    ) -> Result<PeerResponse, GossipClientError> {
        // V1 uses /version endpoint
        let url = format!(
            "{}{}",
            gossip_base_url(&peer, ProtocolVersion::V1),
            GossipRoutes::Version
        );
        debug!("Posting V1 handshake to {}: {:?}", url, version);
        let headers = traced_headers();
        let response = self
            .internal_client()
            .post(&url)
            .headers(headers)
            .json(&version)
            .send()
            .await
            .map_err(|error| GossipClientError::GetRequest(peer.to_string(), error.to_string()))?;

        if !response.status().is_success() {
            return Err(GossipClientError::GetRequest(
                peer.to_string(),
                response.status().to_string(),
            ));
        }

        let response: GossipResponse<HandshakeResponseV1> =
            response.json().await.map_err(|error| {
                GossipClientError::GetJsonResponsePayload(peer.to_string(), error.to_string())
            })?;

        match response {
            GossipResponse::Accepted(v1_response) => {
                // Convert V1 response to V2 format for internal compatibility
                let v2_response = HandshakeResponseV2 {
                    version: v1_response.version,
                    protocol_version: v1_response.protocol_version,
                    peers: v1_response.peers,
                    timestamp: v1_response.timestamp,
                    message: v1_response.message,
                    consensus_config_hash: H256::zero(), // V1 doesn't have consensus hash
                };
                Ok(PeerResponse::Accepted(v2_response))
            }
            GossipResponse::Rejected(reason) => match reason {
                RejectionReason::InvalidCredentials => Ok(PeerResponse::Rejected(
                    irys_types::version::RejectedResponse {
                        reason: irys_types::version::RejectionReason::InvalidCredentials,
                        message: Some("Invalid credentials provided".to_string()),
                        retry_after: None,
                    },
                )),
                RejectionReason::ProtocolMismatch => Ok(PeerResponse::Rejected(
                    irys_types::version::RejectedResponse {
                        reason: irys_types::version::RejectionReason::ProtocolMismatch,
                        message: Some("Protocol mismatch".to_string()),
                        retry_after: None,
                    },
                )),
                _ => Err(GossipClientError::GetRequest(
                    peer.to_string(),
                    format!("Unexpected rejection reason: {:?}", reason),
                )),
            },
        }
    }

    pub async fn post_handshake_v2(
        &self,
        peer: SocketAddr,
        version: HandshakeRequestV2,
    ) -> Result<PeerResponse, GossipClientError> {
        // V2 uses /v2/handshake endpoint
        let url = format!(
            "{}{}",
            gossip_base_url(&peer, ProtocolVersion::V2),
            GossipRoutes::Handshake
        );
        debug!("Posting V2 handshake to {}: {:?}", url, version);
        let headers = traced_headers();
        let response = self
            .internal_client()
            .post(&url)
            .headers(headers)
            .json(&version)
            .send()
            .await
            .map_err(|error| GossipClientError::GetRequest(peer.to_string(), error.to_string()))?;

        if !response.status().is_success() {
            return Err(GossipClientError::GetRequest(
                peer.to_string(),
                response.status().to_string(),
            ));
        }

        let response: GossipResponse<HandshakeResponseV2> =
            response.json().await.map_err(|error| {
                GossipClientError::GetJsonResponsePayload(peer.to_string(), error.to_string())
            })?;

        match response {
            GossipResponse::Accepted(version_response) => {
                Ok(PeerResponse::Accepted(version_response))
            }
            GossipResponse::Rejected(reason) => match reason {
                RejectionReason::InvalidCredentials => Ok(PeerResponse::Rejected(
                    irys_types::version::RejectedResponse {
                        reason: irys_types::version::RejectionReason::InvalidCredentials,
                        message: Some("Invalid credentials provided".to_string()),
                        retry_after: None,
                    },
                )),
                RejectionReason::ProtocolMismatch => Ok(PeerResponse::Rejected(
                    irys_types::version::RejectedResponse {
                        reason: irys_types::version::RejectionReason::ProtocolMismatch,
                        message: Some("Protocol mismatch".to_string()),
                        retry_after: None,
                    },
                )),
                _ => Err(GossipClientError::GetRequest(
                    peer.to_string(),
                    format!("Unexpected rejection reason: {:?}", reason),
                )),
            },
        }
    }

    pub async fn get_block_index(
        &self,
        peer: PeerAddress,
        query: BlockIndexQuery,
    ) -> Result<Vec<BlockIndexItem>, GossipClientError> {
        let url = format!(
            "{}{}",
            gossip_base_url(&peer.gossip, ProtocolVersion::V1),
            GossipRoutes::BlockIndex
        );
        let headers = traced_headers();
        let response = self
            .internal_client()
            .get(&url)
            .headers(headers)
            .query(&query)
            .send()
            .await;

        let gossip_result = match response {
            Ok(resp) => {
                if !resp.status().is_success() {
                    Err(GossipClientError::GetRequest(
                        peer.gossip.to_string(),
                        resp.status().to_string(),
                    ))
                } else {
                    match resp.json::<GossipResponse<Vec<BlockIndexItem>>>().await {
                        Ok(GossipResponse::Accepted(index)) => Ok(index),
                        Ok(GossipResponse::Rejected(reason)) => Err(GossipClientError::GetRequest(
                            peer.gossip.to_string(),
                            format!("Request rejected: {:?}", reason),
                        )),
                        Err(e) => Err(GossipClientError::GetJsonResponsePayload(
                            peer.gossip.to_string(),
                            e.to_string(),
                        )),
                    }
                }
            }
            Err(e) => Err(GossipClientError::GetRequest(
                peer.gossip.to_string(),
                e.to_string(),
            )),
        };

        gossip_result
    }

    pub async fn get_protocol_versions(
        &self,
        peer: PeerAddress,
    ) -> Result<Vec<u32>, GossipClientError> {
        let url = format!(
            "http://{}/gossip{}",
            peer.gossip,
            GossipRoutes::ProtocolVersion
        );
        let headers = traced_headers();
        let response = self
            .internal_client()
            .get(&url)
            .headers(headers)
            .send()
            .await
            .map_err(|error| {
                GossipClientError::GetRequest(peer.gossip.to_string(), error.to_string())
            })?;

        if !response.status().is_success() {
            // Very old V1 peers don't have the protocol versions endpoint.
            // If we get a non-500 error (e.g., 404 Not Found), assume it's an old V1 peer.
            if !response.status().is_server_error() {
                debug!(
                    "Peer {} doesn't have protocol versions endpoint, assuming V1",
                    peer.gossip
                );
                return Ok(vec![1]);
            }

            return Err(GossipClientError::GetRequest(
                peer.gossip.to_string(),
                response.status().to_string(),
            ));
        }

        let versions: Vec<u32> = response.json().await.map_err(|error| {
            GossipClientError::GetJsonResponsePayload(peer.gossip.to_string(), error.to_string())
        })?;

        // Reject peers advertising too many versions to prevent DDoS attacks
        if versions.len() > MAX_PROTOCOL_VERSIONS {
            return Err(GossipClientError::GetRequest(
                peer.gossip.to_string(),
                format!(
                    "Peer returned {} protocol versions, exceeding maximum of {}",
                    versions.len(),
                    MAX_PROTOCOL_VERSIONS
                ),
            ));
        }

        Ok(versions)
    }

    #[instrument(level = "trace", skip(self, peer_list), fields(%peer_id))]
    pub async fn check_health(
        &self,
        peer_id: &IrysPeerId,
        peer: PeerAddress,
        protocol_version: ProtocolVersion,
        peer_list: &PeerList,
    ) -> Result<bool, GossipClientError> {
        if !self.circuit_breaker.is_available(peer_id) {
            tracing::debug!(?peer_id, "circuit breaker open, skipping health check");
            return Err(GossipClientError::CircuitBreakerOpen(*peer_id));
        }

        let url = format!(
            "{}{}",
            gossip_base_url(&peer.gossip, protocol_version),
            GossipRoutes::Health
        );
        let peer_addr_str = peer.gossip.to_string();

        let headers = traced_headers();
        let response = match self
            .internal_client()
            .get(&url)
            .headers(headers)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(error) => {
                tracing::debug!(
                    peer = ?peer_id,
                    error = %error,
                    "health check request failed, recording circuit breaker failure"
                );
                self.circuit_breaker.record_failure(peer_id);
                return Err(GossipClientError::GetRequest(
                    peer_addr_str,
                    error.to_string(),
                ));
            }
        };

        if !response.status().is_success() {
            self.circuit_breaker.record_failure(peer_id);
            return Err(GossipClientError::HealthCheck(
                peer_addr_str,
                response.status(),
            ));
        }

        let response: GossipResponse<bool> = match response.json().await {
            Ok(resp) => resp,
            Err(error) => {
                self.circuit_breaker.record_failure(peer_id);
                return Err(GossipClientError::GetJsonResponsePayload(
                    peer_addr_str,
                    error.to_string(),
                ));
            }
        };

        match response {
            GossipResponse::Accepted(val) => {
                if val {
                    self.circuit_breaker.record_success(peer_id);
                } else {
                    self.circuit_breaker.record_failure(peer_id);
                }
                Ok(val)
            }
            GossipResponse::Rejected(reason) => {
                warn!("Health check rejected with reason: {:?}", reason);
                self.circuit_breaker.record_failure(peer_id);
                match reason {
                    RejectionReason::HandshakeRequired(reason) => {
                        warn!("Health check requires handshake: {:?}", reason);
                        peer_list.initiate_handshake(peer.api, peer.gossip, true);
                    }
                    RejectionReason::GossipDisabled => {
                        return Ok(false);
                    }
                    RejectionReason::InvalidCredentials | RejectionReason::ProtocolMismatch => {
                        warn!("Health check rejected with reason: {:?}", reason);
                    }
                    _ => {
                        warn!(
                            "Unexpected rejection reason for the health check: {:?}",
                            reason
                        );
                    }
                };
                Ok(true)
            }
        }
    }

    /// Pre-serialize a `GossipDataV2` once into `(GossipRoutes, bytes::Bytes)` for V2 peers.
    /// Returns `None` on serialization failure.
    pub fn pre_serialize_for_broadcast(
        &self,
        data: &GossipDataV2,
    ) -> Option<(GossipRoutes, bytes::Bytes)> {
        let (route, json_result) = match data {
            GossipDataV2::Chunk(chunk) => (
                GossipRoutes::Chunk,
                serde_json::to_vec(&self.create_request_v2(chunk.clone())),
            ),
            GossipDataV2::Transaction(header) => (
                GossipRoutes::Transaction,
                serde_json::to_vec(&self.create_request_v2(header.clone())),
            ),
            GossipDataV2::CommitmentTransaction(tx) => (
                GossipRoutes::CommitmentTx,
                serde_json::to_vec(&self.create_request_v2(tx.clone())),
            ),
            GossipDataV2::BlockHeader(header) => {
                if header.poa.chunk.is_none() {
                    error!(
                        target = "p2p::gossip_client::pre_serialize",
                        block.hash = ?header.block_hash,
                        "Pre-serializing a block header without the POA chunk"
                    );
                }
                (
                    GossipRoutes::Block,
                    serde_json::to_vec(&self.create_request_v2(header.clone())),
                )
            }
            GossipDataV2::BlockBody(body) => (
                GossipRoutes::BlockBody,
                serde_json::to_vec(&self.create_request_v2(body.clone())),
            ),
            GossipDataV2::ExecutionPayload(payload) => (
                GossipRoutes::ExecutionPayload,
                serde_json::to_vec(&self.create_request_v2(payload.clone())),
            ),
            GossipDataV2::IngressProof(proof) => (
                GossipRoutes::IngressProof,
                serde_json::to_vec(&self.create_request_v2(proof.clone())),
            ),
        };
        match json_result {
            Ok(b) => Some((route, bytes::Bytes::from(b))),
            Err(e) => {
                warn!(?route, "Failed to pre-serialize gossip data: {}", e);
                None
            }
        }
    }

    /// Send pre-serialized JSON body to a V2 peer. Skips serialization — posts raw bytes.
    async fn send_preserialized(
        &self,
        gossip_address: &SocketAddr,
        route: GossipRoutes,
        body: bytes::Bytes,
    ) -> GossipResult<GossipResponse<()>> {
        let url = format!(
            "{}{}",
            gossip_base_url(gossip_address, ProtocolVersion::V2),
            route
        );

        debug!("Sending pre-serialized data to {}", url);

        let mut headers = traced_headers();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|response_error| {
                GossipError::Network(format!(
                    "Failed to send data to {}: {}",
                    url, response_error
                ))
            })?;

        let status = response.status();

        match status {
            StatusCode::OK => {
                let text = response.text().await.map_err(|e| {
                    GossipError::Network(format!("Failed to read response from {}: {}", url, e))
                })?;

                if text.trim().is_empty() {
                    return Err(GossipError::Network(format!("Empty response from {}", url)));
                }

                let parsed = serde_json::from_str(&text).map_err(|e| {
                    GossipError::Network(format!(
                        "{}: Failed to parse JSON: {} - Response: {}",
                        url, e, text
                    ))
                })?;
                Ok(parsed)
            }
            _ => {
                let error_text = response.text().await.unwrap_or_default();
                Err(GossipError::Network(format!(
                    "API request {} failed with status: {} - {}",
                    url, status, error_text
                )))
            }
        }
    }

    /// Spawns a detached task that sends pre-serialized data, updates peer score, and records cache seen.
    pub fn send_preserialized_detached(
        &self,
        peer: (&IrysPeerId, &PeerListItem),
        route: GossipRoutes,
        body: bytes::Bytes,
        peer_list: &PeerList,
        cache: Arc<GossipCache>,
        gossip_cache_key: GossipCacheKey,
    ) {
        let client = self.clone();
        let peer_list = peer_list.clone();
        let peer_id = *peer.0;
        let peer = peer.1.clone();
        let span = tracing::Span::current();

        tokio::spawn(
            async move {
                if let Err(e) = client.check_circuit_breaker(&peer_id) {
                    record_gossip_outbound_error(gossip_error_type(&e));
                    return;
                }
                let result = client
                    .send_preserialized(&peer.address.gossip, route, body)
                    .await;
                Self::handle_score(&peer_list, &result, &peer_id, &client.circuit_breaker);
                match result {
                    Ok(_) => {
                        if let Err(err) = cache.record_seen(peer_id, gossip_cache_key) {
                            error!("Error recording seen data in cache: {:?}", err);
                        }
                    }
                    Err(e) => {
                        record_gossip_outbound_error(gossip_error_type(&e));
                        error!("Error sending pre-serialized data to peer: {:?}", e);
                    }
                }
            }
            .instrument(span),
        );
    }

    /// Send data to a peer
    ///
    /// # Errors
    ///
    /// If the peer is offline or the request fails, an error is returned.
    async fn send_data(
        &self,
        peer: &PeerListItem,
        data: &GossipDataV2,
    ) -> GossipResult<GossipResponse<()>> {
        if peer.protocol_version == irys_types::ProtocolVersion::V1 {
            if let GossipDataV2::BlockBody(_) = data {
                return Ok(GossipResponse::Rejected(
                    RejectionReason::UnsupportedFeature,
                ));
            }

            if let Some(data_v1) = data.to_v1() {
                return self.send_data_v1(peer, &data_v1).await;
            }

            return Ok(GossipResponse::Rejected(
                RejectionReason::UnsupportedFeature,
            ));
        }

        match data {
            GossipDataV2::Chunk(unpacked_chunk) => {
                self.send_data_internal(
                    &peer.address.gossip,
                    GossipRoutes::Chunk,
                    unpacked_chunk,
                    ProtocolVersion::V2,
                )
                .await
            }
            GossipDataV2::Transaction(irys_transaction_header) => {
                self.send_data_internal(
                    &peer.address.gossip,
                    GossipRoutes::Transaction,
                    irys_transaction_header,
                    ProtocolVersion::V2,
                )
                .await
            }
            GossipDataV2::CommitmentTransaction(commitment_tx) => {
                self.send_data_internal(
                    &peer.address.gossip,
                    GossipRoutes::CommitmentTx,
                    commitment_tx,
                    ProtocolVersion::V2,
                )
                .await
            }
            GossipDataV2::BlockHeader(irys_block_header) => {
                if irys_block_header.poa.chunk.is_none() {
                    error!(
                        target = "p2p::gossip_client::send_data",
                        block.hash = ?irys_block_header.block_hash,
                        "Sending a block header without the POA chunk"
                    );
                }
                self.send_data_internal(
                    &peer.address.gossip,
                    GossipRoutes::Block,
                    &irys_block_header,
                    ProtocolVersion::V2,
                )
                .await
            }
            GossipDataV2::BlockBody(block_body) => {
                self.send_data_internal(
                    &peer.address.gossip,
                    GossipRoutes::BlockBody,
                    &block_body,
                    ProtocolVersion::V2,
                )
                .await
            }
            GossipDataV2::ExecutionPayload(execution_payload) => {
                self.send_data_internal(
                    &peer.address.gossip,
                    GossipRoutes::ExecutionPayload,
                    &execution_payload,
                    ProtocolVersion::V2,
                )
                .await
            }
            GossipDataV2::IngressProof(ingress_proof) => {
                self.send_data_internal(
                    &peer.address.gossip,
                    GossipRoutes::IngressProof,
                    ingress_proof,
                    ProtocolVersion::V2,
                )
                .await
            }
        }
    }

    async fn send_data_v1(
        &self,
        peer: &PeerListItem,
        data: &irys_types::v1::GossipDataV1,
    ) -> GossipResult<GossipResponse<()>> {
        use irys_types::v1::GossipDataV1;
        match data {
            GossipDataV1::Chunk(unpacked_chunk) => {
                self.send_data_internal(
                    &peer.address.gossip,
                    GossipRoutes::Chunk,
                    unpacked_chunk,
                    ProtocolVersion::V1,
                )
                .await
            }
            GossipDataV1::Transaction(irys_transaction_header) => {
                self.send_data_internal(
                    &peer.address.gossip,
                    GossipRoutes::Transaction,
                    irys_transaction_header,
                    ProtocolVersion::V1,
                )
                .await
            }
            GossipDataV1::CommitmentTransaction(commitment_tx) => {
                self.send_data_internal(
                    &peer.address.gossip,
                    GossipRoutes::CommitmentTx,
                    commitment_tx,
                    ProtocolVersion::V1,
                )
                .await
            }
            GossipDataV1::Block(irys_block_header) => {
                if irys_block_header.poa.chunk.is_none() {
                    error!(
                        target = "p2p::gossip_client::send_data",
                        block.hash = ?irys_block_header.block_hash,
                        "Sending a block header without the POA chunk"
                    );
                }
                self.send_data_internal(
                    &peer.address.gossip,
                    GossipRoutes::Block,
                    irys_block_header,
                    ProtocolVersion::V1,
                )
                .await
            }
            GossipDataV1::ExecutionPayload(execution_payload) => {
                self.send_data_internal(
                    &peer.address.gossip,
                    GossipRoutes::ExecutionPayload,
                    execution_payload,
                    ProtocolVersion::V1,
                )
                .await
            }
            GossipDataV1::IngressProof(ingress_proof) => {
                self.send_data_internal(
                    &peer.address.gossip,
                    GossipRoutes::IngressProof,
                    ingress_proof,
                    ProtocolVersion::V1,
                )
                .await
            }
        }
    }

    #[instrument(name = "send_data_internal", skip_all, fields(%route, ?protocol_version))]
    async fn send_data_internal<T, R>(
        &self,
        gossip_address: &SocketAddr,
        route: GossipRoutes,
        data: &T,
        protocol_version: ProtocolVersion,
    ) -> GossipResult<GossipResponse<R>>
    where
        T: Serialize + Clone,
        for<'de> R: Deserialize<'de>,
    {
        // Construct URL based on protocol version
        let url = format!(
            "{}{}",
            gossip_base_url(gossip_address, protocol_version),
            route
        );

        debug!("Sending data to {} using {:?}", url, protocol_version);

        let headers = traced_headers();

        let response = match protocol_version {
            ProtocolVersion::V1 => {
                let req = self.create_request_v1(data.clone());
                self.client
                    .post(&url)
                    .headers(headers)
                    .json(&req)
                    .send()
                    .await
            }
            ProtocolVersion::V2 => {
                let req = self.create_request_v2(data.clone());
                self.client
                    .post(&url)
                    .headers(headers)
                    .json(&req)
                    .send()
                    .await
            }
        };

        let response = response.map_err(|response_error| {
            GossipError::Network(format!(
                "Failed to send data to {}: {}",
                url, response_error
            ))
        })?;

        let status = response.status();

        match status {
            StatusCode::OK => {
                let text = response.text().await.map_err(|e| {
                    GossipError::Network(format!("Failed to read response from {}: {}", url, e))
                })?;

                if text.trim().is_empty() {
                    return Err(GossipError::Network(format!("Empty response from {}", url)));
                }

                let body = serde_json::from_str(&text).map_err(|e| {
                    GossipError::Network(format!(
                        "{}: Failed to parse JSON: {} - Response: {}",
                        url, e, text
                    ))
                })?;
                Ok(body)
            }
            _ => {
                let error_text = response.text().await.unwrap_or_default();
                Err(GossipError::Network(format!(
                    "API request {} failed with status: {} - {}",
                    url, status, error_text
                )))
            }
        }
    }

    fn handle_score<T>(
        peer_list: &PeerList,
        result: &GossipResult<GossipResponse<T>>,
        peer_id: &IrysPeerId,
        circuit_breaker: &CircuitBreakerManager<IrysPeerId>,
    ) {
        match &result {
            Ok(_) => {
                peer_list.increase_peer_score_by_peer_id(peer_id, ScoreIncreaseReason::DataRequest);
                peer_list.set_is_online_by_peer_id(peer_id, true);
                circuit_breaker.record_success(peer_id);
            }
            Err(err) => {
                if let GossipError::Network(_message) = err {
                    debug!(
                        "Setting peer {:?} status to 'offline' due to a network error",
                        peer_id
                    );
                    peer_list.set_is_online_by_peer_id(peer_id, false);
                }
                peer_list.decrease_peer_score_by_peer_id(
                    peer_id,
                    ScoreDecreaseReason::NetworkError(format!("{:?}", err)),
                );
                circuit_breaker.record_failure(peer_id);
            }
        }
    }

    /// Handle scoring for data retrieval operations based on response time and success
    fn handle_data_retrieval_score<T>(
        peer_list: &PeerList,
        result: &GossipResult<T>,
        peer_id: &IrysPeerId,
        response_time: Duration,
        circuit_breaker: &CircuitBreakerManager<IrysPeerId>,
    ) {
        match result {
            Ok(_) => {
                if response_time <= FAST_RESPONSE_THRESHOLD {
                    peer_list.increase_peer_score_by_peer_id(
                        peer_id,
                        ScoreIncreaseReason::TimelyResponse,
                    );
                } else if response_time <= NORMAL_RESPONSE_THRESHOLD {
                    peer_list.increase_peer_score_by_peer_id(peer_id, ScoreIncreaseReason::Online);
                } else {
                    peer_list
                        .decrease_peer_score_by_peer_id(peer_id, ScoreDecreaseReason::SlowResponse);
                }
                circuit_breaker.record_success(peer_id);
            }
            Err(err) => {
                peer_list.decrease_peer_score_by_peer_id(
                    peer_id,
                    ScoreDecreaseReason::NetworkError(format!(
                        "handle_data_retrieval_score resulted in an error: {:?}",
                        err
                    )),
                );
                circuit_breaker.record_failure(peer_id);
            }
        }
    }

    /// Sends data to a peer and update their score in a detached task
    pub fn send_data_and_update_the_score_detached(
        &self,
        peer: (&IrysPeerId, &PeerListItem),
        data: Arc<GossipDataV2>,
        peer_list: &PeerList,
        cache: Arc<GossipCache>,
        gossip_cache_key: GossipCacheKey,
    ) {
        let client = self.clone();
        let peer_list = peer_list.clone();
        let peer_id = *peer.0;
        let peer = peer.1.clone();

        tokio::spawn(async move {
            if let Err(e) = client
                .send_data_and_update_score_internal((&peer_id, &peer), &data, &peer_list)
                .await
            {
                record_gossip_outbound_error(gossip_error_type(&e));
                error!("Error sending data to peer: {:?}", e);
            } else if let Err(err) = cache.record_seen(peer_id, gossip_cache_key) {
                error!("Error recording seen data in cache: {:?}", err);
            }
        });
    }

    /// Sends data to a peer without updating their score
    pub fn send_data_without_score_update(
        &self,
        peer: (&IrysPeerId, &PeerListItem),
        data: Arc<GossipDataV2>,
    ) {
        let client = self.clone();
        let peer = peer.1.clone();

        tokio::spawn(async move {
            if let Err(e) = client.send_data(&peer, &data).await {
                record_gossip_outbound_error(gossip_error_type(&e));
                error!("Error sending data to peer: {}", e);
            }
        });
    }

    /// Sends data to a peer and updates their score specifically for data requests
    pub fn send_data_and_update_score_for_request(
        &self,
        peer: (&IrysPeerId, &PeerListItem),
        data: Arc<GossipDataV2>,
        peer_list: &PeerList,
    ) {
        let client = self.clone();
        let peer_list = peer_list.clone();
        let peer_id = *peer.0;
        let peer = peer.1.clone();

        tokio::spawn(async move {
            let result = client.send_data(&peer, &data).await;
            match &result {
                Ok(_) => {
                    peer_list
                        .increase_peer_score_by_peer_id(&peer_id, ScoreIncreaseReason::DataRequest);
                }
                Err(err) => {
                    record_gossip_outbound_error(gossip_error_type(err));
                    peer_list.decrease_peer_score_by_peer_id(
                        &peer_id,
                        ScoreDecreaseReason::Offline(format!(
                            "send_data_and_update_score_for_request resulted in an error: {:?}",
                            err
                        )),
                    );
                }
            }
        });
    }

    fn create_request_v1<T>(&self, data: T) -> irys_types::GossipRequestV1<T> {
        irys_types::GossipRequestV1 {
            miner_address: self.mining_address,
            data,
        }
    }

    fn create_request_v2<T>(&self, data: T) -> irys_types::GossipRequestV2<T> {
        irys_types::GossipRequestV2 {
            peer_id: self.peer_id,
            miner_address: self.mining_address,
            data,
        }
    }

    pub async fn pull_block_header_from_network(
        &self,
        block_hash: BlockHash,
        use_trusted_peers_only: bool,
        peer_list: &PeerList,
    ) -> Result<(IrysPeerId, Arc<IrysBlockHeader>), PeerNetworkError> {
        let data_request = GossipDataRequestV2::BlockHeader(block_hash);
        self.pull_data_from_network(
            data_request,
            None,
            use_trusted_peers_only,
            peer_list,
            Self::block,
        )
        .await
    }

    pub async fn pull_block_body_from_network(
        &self,
        header: Arc<IrysBlockHeader>,
        use_trusted_peers_only: bool,
        peer_list: &PeerList,
    ) -> Result<(IrysPeerId, SealedBlock), PeerNetworkError> {
        let data_request = GossipDataRequestV2::BlockBody(header.block_hash);
        self.pull_data_from_network(
            data_request,
            Some(&header),
            use_trusted_peers_only,
            peer_list,
            |gossip_data| match gossip_data {
                GossipDataV2::BlockBody(body) => {
                    SealedBlock::new(Arc::clone(&header), Arc::unwrap_or_clone(body)).map_err(|e| {
                        PeerNetworkError::UnexpectedData(format!("Invalid block body: {e:?}"))
                    })
                }
                _ => Err(PeerNetworkError::UnexpectedData(format!(
                    "Expected BlockBody, got {:?}",
                    gossip_data.data_type_and_id()
                ))),
            },
        )
        .await
    }

    /// Pull a block body from a specific peer, updating its score accordingly.
    pub async fn pull_block_body_from_peer(
        &self,
        header: &IrysBlockHeader,
        peer: &(IrysPeerId, PeerListItem),
        peer_list: &PeerList,
    ) -> Result<(IrysPeerId, SealedBlock), PeerNetworkError> {
        let data_request = GossipDataRequestV2::BlockBody(header.block_hash);
        let (peer_id, body) = self
            .pull_data_from_peer_with_retry(
                data_request,
                Some(header),
                peer,
                peer_list,
                Self::block_body,
            )
            .await?;

        let sealed = SealedBlock::new(header.clone(), Arc::unwrap_or_clone(body)).map_err(|e| {
            PeerNetworkError::InvalidBlockBody {
                peer_id,
                reason: format!("{e:?}"),
            }
        })?;
        Ok((peer_id, sealed))
    }

    pub async fn pull_payload_from_network(
        &self,
        evm_payload_hash: B256,
        use_trusted_peers_only: bool,
        peer_list: &PeerList,
    ) -> Result<(IrysPeerId, Block), PeerNetworkError> {
        let data_request = GossipDataRequestV2::ExecutionPayload(evm_payload_hash);
        self.pull_data_from_network(
            data_request,
            None,
            use_trusted_peers_only,
            peer_list,
            Self::execution_payload,
        )
        .await
    }

    pub async fn pull_transaction_from_network(
        &self,
        tx_id: H256,
        use_trusted_peers_only: bool,
        peer_list: &PeerList,
    ) -> Result<(IrysPeerId, IrysTransactionResponse), PeerNetworkError> {
        let data_request = GossipDataRequestV2::Transaction(tx_id);
        self.pull_data_from_network(
            data_request,
            None,
            use_trusted_peers_only,
            peer_list,
            Self::transaction,
        )
        .await
    }

    /// Pull a block from a specific peer, updating its score accordingly.
    pub async fn pull_block_header_from_peer(
        &self,
        block_hash: BlockHash,
        peer: &(IrysPeerId, PeerListItem),
        peer_list: &PeerList,
    ) -> Result<(IrysPeerId, Arc<IrysBlockHeader>), PeerNetworkError> {
        let data_request = GossipDataRequestV2::BlockHeader(block_hash);
        self.pull_data_from_peer_with_retry(data_request, None, peer, peer_list, Self::block)
            .await
    }

    pub async fn pull_transaction_from_peer(
        &self,
        tx_id: H256,
        peer: &(IrysPeerId, PeerListItem),
        peer_list: &PeerList,
    ) -> Result<(IrysPeerId, IrysTransactionResponse), PeerNetworkError> {
        let data_request = GossipDataRequestV2::Transaction(tx_id);
        // Use primitive fetch to avoid recursion: pull_data_and_update_the_score
        // can call pull_block_body_from_v1_peer which calls pull_transaction_from_peer.
        self.pull_primitive_data_from_peer_with_retry(
            data_request,
            peer,
            peer_list,
            Self::transaction,
        )
        .await
    }

    fn block(gossip_data: GossipDataV2) -> Result<Arc<IrysBlockHeader>, PeerNetworkError> {
        match gossip_data {
            GossipDataV2::BlockHeader(block) => Ok(block),
            _ => Err(PeerNetworkError::UnexpectedData(format!(
                "Expected IrysBlockHeader, got {:?}",
                gossip_data.data_type_and_id()
            ))),
        }
    }

    fn execution_payload(gossip_data: GossipDataV2) -> Result<Block, PeerNetworkError> {
        match gossip_data {
            GossipDataV2::ExecutionPayload(block) => Ok(block),
            _ => Err(PeerNetworkError::UnexpectedData(format!(
                "Expected ExecutionPayload, got {:?}",
                gossip_data.data_type_and_id()
            ))),
        }
    }

    fn transaction(gossip_data: GossipDataV2) -> Result<IrysTransactionResponse, PeerNetworkError> {
        match gossip_data {
            GossipDataV2::Transaction(tx) => Ok(IrysTransactionResponse::Storage(tx)),
            GossipDataV2::CommitmentTransaction(tx) => Ok(IrysTransactionResponse::Commitment(tx)),
            _ => Err(PeerNetworkError::UnexpectedData(format!(
                "Expected Transaction or CommitmentTransaction, got {:?}",
                gossip_data.data_type_and_id()
            ))),
        }
    }

    fn block_body(gossip_data: GossipDataV2) -> Result<Arc<BlockBody>, PeerNetworkError> {
        match gossip_data {
            GossipDataV2::BlockBody(body) => Ok(body),
            _ => Err(PeerNetworkError::UnexpectedData(format!(
                "Expected BlockBody, got {:?}",
                gossip_data.data_type_and_id()
            ))),
        }
    }

    /// Pull data from a specific peer with a single handshake retry.
    ///
    /// Runs the provided `fetch` closure up to twice. On the first attempt,
    /// if the peer responds with `HandshakeRequired`, a handshake is initiated
    /// and the request is retried once.
    async fn pull_with_handshake_retry<T, F, Fut>(
        &self,
        data_request: GossipDataRequestV2,
        peer: &(IrysPeerId, PeerListItem),
        peer_list: &PeerList,
        map_data: fn(GossipDataV2) -> Result<T, PeerNetworkError>,
        fetch: F,
    ) -> Result<(IrysPeerId, T), PeerNetworkError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = GossipResult<GossipResponse<Option<GossipDataV2>>>>,
    {
        for attempt in 0..2 {
            let result = fetch().await;
            match Self::handle_peer_pull_response(
                result,
                &data_request,
                peer,
                peer_list,
                map_data,
                attempt,
            )
            .await
            {
                PeerPullOutcome::Ok(val) => return Ok(val),
                PeerPullOutcome::Err(e) => return Err(e),
                PeerPullOutcome::RetryAfterHandshake => continue,
            }
        }
        unreachable!("loop always returns: attempt 1 never yields RetryAfterHandshake")
    }

    /// Pull data using `pull_data_and_update_the_score` with handshake retry.
    ///
    /// Includes circuit breaker checks and V1 BlockBody compatibility handling.
    /// Used by `pull_block_body_from_peer` and `pull_block_header_from_peer`.
    async fn pull_data_from_peer_with_retry<T>(
        &self,
        data_request: GossipDataRequestV2,
        fallback_header: Option<&IrysBlockHeader>,
        peer: &(IrysPeerId, PeerListItem),
        peer_list: &PeerList,
        map_data: fn(GossipDataV2) -> Result<T, PeerNetworkError>,
    ) -> Result<(IrysPeerId, T), PeerNetworkError> {
        self.pull_with_handshake_retry(data_request.clone(), peer, peer_list, map_data, || {
            self.pull_data_and_update_the_score(
                peer,
                data_request.clone(),
                fallback_header,
                peer_list,
            )
        })
        .await
    }

    /// Like `pull_data_from_peer_with_retry` but uses
    /// `pull_primitive_data_and_update_the_score` to avoid the recursive cycle
    /// through `pull_block_body_from_v1_peer` → `pull_transaction_from_peer`.
    /// Used by `pull_transaction_from_peer`.
    async fn pull_primitive_data_from_peer_with_retry<T>(
        &self,
        data_request: GossipDataRequestV2,
        peer: &(IrysPeerId, PeerListItem),
        peer_list: &PeerList,
        map_data: fn(GossipDataV2) -> Result<T, PeerNetworkError>,
    ) -> Result<(IrysPeerId, T), PeerNetworkError> {
        self.pull_with_handshake_retry(data_request.clone(), peer, peer_list, map_data, || {
            self.pull_primitive_data_and_update_the_score(peer, data_request.clone(), peer_list)
        })
        .await
    }

    /// Shared response handling for single-peer pull retries.
    async fn handle_peer_pull_response<T>(
        result: GossipResult<GossipResponse<Option<GossipDataV2>>>,
        data_request: &GossipDataRequestV2,
        peer: &(IrysPeerId, PeerListItem),
        peer_list: &PeerList,
        map_data: fn(GossipDataV2) -> Result<T, PeerNetworkError>,
        attempt: u32,
    ) -> PeerPullOutcome<T> {
        match result {
            Ok(response) => match response {
                GossipResponse::Accepted(Some(data)) => match map_data(data) {
                    Ok(mapped) => PeerPullOutcome::Ok((peer.0, mapped)),
                    Err(e) => PeerPullOutcome::Err(e),
                },
                GossipResponse::Accepted(None) => {
                    PeerPullOutcome::Err(PeerNetworkError::FailedToRequestData(format!(
                        "Peer {} did not have the requested {:?}",
                        peer.0, data_request
                    )))
                }
                GossipResponse::Rejected(reason) => {
                    debug!(
                        "Peer {:?} rejected {:?} request: {:?}",
                        peer.0, data_request, reason
                    );
                    match reason {
                        RejectionReason::HandshakeRequired(handshake_reason) => {
                            warn!(
                                "Request {:?} requires handshake: {:?}",
                                data_request, handshake_reason
                            );
                            peer_list.initiate_handshake(
                                peer.1.address.api,
                                peer.1.address.gossip,
                                true,
                            );
                            if attempt == 0 {
                                debug!("Waiting for handshake to complete...");
                                tokio::time::sleep(HANDSHAKE_WAIT_TIMEOUT).await;
                                return PeerPullOutcome::RetryAfterHandshake;
                            }
                        }
                        RejectionReason::GossipDisabled => {
                            peer_list.set_is_online_by_peer_id(&peer.0, false);
                        }
                        RejectionReason::InvalidCredentials | RejectionReason::ProtocolMismatch => {
                            warn!(
                                "Peer {:?} rejected {:?} with {:?}",
                                peer.0, data_request, reason
                            );
                        }
                        _ => {}
                    }
                    PeerPullOutcome::Err(PeerNetworkError::FailedToRequestData(format!(
                        "Peer {:?} rejected {:?} request: {:?}",
                        peer.0, data_request, reason
                    )))
                }
            },
            Err(err) => match err {
                GossipError::PeerNetwork(e) => PeerPullOutcome::Err(e),
                other => {
                    PeerPullOutcome::Err(PeerNetworkError::FailedToRequestData(other.to_string()))
                }
            },
        }
    }

    pub async fn pull_data_from_network<T>(
        &self,
        data_request: GossipDataRequestV2,
        fallback_header: Option<&IrysBlockHeader>,
        use_trusted_peers_only: bool,
        peer_list: &PeerList,
        map_data: impl Fn(GossipDataV2) -> Result<T, PeerNetworkError>,
    ) -> Result<(IrysPeerId, T), PeerNetworkError> {
        let mut peers = if use_trusted_peers_only {
            peer_list.online_trusted_peers()
        } else {
            // Get the top 10 most active peers
            peer_list.top_active_peers(Some(10), None)
        };

        // Shuffle peers to randomize the selection
        peers.shuffle(&mut rand::thread_rng());
        // Take random 5
        peers.truncate(5);

        if peers.is_empty() {
            return Err(PeerNetworkError::NoPeersAvailable);
        }

        // Try up to DATA_REQUEST_RETRIES rounds across peers; only retry peers on transient errors.
        let mut last_error = None;

        // Track peers eligible for retry across rounds (transient failures only)
        let mut retryable_peers = peers.clone();

        // Track if all failures were due to handshake requirements
        let mut all_failures_were_handshake = true;
        let mut had_any_attempts = false;

        for attempt in 1..=DATA_REQUEST_RETRIES {
            // If no peers remain to try, stop early
            if retryable_peers.is_empty() {
                break;
            }

            // Fan-out concurrently to all retryable peers in this round and accept first success

            let current_round = retryable_peers.clone();
            let mut futs = futures::stream::FuturesUnordered::new();

            for peer in current_round {
                let gc = self.clone();
                let dr = data_request.clone();
                let pl = peer_list;
                let fh = fallback_header;
                futs.push(async move {
                    let peer_id = peer.0;
                    let res = gc.pull_data_and_update_the_score(&peer, dr, fh, pl).await;
                    (peer_id, peer, res)
                });
            }

            let mut next_retryable = Vec::new();

            while let Some((peer_id, peer, result)) = futs.next().await {
                had_any_attempts = true;
                match result {
                    Ok(GossipResponse::Accepted(maybe_data)) => {
                        match maybe_data {
                            Some(data) => match map_data(data) {
                                Ok(data) => {
                                    debug!(
                                        "Successfully pulled {:?} from peer {}",
                                        data_request, peer_id
                                    );
                                    // Drop remaining futures to cancel outstanding requests
                                    return Ok((peer_id, data));
                                }
                                Err(err) => {
                                    warn!("Failed to map data from peer {}: {}", peer_id, err);
                                    peer_list.decrease_peer_score_by_peer_id(
                                        &peer_id,
                                        ScoreDecreaseReason::BogusData(format!("{err}")),
                                    );
                                    last_error = Some(GossipError::from(err));
                                    // Not retriable: don't include this peer for future rounds
                                    all_failures_were_handshake = false;
                                }
                            },
                            None => {
                                // Peer doesn't have this data; keep for future rounds to allow re-gossip
                                debug!("Peer {} doesn't have {:?}", peer_id, data_request);
                                next_retryable.push(peer);
                                all_failures_were_handshake = false;
                            }
                        }
                    }
                    Ok(GossipResponse::Rejected(reason)) => {
                        warn!(
                            "Peer {} rejected the request: {:?}: {:?}",
                            peer_id, data_request, reason
                        );
                        match reason {
                            RejectionReason::HandshakeRequired(reason) => {
                                warn!(
                                    "Data request {:?} requires handshake: {:?}",
                                    data_request, reason
                                );
                                peer_list.initiate_handshake(
                                    peer.1.address.api,
                                    peer.1.address.gossip,
                                    true,
                                );
                                last_error = Some(GossipError::from(
                                    PeerNetworkError::FailedToRequestData(format!(
                                        "Request {:?}: Peer {:?} requires a handshake",
                                        data_request, peer_id
                                    )),
                                ));
                            }
                            RejectionReason::GossipDisabled => {
                                peer_list.set_is_online_by_peer_id(&peer.0, false);
                                last_error = Some(GossipError::from(
                                    PeerNetworkError::FailedToRequestData(format!(
                                        "Request {:?}: Peer {:?} has gossip disabled",
                                        data_request, peer_id
                                    )),
                                ));
                                all_failures_were_handshake = false;
                            }
                            RejectionReason::InvalidData => {
                                last_error = Some(GossipError::from(
                                    PeerNetworkError::FailedToRequestData(format!(
                                        "Request {:?}: Peer {:?} reported invalid data",
                                        data_request, peer_id
                                    )),
                                ));
                                all_failures_were_handshake = false;
                            }
                            RejectionReason::RateLimited => {
                                last_error = Some(GossipError::from(
                                    PeerNetworkError::FailedToRequestData(format!(
                                        "Request {:?}: Peer {:?} rate limited the request",
                                        data_request, peer_id
                                    )),
                                ));
                                all_failures_were_handshake = false;
                            }
                            RejectionReason::UnableToVerifyOrigin => {
                                last_error = Some(GossipError::from(
                                    PeerNetworkError::FailedToRequestData(format!(
                                        "Request {:?}: Peer {:?} unable to verify our origin",
                                        data_request, peer_id
                                    )),
                                ));
                                all_failures_were_handshake = false;
                            }
                            RejectionReason::InvalidCredentials
                            | RejectionReason::ProtocolMismatch => {
                                last_error = Some(GossipError::from(
                                    PeerNetworkError::FailedToRequestData(format!(
                                        "Request {:?}: Peer {:?} rejected with {:?}",
                                        data_request, peer_id, reason
                                    )),
                                ));
                                all_failures_were_handshake = false;
                            }
                            RejectionReason::UnsupportedProtocolVersion(unsupported_version) => {
                                last_error = Some(GossipError::from(
                                    PeerNetworkError::FailedToRequestData(format!(
                                        "Request {:?}: Peer {:?} doesn't support protocol version {:?}",
                                        data_request, peer_id, unsupported_version
                                    )),
                                ));
                                all_failures_were_handshake = false;
                            }
                            RejectionReason::UnsupportedFeature => {
                                last_error = Some(GossipError::from(
                                    PeerNetworkError::FailedToRequestData(format!(
                                        "Request {:?}: Peer {:?} doesn't support the requested feature",
                                        data_request, peer_id
                                    )),
                                ));
                                all_failures_were_handshake = false;
                            }
                        }
                        // Do not retry the same peer on rejection
                    }
                    Err(err) => {
                        last_error = Some(err);
                        warn!(
                            "Failed to fetch {:?} from peer {:?} (attempt {}/{}): {}",
                            data_request,
                            peer_id,
                            attempt,
                            DATA_REQUEST_RETRIES,
                            last_error.as_ref().unwrap()
                        );
                        // Transient failure: keep peer for next round
                        next_retryable.push(peer);
                        all_failures_were_handshake = false;
                    }
                }
            }

            retryable_peers = next_retryable;

            // minimal delay between attempts, skip after final iteration
            if attempt != DATA_REQUEST_RETRIES {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        // If all failures were due to handshake requirements, wait for handshake and retry once
        if had_any_attempts && all_failures_were_handshake && !peers.is_empty() {
            debug!(
                "All attempts failed with HandshakeRequired for {:?}. Waiting {}s for handshake completion...",
                data_request,
                HANDSHAKE_WAIT_TIMEOUT.as_secs()
            );

            tokio::time::sleep(HANDSHAKE_WAIT_TIMEOUT).await;

            // Retry with original peer set one more time
            let mut retry_futs = futures::stream::FuturesUnordered::new();
            for peer in &peers {
                let gc = self.clone();
                let dr = data_request.clone();
                let pl = peer_list;
                let fh = fallback_header;
                retry_futs.push(async move {
                    let peer_id = peer.0;
                    let res = gc.pull_data_and_update_the_score(peer, dr, fh, pl).await;
                    (peer_id, res)
                });
            }

            while let Some((peer_id, result)) = retry_futs.next().await {
                if let Ok(GossipResponse::Accepted(Some(data))) = result {
                    match map_data(data) {
                        Ok(data) => {
                            debug!(
                                "Successfully retrieved {:?} from peer {} after handshake wait",
                                data_request, peer_id
                            );
                            return Ok((peer_id, data));
                        }
                        Err(err) => {
                            warn!(
                                "Failed to map data from peer {} after handshake wait: {}",
                                peer_id, err
                            );
                            peer_list.decrease_peer_score_by_peer_id(
                                &peer_id,
                                ScoreDecreaseReason::BogusData(format!("{err}")),
                            );
                            last_error = Some(GossipError::from(err));
                        }
                    }
                }
            }
        }

        Err(PeerNetworkError::FailedToRequestData(format!(
            "Failed to pull {:?} after trying 5 peers: {:?}",
            data_request, last_error
        )))
    }

    pub async fn hydrate_peers_online_status(&self, peer_list: &PeerList) {
        debug!("Hydrating peers online status");
        let peers = peer_list.all_peers_sorted_by_score();
        for peer in peers {
            match self
                .check_health(&peer.0, peer.1.address, peer.1.protocol_version, peer_list)
                .await
            {
                Ok(is_healthy) => {
                    debug!("Peer {} is healthy: {}", peer.0, is_healthy);
                    peer_list.set_is_online_by_peer_id(&peer.0, is_healthy);
                }
                Err(GossipClientError::CircuitBreakerOpen(peer_id)) => {
                    debug!(
                        ?peer_id,
                        "Circuit breaker open, skipping online status update"
                    );
                }
                Err(err) => {
                    warn!(
                        "Failed to check the health of peer {}: {}, setting offline status",
                        peer.0, err
                    );
                    peer_list.set_is_online_by_peer_id(&peer.0, false);
                }
            }
        }
    }

    #[instrument(level = "debug", skip_all)]
    pub async fn stake_and_pledge_whitelist(
        &self,
        peer_list: &PeerList,
    ) -> Result<Vec<IrysAddress>, PeerNetworkError> {
        // Work only with trusted peers
        let mut peers = peer_list.online_trusted_peers();
        peers.shuffle(&mut rand::thread_rng());

        if peers.is_empty() {
            warn!("The node has no trusted peers to fetch stake_and_pledge_whitelist from");
            return Err(PeerNetworkError::NoPeersAvailable);
        }

        // Retry strategy similar to other network pulls: up to 5 attempts across trusted peers
        let mut last_error: Option<GossipError> = None;
        for attempt in 1..=5 {
            let mut round_failures_were_handshake = true;
            let mut round_had_attempts = false;

            for peer in &peers {
                round_had_attempts = true;
                debug!(
                    "Attempting to fetch stake_and_pledge_whitelist from peer {} (attempt {}/5)",
                    peer.0, attempt
                );
                let url = format!(
                    "{}{}",
                    gossip_base_url(&peer.1.address.gossip, peer.1.protocol_version),
                    GossipRoutes::StakeAndPledgeWhitelist
                );

                let headers = traced_headers();
                let response = self
                    .client
                    .get(&url)
                    .headers(headers)
                    .send()
                    .await
                    .map_err(|response_error| {
                        PeerNetworkError::FailedToRequestData(format!(
                            "Failed to get the stake/pledge whitelist {}: {:?}",
                            url, response_error
                        ))
                    })?;

                let status = response.status();

                let res: GossipResult<GossipResponse<Vec<IrysAddress>>> = match status {
                    StatusCode::OK => {
                        let text = response.text().await.map_err(|e| {
                            PeerNetworkError::FailedToRequestData(format!(
                                "Failed to read response from {}: {}",
                                url, e
                            ))
                        })?;

                        if text.trim().is_empty() {
                            return Err(PeerNetworkError::FailedToRequestData(format!(
                                "Empty response from {}",
                                url
                            )));
                        }

                        let gossip_response = serde_json::from_str(&text).map_err(|e| {
                            PeerNetworkError::FailedToRequestData(format!(
                                "{}: Failed to parse JSON: {} - Response: {}",
                                url, e, text
                            ))
                        })?;
                        Ok(gossip_response)
                    }
                    _ => {
                        let error_text = response.text().await.unwrap_or_default();
                        Err(PeerNetworkError::FailedToRequestData(format!(
                            "API request {} failed with status: {} - {}",
                            url, status, error_text
                        ))
                        .into())
                    }
                };

                // Update score for the peer based on the result
                Self::handle_score(peer_list, &res, &peer.0, &self.circuit_breaker);

                match res {
                    Ok(response) => match response {
                        GossipResponse::Accepted(addresses) => return Ok(addresses),
                        GossipResponse::Rejected(reason) => match reason {
                            RejectionReason::HandshakeRequired(reason) => {
                                warn!(
                                    "Stake and pledge whitelist request requires handshake: {:?}",
                                    reason
                                );
                                last_error =
                                    Some(GossipError::from(PeerNetworkError::FailedToRequestData(
                                        format!("{}: Peer {:?} requires a handshake", url, peer.0),
                                    )));
                                peer_list.initiate_handshake(
                                    peer.1.address.api,
                                    peer.1.address.gossip,
                                    true,
                                );
                            }
                            RejectionReason::GossipDisabled => {
                                last_error =
                                    Some(GossipError::from(PeerNetworkError::FailedToRequestData(
                                        format!("{}: Peer {:?} has gossip disabled", url, peer.0),
                                    )));
                                peer_list.set_is_online_by_peer_id(&peer.0, false);
                                round_failures_were_handshake = false;
                            }
                            RejectionReason::InvalidData => {
                                last_error =
                                    Some(GossipError::from(PeerNetworkError::FailedToRequestData(
                                        format!("{}: Peer {:?} reported invalid data", url, peer.0),
                                    )));
                                round_failures_were_handshake = false;
                            }
                            RejectionReason::RateLimited => {
                                last_error =
                                    Some(GossipError::from(PeerNetworkError::FailedToRequestData(
                                        format!("{}: Peer {:?} rate limited the request when updating the stake and pledge list", url, peer.0),
                                    )));
                                round_failures_were_handshake = false;
                            }
                            RejectionReason::UnableToVerifyOrigin => {
                                last_error =
                                    Some(GossipError::from(PeerNetworkError::FailedToRequestData(
                                        format!(
                                            "{}: Peer {:?} unable to verify our origin when updating the stake and pledge list",
                                            url, peer.0
                                        ),
                                    )));
                                round_failures_were_handshake = false;
                            }
                            RejectionReason::InvalidCredentials
                            | RejectionReason::ProtocolMismatch => {
                                last_error = Some(GossipError::from(
                                    PeerNetworkError::FailedToRequestData(format!(
                                        "{}: Peer {:?} rejected with {:?}",
                                        url, peer.0, reason
                                    )),
                                ));
                                round_failures_were_handshake = false;
                            }
                            RejectionReason::UnsupportedProtocolVersion(unsupported_version) => {
                                last_error = Some(GossipError::from(
                                    PeerNetworkError::FailedToRequestData(format!(
                                        "{}: Peer {:?} doesn't support protocol version {:?}",
                                        url, peer.0, unsupported_version
                                    )),
                                ));
                                round_failures_were_handshake = false;
                            }
                            RejectionReason::UnsupportedFeature => {
                                last_error = Some(GossipError::from(
                                    PeerNetworkError::FailedToRequestData(format!(
                                        "{}: Peer {:?} doesn't support the requested feature",
                                        url, peer.0
                                    )),
                                ));
                                round_failures_were_handshake = false;
                            }
                        },
                    },
                    Err(err) => {
                        last_error = Some(err);
                        round_failures_were_handshake = false;
                        continue;
                    }
                }
            }

            if round_had_attempts && round_failures_were_handshake {
                debug!(
                    "All attempts in round {} failed with HandshakeRequired. Waiting...",
                    attempt
                );
                tokio::time::sleep(HANDSHAKE_WAIT_TIMEOUT).await;
            } else {
                // Small backoff before retrying the whole set again
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        // Map the last error into a PeerNetworkError
        Err(match last_error {
            Some(GossipError::PeerNetwork(e)) => e,
            Some(other) => PeerNetworkError::FailedToRequestData(other.to_string()),
            None => PeerNetworkError::FailedToRequestData(
                "Failed to fetch stake and pledge whitelist".to_string(),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::time::Duration;
    use reqwest::StatusCode;
    use std::io::prelude::*;
    use std::net::TcpListener;
    use std::thread;

    // Test fixtures and utilities
    struct TestFixture {
        client: GossipClient,
    }

    impl TestFixture {
        fn new() -> Self {
            Self {
                client: GossipClient::with_circuit_breaker_config(
                    Duration::from_secs(1),
                    IrysAddress::from([1_u8; 20]),
                    IrysPeerId::from([1_u8; 20]),
                    CircuitBreakerConfig::testing(),
                ),
            }
        }

        fn with_timeout(timeout: Duration) -> Self {
            Self {
                client: GossipClient::with_circuit_breaker_config(
                    timeout,
                    IrysAddress::from([1_u8; 20]),
                    IrysPeerId::from([1_u8; 20]),
                    CircuitBreakerConfig::testing(),
                ),
            }
        }
    }

    fn get_free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .expect("Failed to bind to port")
            .local_addr()
            .expect("Failed to get local addr")
            .port()
    }

    fn create_peer_address(host: &str, port: u16) -> PeerAddress {
        PeerAddress {
            gossip: format!("{}:{}", host, port).parse().expect("Valid address"),
            api: format!("{}:{}", host, port).parse().expect("Valid address"),
            execution: Default::default(),
        }
    }

    // Mock HTTP server for testing
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

                if let Ok((mut stream, _)) = listener.accept() {
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
                }
            });

            std::thread::sleep(Duration::from_millis(50));

            Self {
                port,
                handle: Some(handle),
            }
        }

        fn new_with_delay(status_code: u16, body: &str, content_type: &str, delay_ms: u64) -> Self {
            let port = get_free_port();
            let body = body.to_string();
            let content_type = content_type.to_string();

            let handle = thread::spawn(move || {
                let listener =
                    TcpListener::bind(format!("127.0.0.1:{}", port)).expect("Failed to bind");

                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buffer = [0; 1024];
                    let _ = stream.read(&mut buffer);

                    std::thread::sleep(Duration::from_millis(delay_ms));

                    let response = format!(
                        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n{}",
                        status_code,
                        Self::status_text(status_code),
                        content_type,
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
            });

            std::thread::sleep(Duration::from_millis(50));

            Self {
                port,
                handle: Some(handle),
            }
        }

        fn status_text(code: u16) -> &'static str {
            match code {
                200 => "OK",
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

    mod connection_tests {
        use super::*;

        #[tokio::test]
        async fn test_connection_refused() {
            let fixture = TestFixture::new();
            let unreachable_port = get_free_port();
            let peer = create_peer_address("127.0.0.1", unreachable_port);
            let mock_list = PeerList::test_mock().expect("to create peer list mock");

            let peer_id = IrysPeerId::from(IrysAddress::from([1_u8; 20]));
            let result = fixture
                .client
                .check_health(&peer_id, peer, ProtocolVersion::V1, &mock_list)
                .await;

            assert!(result.is_err());
            match result.unwrap_err() {
                GossipClientError::GetRequest(addr, reason) => {
                    assert_eq!(addr, peer.gossip.to_string());
                    assert!(
                        reason.to_lowercase().contains("connection refused"),
                        "Expected connection refused error, got: {}",
                        reason
                    );
                }
                err => panic!("Expected GetRequest error, got: {:?}", err),
            }
        }

        #[tokio::test]
        async fn test_request_timeout() {
            let fixture = TestFixture::with_timeout(Duration::from_millis(1));
            // Use a non-routable IP address
            let peer = create_peer_address("192.0.2.1", 8080);
            let mock_list = PeerList::test_mock().expect("to create peer list mock");

            let peer_id = IrysPeerId::from(IrysAddress::from([1_u8; 20]));
            let result = fixture
                .client
                .check_health(&peer_id, peer, ProtocolVersion::V1, &mock_list)
                .await;

            assert!(result.is_err());
            match result.unwrap_err() {
                GossipClientError::GetRequest(addr, reason) => {
                    assert_eq!(addr, peer.gossip.to_string());
                    assert!(!reason.is_empty(), "Expected timeout error message");
                }
                err => panic!("Expected GetRequest error for timeout, got: {:?}", err),
            }
        }
    }

    mod health_check_error_tests {
        use super::*;

        async fn test_health_check_error_status(status_code: u16, expected_status: StatusCode) {
            let server = MockHttpServer::new_with_response(status_code, "", "text/plain");
            let fixture = TestFixture::new();
            let peer = create_peer_address("127.0.0.1", server.port());
            let mock_list = PeerList::test_mock().expect("to create peer list mock");

            let peer_id = IrysPeerId::from(IrysAddress::from([1_u8; 20]));
            let result = fixture
                .client
                .check_health(&peer_id, peer, ProtocolVersion::V1, &mock_list)
                .await;

            assert!(result.is_err());
            match result.unwrap_err() {
                GossipClientError::HealthCheck(addr, status) => {
                    assert_eq!(addr, peer.gossip.to_string());
                    assert_eq!(status, expected_status);
                }
                err => panic!(
                    "Expected HealthCheck error for status {}, got: {:?}",
                    status_code, err
                ),
            }
        }

        #[tokio::test]
        async fn test_404_not_found() {
            test_health_check_error_status(404, StatusCode::NOT_FOUND).await;
        }

        #[tokio::test]
        async fn test_500_internal_server_error() {
            test_health_check_error_status(500, StatusCode::INTERNAL_SERVER_ERROR).await;
        }
    }

    mod health_check_v2_tests {
        use super::*;

        #[tokio::test]
        async fn test_v2_connection_refused() {
            let fixture = TestFixture::new();
            let unreachable_port = get_free_port();
            let peer = create_peer_address("127.0.0.1", unreachable_port);
            let mock_list = PeerList::test_mock().expect("to create peer list mock");

            let peer_id = IrysPeerId::from(IrysAddress::from([1_u8; 20]));
            let result = fixture
                .client
                .check_health(&peer_id, peer, ProtocolVersion::V2, &mock_list)
                .await;

            assert!(result.is_err());
            match result.unwrap_err() {
                GossipClientError::GetRequest(addr, reason) => {
                    assert_eq!(addr, peer.gossip.to_string());
                    assert!(
                        reason.to_lowercase().contains("connection refused"),
                        "Expected connection refused error, got: {}",
                        reason
                    );
                }
                err => panic!("Expected GetRequest error, got: {:?}", err),
            }
        }

        #[tokio::test]
        async fn test_v2_health_check_accepted() {
            let body = serde_json::to_string(&GossipResponse::Accepted(true)).unwrap();
            let server = MockHttpServer::new_with_response(200, &body, "application/json");
            let fixture = TestFixture::new();
            let peer = create_peer_address("127.0.0.1", server.port());
            let mock_list = PeerList::test_mock().expect("to create peer list mock");

            let peer_id = IrysPeerId::from(IrysAddress::from([1_u8; 20]));
            let result = fixture
                .client
                .check_health(&peer_id, peer, ProtocolVersion::V2, &mock_list)
                .await;

            assert!(
                result.is_ok(),
                "V2 health check should succeed: {:?}",
                result
            );
            assert!(result.unwrap(), "V2 health check should return true");
        }

        #[tokio::test]
        async fn test_v2_404_not_found() {
            let server = MockHttpServer::new_with_response(404, "", "text/plain");
            let fixture = TestFixture::new();
            let peer = create_peer_address("127.0.0.1", server.port());
            let mock_list = PeerList::test_mock().expect("to create peer list mock");

            let peer_id = IrysPeerId::from(IrysAddress::from([1_u8; 20]));
            let result = fixture
                .client
                .check_health(&peer_id, peer, ProtocolVersion::V2, &mock_list)
                .await;

            assert!(result.is_err());
            match result.unwrap_err() {
                GossipClientError::HealthCheck(addr, status) => {
                    assert_eq!(addr, peer.gossip.to_string());
                    assert_eq!(status, StatusCode::NOT_FOUND);
                }
                err => panic!("Expected HealthCheck error, got: {:?}", err),
            }
        }

        #[tokio::test]
        async fn test_v2_invalid_json_response() {
            let server =
                MockHttpServer::new_with_response(200, "invalid json {", "application/json");
            let fixture = TestFixture::new();
            let peer = create_peer_address("127.0.0.1", server.port());
            let mock_list = PeerList::test_mock().expect("to create peer list mock");

            let peer_id = IrysPeerId::from(IrysAddress::from([1_u8; 20]));
            let result = fixture
                .client
                .check_health(&peer_id, peer, ProtocolVersion::V2, &mock_list)
                .await;

            assert!(result.is_err());
            assert!(matches!(
                result.unwrap_err(),
                GossipClientError::GetJsonResponsePayload(_, _)
            ));
        }
    }

    // Response parsing tests
    mod response_parsing_tests {
        use super::*;

        #[tokio::test]
        async fn test_invalid_json_response() {
            let server =
                MockHttpServer::new_with_response(200, "invalid json {", "application/json");
            let fixture = TestFixture::new();
            let peer = create_peer_address("127.0.0.1", server.port());
            let mock_list = PeerList::test_mock().expect("to create peer list mock");

            let peer_id = IrysPeerId::from(IrysAddress::from([1_u8; 20]));
            let result = fixture
                .client
                .check_health(&peer_id, peer, ProtocolVersion::V1, &mock_list)
                .await;

            assert!(result.is_err());
            match result.unwrap_err() {
                GossipClientError::GetJsonResponsePayload(addr, reason) => {
                    assert_eq!(addr, peer.gossip.to_string());
                    assert!(
                        reason.contains("expected")
                            || reason.contains("EOF")
                            || reason.contains("invalid"),
                        "Expected JSON parsing error, got: {}",
                        reason
                    );
                }
                err => panic!("Expected GetJsonResponsePayload error, got: {:?}", err),
            }
        }

        // Additional test for malformed JSON
        #[tokio::test]
        async fn test_truncated_json_response() {
            let server = MockHttpServer::new_with_response(
                200,
                r#"{"status": "healthy", "version"#,
                "application/json",
            );
            let fixture = TestFixture::new();
            let peer = create_peer_address("127.0.0.1", server.port());
            let mock_list = PeerList::test_mock().expect("to create peer list mock");

            let peer_id = IrysPeerId::from(IrysAddress::from([1_u8; 20]));
            let result = fixture
                .client
                .check_health(&peer_id, peer, ProtocolVersion::V1, &mock_list)
                .await;

            assert!(result.is_err());
            assert!(matches!(
                result.unwrap_err(),
                GossipClientError::GetJsonResponsePayload(_, _)
            ));
        }
    }

    mod data_retrieval_scoring_tests {
        use super::*;
        use irys_types::IrysAddress;
        use irys_types::{PeerAddress, PeerListItem, PeerScore, RethPeerInfo};
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use std::time::{SystemTime, UNIX_EPOCH};

        fn create_test_peer(id: u8) -> (IrysPeerId, IrysAddress, PeerListItem) {
            let mining_addr = IrysAddress::from([id; 20]);
            let peer_id = IrysPeerId::from(mining_addr); // For tests: peer_id = mining_addr for simplicity
            let peer_address = PeerAddress {
                gossip: SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(192, 168, 1, id)),
                    8000 + id as u16,
                ),
                api: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, id)), 9000 + id as u16),
                execution: RethPeerInfo::default(),
            };
            let peer = PeerListItem {
                peer_id,
                mining_address: mining_addr,
                address: peer_address,
                reputation_score: PeerScore::new(PeerScore::INITIAL),
                response_time: 100,
                is_online: true,
                last_seen: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                protocol_version: Default::default(),
            };
            (peer_id, mining_addr, peer)
        }

        #[test]
        fn test_handle_data_retrieval_score_success_cases() {
            let test_cases = vec![
                (Duration::from_millis(300), true),
                (Duration::from_millis(499), true),
                (Duration::from_millis(500), true),
                (Duration::from_millis(1500), true),
                (Duration::from_millis(1999), true),
                (Duration::from_millis(2000), true),
            ];

            for (response_time, should_increase) in test_cases {
                let peer_list = PeerList::test_mock().expect("to create peer list mock");
                let fixture = TestFixture::new();
                let (peer_id, _, peer) = create_test_peer(1);
                peer_list.add_or_update_peer(peer, true);

                let initial_score = peer_list.get_peer(&peer_id).unwrap().reputation_score.get();

                GossipClient::handle_data_retrieval_score(
                    &peer_list,
                    &Ok(()),
                    &peer_id,
                    response_time,
                    &fixture.client.circuit_breaker,
                );

                let updated_score = peer_list.get_peer(&peer_id).unwrap().reputation_score.get();
                if should_increase {
                    assert!(
                        updated_score > initial_score,
                        "Response time {:?} should increase score from {} to {}",
                        response_time,
                        initial_score,
                        updated_score
                    );
                }
            }
        }

        #[test]
        fn test_handle_data_retrieval_score_slow_response() {
            const EXPECTED_DECREASE_OF_ONE: u16 = 1;
            let test_cases = vec![
                (Duration::from_secs(3), EXPECTED_DECREASE_OF_ONE),
                (Duration::from_secs(5), EXPECTED_DECREASE_OF_ONE),
                (Duration::from_secs(10), EXPECTED_DECREASE_OF_ONE),
            ];

            for (response_time, expected_decrease) in test_cases {
                let peer_list = PeerList::test_mock().expect("to create peer list mock");
                let fixture = TestFixture::new();
                let (peer_id, _, peer) = create_test_peer(1);
                peer_list.add_or_update_peer(peer, true);

                let initial_score = peer_list.get_peer(&peer_id).unwrap().reputation_score.get();

                GossipClient::handle_data_retrieval_score(
                    &peer_list,
                    &Ok(()),
                    &peer_id,
                    response_time,
                    &fixture.client.circuit_breaker,
                );

                let updated_score = peer_list.get_peer(&peer_id).unwrap().reputation_score.get();
                assert_eq!(
                    updated_score,
                    initial_score - expected_decrease,
                    "Slow response {:?} should decrease score by {}",
                    response_time,
                    expected_decrease
                );
            }
        }

        #[test]
        fn test_handle_data_retrieval_score_failed_response() {
            let peer_list = PeerList::test_mock().expect("to create peer list mock");
            let fixture = TestFixture::new();
            let (peer_id, _, peer) = create_test_peer(1);
            peer_list.add_or_update_peer(peer, true);

            let initial_score = peer_list.get_peer(&peer_id).unwrap().reputation_score.get();
            let response_time = Duration::from_millis(500);

            GossipClient::handle_data_retrieval_score::<()>(
                &peer_list,
                &Err(GossipError::Network("timeout".to_string())),
                &peer_id,
                response_time,
                &fixture.client.circuit_breaker,
            );

            let updated_score = peer_list.get_peer(&peer_id).unwrap().reputation_score.get();
            assert_eq!(
                updated_score,
                initial_score - 3,
                "Failed response should decrease by 3"
            );
        }

        #[test]
        fn test_multiple_score_updates_in_sequence() {
            let peer_list = PeerList::test_mock().expect("to create peer list mock");
            let fixture = TestFixture::new();
            let (peer_id, _, peer) = create_test_peer(1);
            peer_list.add_or_update_peer(peer, true);

            let operations = vec![
                (Duration::from_millis(100), Ok(()), 51),
                (Duration::from_millis(1500), Ok(()), 52),
                (Duration::from_secs(3), Ok(()), 51),
                (
                    Duration::from_millis(500),
                    Err(GossipError::Network("error".to_string())),
                    48,
                ),
            ];

            for (response_time, result, expected_score) in operations {
                match result {
                    Ok(()) => {
                        GossipClient::handle_data_retrieval_score(
                            &peer_list,
                            &Ok(()),
                            &peer_id,
                            response_time,
                            &fixture.client.circuit_breaker,
                        );
                    }
                    Err(e) => {
                        GossipClient::handle_data_retrieval_score::<()>(
                            &peer_list,
                            &Err(e),
                            &peer_id,
                            response_time,
                            &fixture.client.circuit_breaker,
                        );
                    }
                }

                let current_score = peer_list.get_peer(&peer_id).unwrap().reputation_score.get();
                assert_eq!(
                    current_score, expected_score,
                    "After response time {:?}, score should be {}",
                    response_time, expected_score
                );
            }
        }
    }

    mod integration_timing_tests {
        use super::*;

        #[tokio::test]
        async fn test_mock_server_with_delay_functionality() {
            // Test that new_with_delay creates servers with appropriate delays
            let fast_server = MockHttpServer::new_with_delay(200, "test", "text/plain", 100);
            let slow_server = MockHttpServer::new_with_delay(200, "test", "text/plain", 2000);

            let client = reqwest::Client::new();

            // Test fast server
            let start_time = std::time::Instant::now();
            let fast_url = format!("http://127.0.0.1:{}/", fast_server.port());
            let fast_response = client.get(&fast_url).send().await;
            let fast_duration = start_time.elapsed();

            assert!(fast_response.is_ok(), "Fast server should respond");
            assert!(
                fast_duration >= Duration::from_millis(100),
                "Fast server should have at least 100ms delay"
            );
            assert!(
                fast_duration < Duration::from_millis(500),
                "Fast server should respond quickly"
            );

            // Test slow server
            let start_time = std::time::Instant::now();
            let slow_url = format!("http://127.0.0.1:{}/", slow_server.port());
            let slow_response = client.get(&slow_url).send().await;
            let slow_duration = start_time.elapsed();

            assert!(slow_response.is_ok(), "Slow server should respond");
            assert!(
                slow_duration >= Duration::from_secs(2),
                "Slow server should have at least 2s delay"
            );
        }
    }

    mod circuit_breaker_tests {
        use super::*;
        use irys_types::IrysAddress;

        #[tokio::test]
        async fn test_health_check_respects_circuit_breaker() {
            let fixture = TestFixture::new();
            let peer_list = PeerList::test_mock().expect("to create peer list mock");
            let peer_id = IrysPeerId::from(IrysAddress::from([40_u8; 20]));
            let peer_address = create_peer_address("127.0.0.1", 8080);

            // Open the circuit by recording failures (100 to match testing config threshold)
            for _ in 0..100 {
                fixture.client.circuit_breaker.record_failure(&peer_id);
            }

            // Verify circuit is open
            assert!(
                !fixture.client.circuit_breaker.is_available(&peer_id),
                "Circuit breaker should be open after failures"
            );

            // Health check should return error without making a request
            let result = fixture
                .client
                .check_health(&peer_id, peer_address, ProtocolVersion::V1, &peer_list)
                .await;

            assert!(
                result.is_err(),
                "Health check should return error when circuit is open"
            );
            let err = result.unwrap_err();
            assert!(
                matches!(err, GossipClientError::CircuitBreakerOpen(_)),
                "Error should indicate circuit breaker is open, got: {:?}",
                err
            );
        }
    }

    mod concurrent_scoring_tests {
        use super::*;
        use irys_types::IrysAddress;
        use irys_types::{PeerAddress, PeerListItem, PeerScore, ProtocolVersion, RethPeerInfo};
        use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
        use std::sync::Arc;
        use tokio::task::JoinSet;

        #[tokio::test]
        async fn test_concurrent_score_updates() {
            let peer_list = Arc::new(PeerList::test_mock().expect("to create peer list mock"));
            let fixture = Arc::new(TestFixture::new());
            let mining_addr = IrysAddress::from([1_u8; 20]);
            let peer_id = IrysPeerId::from(mining_addr);
            let peer = PeerListItem {
                peer_id,
                mining_address: mining_addr,
                address: PeerAddress {
                    gossip: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 9000)),
                    api: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 9001)),
                    execution: RethPeerInfo::default(),
                },
                reputation_score: PeerScore::new(PeerScore::INITIAL),
                response_time: 0,
                last_seen: 0,
                is_online: true,
                protocol_version: ProtocolVersion::default(),
            };
            peer_list.add_or_update_peer(peer, true);

            let mut join_set = JoinSet::new();

            for i in 0..20 {
                let peer_list_clone = peer_list.clone();
                let fixture_clone = fixture.clone();
                let peer_id_copy = peer_id;

                join_set.spawn(async move {
                    let response_time = if i % 3 == 0 {
                        Duration::from_millis(300)
                    } else if i % 3 == 1 {
                        Duration::from_millis(1500)
                    } else {
                        Duration::from_secs(3)
                    };

                    if i % 4 == 0 {
                        GossipClient::handle_data_retrieval_score::<()>(
                            &peer_list_clone,
                            &Err(GossipError::Network("test".to_string())),
                            &peer_id_copy,
                            response_time,
                            &fixture_clone.client.circuit_breaker,
                        );
                    } else {
                        GossipClient::handle_data_retrieval_score(
                            &peer_list_clone,
                            &Ok(()),
                            &peer_id_copy,
                            response_time,
                            &fixture_clone.client.circuit_breaker,
                        );
                    }
                });
            }

            while (join_set.join_next().await).is_some() {}

            let final_peer = peer_list.get_peer(&peer_id);
            assert!(final_peer.is_some());
            let final_score = final_peer.unwrap().reputation_score.get();
            // Score should be within valid bounds
            assert!(final_score <= PeerScore::MAX);
        }
    }

    mod protocol_version_tests {
        use super::*;

        #[tokio::test]
        async fn test_get_protocol_versions() {
            let server = MockHttpServer::new_with_response(200, "[1, 2]", "application/json");
            let fixture = TestFixture::new();
            let peer = create_peer_address("127.0.0.1", server.port());

            let versions = fixture
                .client
                .get_protocol_versions(peer)
                .await
                .expect("to get versions");
            assert_eq!(versions, vec![1, 2]);
        }
    }

    mod handle_peer_pull_response_tests {
        use super::*;
        use crate::types::HandshakeRequirementReason;
        use irys_types::PeerScore;

        fn test_peer() -> (IrysPeerId, PeerListItem) {
            let peer_id = IrysPeerId::from([2_u8; 20]);
            let item = PeerListItem {
                peer_id,
                mining_address: IrysAddress::from([2_u8; 20]),
                reputation_score: PeerScore::new(PeerScore::INITIAL),
                response_time: 0,
                address: create_peer_address("127.0.0.1", 9999),
                last_seen: 0,
                is_online: true,
                protocol_version: ProtocolVersion::V2,
            };
            (peer_id, item)
        }

        fn identity_block(data: GossipDataV2) -> Result<Arc<IrysBlockHeader>, PeerNetworkError> {
            match data {
                GossipDataV2::BlockHeader(h) => Ok(h),
                _ => Err(PeerNetworkError::UnexpectedData(
                    "expected block header".into(),
                )),
            }
        }

        #[tokio::test]
        async fn accepted_some_returns_ok() {
            let peer = test_peer();
            let peer_list = PeerList::test_mock().unwrap();
            let header = Arc::new(IrysBlockHeader::default());
            let result: GossipResult<GossipResponse<Option<GossipDataV2>>> = Ok(
                GossipResponse::Accepted(Some(GossipDataV2::BlockHeader(header.clone()))),
            );

            let outcome = GossipClient::handle_peer_pull_response(
                result,
                &GossipDataRequestV2::BlockHeader(H256::zero()),
                &peer,
                &peer_list,
                identity_block,
                0,
            )
            .await;

            match outcome {
                PeerPullOutcome::Ok((id, _block)) => assert_eq!(id, peer.0),
                other => panic!("expected Ok, got {:?}", other),
            }
        }

        #[tokio::test]
        async fn accepted_none_returns_err() {
            let peer = test_peer();
            let peer_list = PeerList::test_mock().unwrap();
            let result = Ok(GossipResponse::Accepted(None));

            let outcome = GossipClient::handle_peer_pull_response(
                result,
                &GossipDataRequestV2::BlockHeader(H256::zero()),
                &peer,
                &peer_list,
                identity_block,
                0,
            )
            .await;

            assert!(matches!(outcome, PeerPullOutcome::Err(_)));
        }

        #[tokio::test(start_paused = true)]
        async fn handshake_required_on_first_attempt_returns_retry() {
            let peer = test_peer();
            let peer_list = PeerList::test_mock().unwrap();
            let result = Ok(GossipResponse::Rejected(
                RejectionReason::HandshakeRequired(Some(
                    HandshakeRequirementReason::RequestOriginIsNotInThePeerList,
                )),
            ));

            let outcome = GossipClient::handle_peer_pull_response(
                result,
                &GossipDataRequestV2::BlockHeader(H256::zero()),
                &peer,
                &peer_list,
                identity_block,
                0,
            )
            .await;

            assert!(matches!(outcome, PeerPullOutcome::RetryAfterHandshake));
        }

        #[tokio::test]
        async fn handshake_required_on_second_attempt_returns_err() {
            let peer = test_peer();
            let peer_list = PeerList::test_mock().unwrap();
            let result = Ok(GossipResponse::Rejected(
                RejectionReason::HandshakeRequired(Some(
                    HandshakeRequirementReason::RequestOriginIsNotInThePeerList,
                )),
            ));

            let outcome = GossipClient::handle_peer_pull_response(
                result,
                &GossipDataRequestV2::BlockHeader(H256::zero()),
                &peer,
                &peer_list,
                identity_block,
                1,
            )
            .await;

            assert!(matches!(outcome, PeerPullOutcome::Err(_)));
        }

        #[tokio::test]
        async fn gossip_disabled_sets_peer_offline() {
            let peer = test_peer();
            let peer_list = PeerList::test_mock().unwrap();
            // Add the peer so set_is_online_by_peer_id can find it
            peer_list.add_or_update_peer(peer.1.clone(), true);
            let result = Ok(GossipResponse::Rejected(RejectionReason::GossipDisabled));

            let outcome = GossipClient::handle_peer_pull_response(
                result,
                &GossipDataRequestV2::BlockHeader(H256::zero()),
                &peer,
                &peer_list,
                identity_block,
                0,
            )
            .await;

            assert!(matches!(outcome, PeerPullOutcome::Err(_)));
            // Verify the peer was marked offline
            if let Some(item) = peer_list.peer_by_id(&peer.0) {
                assert!(!item.is_online);
            }
        }

        #[tokio::test]
        async fn gossip_error_returns_err() {
            let peer = test_peer();
            let peer_list = PeerList::test_mock().unwrap();
            let result = Err(GossipError::Network("connection reset".into()));

            let outcome = GossipClient::handle_peer_pull_response(
                result,
                &GossipDataRequestV2::BlockHeader(H256::zero()),
                &peer,
                &peer_list,
                identity_block,
                0,
            )
            .await;

            assert!(matches!(outcome, PeerPullOutcome::Err(_)));
        }
    }

    mod pull_with_handshake_retry_tests {
        use super::*;
        use crate::types::HandshakeRequirementReason;
        use irys_types::PeerScore;
        use std::sync::atomic::{AtomicU32, Ordering};

        fn test_peer() -> (IrysPeerId, PeerListItem) {
            let peer_id = IrysPeerId::from([3_u8; 20]);
            let item = PeerListItem {
                peer_id,
                mining_address: IrysAddress::from([3_u8; 20]),
                reputation_score: PeerScore::new(PeerScore::INITIAL),
                response_time: 0,
                address: create_peer_address("127.0.0.1", 9998),
                last_seen: 0,
                is_online: true,
                protocol_version: ProtocolVersion::V2,
            };
            (peer_id, item)
        }

        fn identity_block(data: GossipDataV2) -> Result<Arc<IrysBlockHeader>, PeerNetworkError> {
            match data {
                GossipDataV2::BlockHeader(h) => Ok(h),
                _ => Err(PeerNetworkError::UnexpectedData(
                    "expected block header".into(),
                )),
            }
        }

        #[tokio::test]
        async fn immediate_success() {
            let fixture = TestFixture::new();
            let peer = test_peer();
            let peer_list = PeerList::test_mock().unwrap();
            let header = Arc::new(IrysBlockHeader::default());
            let header_clone = header.clone();

            let result =
                fixture
                    .client
                    .pull_with_handshake_retry(
                        GossipDataRequestV2::BlockHeader(H256::zero()),
                        &peer,
                        &peer_list,
                        identity_block,
                        || {
                            let h = header_clone.clone();
                            async move {
                                Ok(GossipResponse::Accepted(Some(GossipDataV2::BlockHeader(h))))
                            }
                        },
                    )
                    .await;

            assert!(result.is_ok());
            let (peer_id, _block) = result.unwrap();
            assert_eq!(peer_id, peer.0);
        }

        #[tokio::test(start_paused = true)]
        async fn handshake_required_then_success_on_retry() {
            let fixture = TestFixture::new();
            let peer = test_peer();
            let peer_list = PeerList::test_mock().unwrap();
            let header = Arc::new(IrysBlockHeader::default());
            let header_clone = header.clone();
            let call_count = Arc::new(AtomicU32::new(0));
            let call_count_clone = call_count.clone();

            let result = fixture
                .client
                .pull_with_handshake_retry(
                    GossipDataRequestV2::BlockHeader(H256::zero()),
                    &peer,
                    &peer_list,
                    identity_block,
                    move || {
                        let attempt = call_count_clone.fetch_add(1, Ordering::SeqCst);
                        let h = header_clone.clone();
                        async move {
                            if attempt == 0 {
                                Ok(GossipResponse::Rejected(
                                    RejectionReason::HandshakeRequired(Some(
                                        HandshakeRequirementReason::RequestOriginIsNotInThePeerList,
                                    )),
                                ))
                            } else {
                                Ok(GossipResponse::Accepted(Some(GossipDataV2::BlockHeader(h))))
                            }
                        }
                    },
                )
                .await;

            assert!(result.is_ok());
            assert_eq!(call_count.load(Ordering::SeqCst), 2);
        }

        #[tokio::test(start_paused = true)]
        async fn handshake_required_both_attempts_returns_err() {
            let fixture = TestFixture::new();
            let peer = test_peer();
            let peer_list = PeerList::test_mock().unwrap();

            let result = fixture
                .client
                .pull_with_handshake_retry(
                    GossipDataRequestV2::BlockHeader(H256::zero()),
                    &peer,
                    &peer_list,
                    identity_block,
                    || async {
                        Ok(GossipResponse::Rejected(
                            RejectionReason::HandshakeRequired(Some(
                                HandshakeRequirementReason::RequestOriginIsNotInThePeerList,
                            )),
                        ))
                    },
                )
                .await;

            assert!(result.is_err());
        }

        #[tokio::test]
        async fn persistent_error_returns_err() {
            let fixture = TestFixture::new();
            let peer = test_peer();
            let peer_list = PeerList::test_mock().unwrap();

            let result = fixture
                .client
                .pull_with_handshake_retry(
                    GossipDataRequestV2::BlockHeader(H256::zero()),
                    &peer,
                    &peer_list,
                    identity_block,
                    || async { Err(GossipError::Network("connection refused".into())) },
                )
                .await;

            assert!(result.is_err());
        }
    }
}
