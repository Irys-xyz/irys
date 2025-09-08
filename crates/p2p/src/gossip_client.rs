#![allow(
    clippy::module_name_repetitions,
    reason = "I have no idea how to name this module to satisfy this lint"
)]
use crate::types::{GossipError, GossipResponse, GossipResult, RejectionReason};
use crate::GossipCache;
use core::time::Duration;
use irys_domain::{PeerList, ScoreDecreaseReason, ScoreIncreaseReason};
use irys_types::{
    Address, BlockHash, GossipCacheKey, GossipData, GossipDataRequest, GossipRequest,
    IrysBlockHeader, PeerAddress, PeerListItem, PeerNetworkError,
};
use rand::prelude::SliceRandom as _;
use reqwest::{Client, StatusCode};
use reth::primitives::Block;
use reth::revm::primitives::B256;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, error, warn};

#[derive(Debug, Clone, thiserror::Error)]
pub enum GossipClientError {
    #[error("Get request to {0} failed with reason {1}")]
    GetRequest(String, String),
    #[error("Health check to {0} failed with status code {1}")]
    HealthCheck(String, reqwest::StatusCode),
    #[error("Failed to get json response payload from {0} with reason {1}")]
    GetJsonResponsePayload(String, String),
}

#[derive(Debug, Clone)]
pub struct GossipClient {
    pub mining_address: Address,
    client: Client,
}

// TODO: Remove this when PeerList is no longer an actix service
impl Default for GossipClient {
    fn default() -> Self {
        panic!("GossipClient must be initialized with a timeout and mining address. Default is implemented only to satisfy actix trait bounds.");
    }
}

impl GossipClient {
    #[must_use]
    pub fn new(timeout: Duration, mining_address: Address) -> Self {
        Self {
            mining_address,
            client: Client::builder()
                .timeout(timeout)
                .build()
                .expect("Failed to create reqwest client"),
        }
    }

    pub fn internal_client(&self) -> &Client {
        &self.client
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
        let peer_miner_address = peer.0;
        let peer = peer.1;

        let res = self.send_data(peer, data).await;
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
        let url = format!("http://{}/gossip/get_data", peer.1.address.gossip);

        let res = self.send_data_internal(url, &requested_data).await;
        Self::handle_score(peer_list, &res, &peer.0);
        res
    }

    /// Request specific data from the peer. Returns the data right away if the peer has it
    /// and updates the peer's score based on the result.
    async fn pull_data_and_update_the_score(
        &self,
        peer: &(Address, PeerListItem),
        requested_data: GossipDataRequest,
        peer_list: &PeerList,
    ) -> GossipResult<GossipResponse<Option<GossipData>>> {
        let url = format!("http://{}/gossip/pull_data", peer.1.address.gossip);

        let res = self.send_data_internal(url, &requested_data).await;
        Self::handle_score(peer_list, &res, &peer.0);
        res
    }

