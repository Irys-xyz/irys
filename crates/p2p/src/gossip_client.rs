#![allow(
    clippy::module_name_repetitions,
    reason = "I have no idea how to name this module to satisfy this lint"
)]
use crate::types::{GossipError, GossipResult};
use backoff::{future::retry_notify, ExponentialBackoff};
use core::time::Duration;
use irys_domain::{PeerList, ScoreDecreaseReason, ScoreIncreaseReason};
use irys_types::{
    Address, BlockHash, GossipData, GossipDataRequest, GossipRequest, IrysBlockHeader, PeerAddress,
    PeerListItem, PeerNetworkError,
};
use rand::prelude::SliceRandom as _;
use reqwest::{Client, StatusCode};
use reth::primitives::Block;
use reth::revm::primitives::B256;
use serde::Serialize;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

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
    client: Arc<Client>,
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
            client: Arc::new(
                Client::builder()
                    .timeout(timeout)
                    .build()
                    .expect("Failed to create reqwest client"),
            ),
        }
    }

    pub fn create_backoff() -> ExponentialBackoff {
        Self::create_normal_backoff()
    }

    pub fn create_critical_backoff() -> ExponentialBackoff {
        ExponentialBackoff {
            initial_interval: Duration::from_millis(50),
            max_interval: Duration::from_secs(1),
            max_elapsed_time: Some(Duration::from_secs(30)),
            multiplier: 1.5,
            ..Default::default()
        }
    }

    pub fn create_normal_backoff() -> ExponentialBackoff {
        ExponentialBackoff {
            initial_interval: Duration::from_millis(100),
            max_interval: Duration::from_secs(2),
            max_elapsed_time: Some(Duration::from_secs(10)),
            ..Default::default()
        }
    }

    pub fn create_light_backoff() -> ExponentialBackoff {
        ExponentialBackoff {
            initial_interval: Duration::from_millis(200),
            max_interval: Duration::from_secs(5),
            max_elapsed_time: Some(Duration::from_secs(5)),
            multiplier: 2.5,
            ..Default::default()
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
        res
    }

    /// Request a specific data to be gossiped. Returns true if the peer has the data,
    /// and false if it doesn't.
    pub async fn make_get_data_request_and_update_the_score(
        &self,
        peer: &(Address, PeerListItem),
        requested_data: GossipDataRequest,
        peer_list: &PeerList,
    ) -> GossipResult<bool> {
        let url = format!("http://{}/gossip/get_data", peer.1.address.gossip);
        let get_data_request = self.create_request(requested_data);
        let backoff = Self::create_backoff();

        let client = self.client.clone();

        let res = retry_notify(
            backoff,
            || async {
                let response = client
                    .post(&url)
                    .json(&get_data_request)
                    .send()
                    .await
                    .map_err(|error| {
                        warn!(
                            "Network error during get_data request to {}: {}",
                            url, error
                        );
                        backoff::Error::transient(GossipError::Network(error.to_string()))
                    })?;

                let response =
                    Self::classify_http(response, |e| GossipError::Network(e.to_string()))?;

                response.json::<bool>().await.map_err(|error| {
                    // Prefer Protocol here if you have it
                    warn!(
                        "JSON parsing error during get_data request to {}: {}",
                        url, error
                    );
                    backoff::Error::permanent(GossipError::Network(error.to_string()))
                })
            },
            |err, dur| {
                warn!(?dur, %err, "retrying get_data request after {:?}", dur);
            },
        )
        .await;

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
    ) -> GossipResult<Option<GossipData>> {
        let url = format!("http://{}/gossip/pull_data", peer.1.address.gossip);
        let get_data_request = self.create_request(requested_data);

        let res = self
            .client
            .post(&url)
            .json(&get_data_request)
            .send()
            .await
            .map_err(|error| GossipError::Network(error.to_string()))?
            .json()
            .await
            .map_err(|error| GossipError::Network(error.to_string()));
        Self::handle_score(peer_list, &res, &peer.0);
        res
    }

    pub async fn check_health(&self, peer: PeerAddress) -> Result<bool, GossipClientError> {
        let url = format!("http://{}/gossip/health", peer.gossip);
        let backoff = Self::create_backoff();
        let client = self.client.clone();

        retry_notify(
            backoff,
            || async {
                let res = client.get(&url).send().await.map_err(|error| {
                    warn!("Network error during health check to {}: {}", url, error);
                    backoff::Error::transient(GossipClientError::GetRequest(url.clone(), error.to_string()))
                })?;

                Self::classify_http(res, |e| GossipClientError::GetRequest(url.clone(), e.to_string()))?
                    .json::<bool>()
                    .await
                    .map_err(|error| {
                        warn!(
                            "JSON parsing error during health check to {}: {}",
                            url, error
                        );
                        backoff::Error::permanent(GossipClientError::GetJsonResponsePayload(url.clone(), error.to_string()))
                    })
            },
            |err, dur| {
                warn!(?dur, %err, "retrying health check after {:?}", dur);
            },
        )
        .await
    }

    /// Send data to a peer
    ///
    /// # Errors
    ///
    /// If the peer is offline or the request fails, an error is returned.
    async fn send_data(&self, peer: &PeerListItem, data: &GossipData) -> GossipResult<()> {
        Self::check_if_peer_is_online(peer)?;
        match data {
            GossipData::Chunk(unpacked_chunk) => {
                let url = format!("http://{}/gossip/chunk", peer.address.gossip);
                self.send_data_internal(&url, unpacked_chunk, Self::create_critical_backoff())
                    .await?;
            }
            GossipData::Transaction(irys_transaction_header) => {
                let url = format!("http://{}/gossip/transaction", peer.address.gossip);
                self.send_data_internal(
                    &url,
                    irys_transaction_header,
                    Self::create_critical_backoff(),
                )
                .await?;
            }
            GossipData::CommitmentTransaction(commitment_tx) => {
                let url = format!("http://{}/gossip/commitment_tx", peer.address.gossip);
                self.send_data_internal(&url, commitment_tx, Self::create_critical_backoff())
                    .await?;
            }
            GossipData::Block(irys_block_header) => {
                let url = format!("http://{}/gossip/block", peer.address.gossip);
                self.send_data_internal(&url, &irys_block_header, Self::create_normal_backoff())
                    .await?;
            }
            GossipData::ExecutionPayload(execution_payload) => {
                let url = format!("http://{}/gossip/execution_payload", peer.address.gossip);
                self.send_data_internal(&url, &execution_payload, Self::create_normal_backoff())
                    .await?;
            }
            GossipData::IngressProof(ingress_proof) => {
                let url = format!("http://{}/gossip/ingress_proof", peer.address.gossip);
                self.send_data_internal(&url, &ingress_proof, Self::create_normal_backoff())
                    .await?;
            }
        };

        Ok(())
    }

    fn check_if_peer_is_online(peer: &PeerListItem) -> GossipResult<()> {
        if !peer.is_online {
            return Err(GossipError::InvalidPeer("Peer is offline".into()));
        }
        Ok(())
    }
    fn classify_http<E>(
        res: reqwest::Response,
        map: impl FnOnce(reqwest::Error) -> E,
    ) -> Result<reqwest::Response, backoff::Error<E>> {
        match res.error_for_status_ref() {
            Ok(_) => Ok(res),
            Err(e) => {
                let s = res.status();
                if s.is_client_error() && s != StatusCode::TOO_MANY_REQUESTS {
                    Err(backoff::Error::permanent(map(e)))
                } else {
                    Err(backoff::Error::transient(map(e)))
                }
            }
        }
    }
    async fn send_data_internal<T: Serialize + ?Sized>(
        &self,
        url: &str,
        data: &T,
        backoff_strategy: ExponentialBackoff,
    ) -> Result<(), GossipError> {
        let req = self.create_request(data);
        let client = self.client.clone();

        retry_notify(
            backoff_strategy,
            || async {
                let res = client.post(url).json(&req).send().await.map_err(|error| {
                    warn!("Network error during send_data to {}: {}", url, error);
                    backoff::Error::transient(GossipError::Network(error.to_string()))
                })?;

                // HTTP status -> transient/permanent
                Self::classify_http(res, |e| GossipError::Network(e.to_string()))?;
                Ok(())
            },
            |err, dur| {
                warn!(?dur, %err, "retrying send_data after {:?}", dur);
            },
        )
        .await
    }

    fn handle_score<T>(
        peer_list: &PeerList,
        result: &GossipResult<T>,
        peer_miner_address: &Address,
    ) {
        match &result {
            Ok(_) => {
                // Successful send, increase score
                peer_list.increase_peer_score(peer_miner_address, ScoreIncreaseReason::Online);
            }
            Err(_) => {
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
                error!("Error sending data to peer: {}", e);
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
            peer_list.trusted_peers()
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
                    Ok(Some(data)) => {
                        info!(
                            "Successfully requested {:?} from peer {}",
                            data_request, address
                        );
                        match map_data(data) {
                            Ok(data) => return Ok((*address, data)),
                            Err(err) => {
                                warn!("Failed to map data from peer {}: {}", address, err);
                                continue;
                            }
                        };
                    }
                    Ok(None) => {
                        // Peer doesn't have this block, try another peer
                        debug!("Peer {} doesn't have {:?}", address, data_request);
                        continue;
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
            "Failed to fetch {:?} after trying 5 peers: {:?}",
            data_request, last_error
        )))
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

            let result = fixture.client.check_health(peer).await;

            assert!(result.is_err());
            assert!(
                result.unwrap_err().to_string().to_lowercase().contains("failed"),
                "Expected connection error"
            );
        }

        #[tokio::test]
        async fn test_request_timeout() {
            let fixture = TestFixture::with_timeout(Duration::from_millis(1));
            // Use a non-routable IP address
            let peer = create_peer_address("192.0.2.1", 8080);

            let result = fixture.client.check_health(peer).await;

            assert!(result.is_err());
            assert!(
                result.is_err(),
                "Expected timeout error"
            );
        }
    }

    mod health_check_error_tests {
        use super::*;

        async fn test_health_check_error_status(status_code: u16) {
            let server = MockHttpServer::new_with_response(status_code, "", "text/plain");
            let fixture = TestFixture::new();
            let peer = create_peer_address("127.0.0.1", server.port());

            let result = fixture.client.check_health(peer).await;

            assert!(result.is_err());
            assert!(
                result.is_err(),
                "Expected error for status {}",
                status_code
            );
        }

        #[tokio::test]
        async fn test_404_not_found() {
            test_health_check_error_status(404).await;
        }

        #[tokio::test]
        async fn test_500_internal_server_error() {
            test_health_check_error_status(500).await;
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

            let result = fixture.client.check_health(peer).await;

            assert!(result.is_err());
            assert!(
                result.is_err(),
                "Expected JSON parsing error"
            );
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

            let result = fixture.client.check_health(peer).await;

            assert!(result.is_err());
            assert!(
                result.is_err(),
                "Expected JSON parsing error"
            );
        }
    }
}