    pub async fn check_health(
        &self,
        peer: PeerAddress,
        peer_list: &PeerList,
    ) -> Result<bool, GossipClientError> {
        let url = format!("http://{}/gossip/health", peer.gossip);
        let peer_addr = peer.gossip.to_string();

        let response = self
            .internal_client()
            .get(&url)
            .send()
            .await
            .map_err(|error| GossipClientError::GetRequest(peer_addr.clone(), error.to_string()))?;

        if !response.status().is_success() {
            return Err(GossipClientError::HealthCheck(peer_addr, response.status()));
        }

        let response: GossipResponse<bool> = response.json().await.map_err(|error| {
            GossipClientError::GetJsonResponsePayload(peer_addr, error.to_string())
        })?;

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
                self.send_data_internal(
                    format!("http://{}/gossip/chunk", peer.address.gossip),
                    unpacked_chunk,
                )
                .await
            }
            GossipData::Transaction(irys_transaction_header) => {
                self.send_data_internal(
                    format!("http://{}/gossip/transaction", peer.address.gossip),
                    irys_transaction_header,
                )
                .await
            }
            GossipData::CommitmentTransaction(commitment_tx) => {
                self.send_data_internal(
                    format!("http://{}/gossip/commitment_tx", peer.address.gossip),
                    commitment_tx,
                )
                .await
            }
            GossipData::Block(irys_block_header) => {
                self.send_data_internal(
                    format!("http://{}/gossip/block", peer.address.gossip),
                    &irys_block_header,
                )
                .await
            }
            GossipData::ExecutionPayload(execution_payload) => {
                self.send_data_internal(
                    format!("http://{}/gossip/execution_payload", peer.address.gossip),
                    &execution_payload,
                )
                .await
            }
            GossipData::IngressProof(ingress_proof) => {
                self.send_data_internal(
                    format!("http://{}/gossip/ingress_proof", peer.address.gossip),
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

        let req = self.create_request(data);
        let response =
            self.client
                .post(&url)
                .json(&req)
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

                let body = serde_json::from_str(&text).map_err(|e| {
                    GossipError::Network(format!(
                        "Failed to parse JSON: {} - Response: {}",
                        e, text
                    ))
                })?;
                Ok(body)
            }
            _ => {
                let error_text = response.text().await.unwrap_or_default();
                Err(GossipError::Network(format!(
                    "API request failed with status: {} - {}",
                    status, error_text
                )))
            }
        }
    }

    fn handle_score<T>(
        peer_list: &PeerList,
        result: &GossipResult<GossipResponse<T>>,
        peer_miner_address: &Address,
    ) {
        match &result {
            Ok(_) => {
                // Successful send, increase score for data request
                peer_list.increase_peer_score(peer_miner_address, ScoreIncreaseReason::DataRequest);
                peer_list.set_is_online(peer_miner_address, true);
            }
            Err(err) => {
                if let GossipError::Network(_message) = err {
                    peer_list.set_is_online(peer_miner_address, false);
                }
                // Failed to send, decrease score
                peer_list.decrease_peer_score(peer_miner_address, ScoreDecreaseReason::Offline);
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

        tokio::spawn(async move {
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
        });
    }

    /// Sends data to a peer without updating their score
    pub fn send_data_without_score_update(
        &self,
        peer: (&Address, &PeerListItem),
        data: Arc<GossipData>,
    ) {
        let client = self.clone();
        let peer = peer.1.clone();

        tokio::spawn(async move {
            if let Err(e) = client.send_data(&peer, &data).await {
                error!("Error sending data to peer: {}", e);
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

        tokio::spawn(async move {
            let result = client.send_data(&peer, &data).await;
            match &result {
                Ok(_) => {
                    // Use DataRequest reason for score increase
                    peer_list
                        .increase_peer_score(&peer_miner_address, ScoreIncreaseReason::DataRequest);
                }
                Err(_) => {
                    peer_list
                        .decrease_peer_score(&peer_miner_address, ScoreDecreaseReason::Offline);
                }
            }
        });
    }

    fn create_request<T>(&self, data: T) -> GossipRequest<T> {
        GossipRequest {
            miner_address: self.mining_address,
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
        self.pull_data_from_network(data_request, use_trusted_peers_only, peer_list, Self::block)
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
            Self::execution_payload,
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
                        let header = Self::block(data)?;
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

    fn block(gossip_data: GossipData) -> Result<Arc<IrysBlockHeader>, PeerNetworkError> {
        match gossip_data {
            GossipData::Block(block) => Ok(block),
            _ => Err(PeerNetworkError::UnexpectedData(format!(
                "Expected IrysBlockHeader, got {:?}",
                gossip_data.data_type_and_id()
            ))),
        }
    }

    fn execution_payload(gossip_data: GossipData) -> Result<Block, PeerNetworkError> {
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

        // Try up to 5 iterations over the peer list to get the block
        let mut last_error = None;

        for attempt in 1..=5 {
            for peer in &peers {
                let address = &peer.0;
                debug!(
                    "Attempting to fetch {:?} from peer {} (attempt {}/5)",
                    data_request, address, attempt
                );

                match self
                    .pull_data_and_update_the_score(peer, data_request.clone(), peer_list)
                    .await
                {
                    Ok(response) => {
                        match response {
                            GossipResponse::Accepted(maybe_data) => match maybe_data {
                                Some(data) => match map_data(data) {
                                    Ok(data) => return Ok((*address, data)),
                                    Err(err) => {
                                        warn!(
                                            "Failed to map data from peer {:?}: {:?}",
                                            address, err
                                        );
                                        continue;
                                    }
                                },
                                None => {
                                    // Peer doesn't have this block, try another peer
                                    debug!("Peer {} doesn't have {:?}", address, data_request);
                                    continue;
                                }
                            },
                            GossipResponse::Rejected(reason) => {
                                warn!(
                                    "Peer {} reject the request: {:?}: {:?}",
                                    address, data_request, reason
                                );
                                match reason {
                                    RejectionReason::HandshakeRequired => {
                                        peer_list.initiate_handshake(peer.1.address.api, true);
                                        last_error = Some(GossipError::from(
                                            PeerNetworkError::FailedToRequestData(format!(
                                                "Peer {:?} requires a handshake",
                                                address
                                            )),
                                        ));
                                    }
                                    RejectionReason::GossipDisabled => {
                                        peer_list.set_is_online(&peer.0, false);
                                        last_error = Some(GossipError::from(
                                            PeerNetworkError::FailedToRequestData(format!(
                                                "Peer {:?} has gossip disabled",
                                                address
                                            )),
                                        ));
                                    }
                                }
                                continue;
                            }
                        }
                    }
                    Err(err) => {
                        last_error = Some(err);
                        warn!(
                            "Failed to fetch {:?} from peer {} (attempt {}/5): {}",
                            data_request,
                            address,
                            attempt,
                            last_error.as_ref().unwrap()
                        );

                        // Move on to the next peer
                        continue;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
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
            match self.check_health(peer.1.address, peer_list).await {
                Ok(is_healthy) => {
                    debug!("Peer {} is healthy: {}", peer.0, is_healthy);
                    peer_list.set_is_online(&peer.0, is_healthy);
                }
                Err(err) => {
                    warn!(
                        "Failed to check the health of peer {}: {}, setting offline status",
                        peer.0, err
                    );
                    peer_list.set_is_online(&peer.0, false);
                }
            }
        }
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
                client: GossipClient::new(Duration::from_secs(1), Address::from([1_u8; 20])),
            }
        }

        fn with_timeout(timeout: Duration) -> Self {
            Self {
                client: GossipClient::new(timeout, Address::from([1_u8; 20])),
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

            let result = fixture.client.check_health(peer, &mock_list).await;

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

            let result = fixture.client.check_health(peer, &mock_list).await;

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

            let result = fixture.client.check_health(peer, &mock_list).await;

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

            let result = fixture.client.check_health(peer, &mock_list).await;

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

            let result = fixture.client.check_health(peer, &mock_list).await;

            assert!(result.is_err());
            assert!(matches!(
                result.unwrap_err(),
                GossipClientError::GetJsonResponsePayload(_, _)
            ));
        }
    }
}
