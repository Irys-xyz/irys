#![allow(
    clippy::module_name_repetitions,
    reason = "I have no idea how to name this module to satisfy this lint"
)]
use crate::types::{GossipError, GossipResult};
use core::time::Duration;
use irys_domain::{PeerList, ScoreDecreaseReason, ScoreIncreaseReason};
use irys_types::{
    Address, GossipData, GossipDataRequest, GossipRequest, PeerAddress, PeerListItem,
};
use reqwest::Client;
use reqwest::Response;
use serde::Serialize;
use std::sync::Arc;
use tracing::error;

#[derive(Debug, Clone, thiserror::Error)]
pub enum GossipClientError {
    #[error("Get request to {0} failed with reason {1}")]
    GetRequestFailed(String, String),
    #[error("Health check to {0} failed with status code {1}")]
    HealthCheckFailed(String, reqwest::StatusCode),
    #[error("Failed to get json response payload from {0} with reason {1}")]
    GetJsonResponsePayloadFailed(String, String),
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
        let peer_addr = peer.gossip.to_string();

        let response = self
            .internal_client()
            .get(&url)
            .send()
            .await
            .map_err(|error| {
                GossipClientError::GetRequestFailed(peer_addr.clone(), error.to_string())
            })?;

        if !response.status().is_success() {
            return Err(GossipClientError::HealthCheckFailed(
                peer_addr,
                response.status(),
            ));
        }

        response.json().await.map_err(|error| {
            GossipClientError::GetJsonResponsePayloadFailed(peer_addr, error.to_string())
        })
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
                self.send_data_internal(
                    format!("http://{}/gossip/chunk", peer.address.gossip),
                    unpacked_chunk,
                )
                .await?;
            }
            GossipData::Transaction(irys_transaction_header) => {
                self.send_data_internal(
                    format!("http://{}/gossip/transaction", peer.address.gossip),
                    irys_transaction_header,
                )
                .await?;
            }
            GossipData::CommitmentTransaction(commitment_tx) => {
                self.send_data_internal(
                    format!("http://{}/gossip/commitment_tx", peer.address.gossip),
                    commitment_tx,
                )
                .await?;
            }
            GossipData::Block(irys_block_header) => {
                self.send_data_internal(
                    format!("http://{}/gossip/block", peer.address.gossip),
                    &irys_block_header,
                )
                .await?;
            }
            GossipData::ExecutionPayload(execution_payload) => {
                self.send_data_internal(
                    format!("http://{}/gossip/execution_payload", peer.address.gossip),
                    &execution_payload,
                )
                .await?;
            }
            GossipData::IngressProof(ingress_proof) => {
                self.send_data_internal(
                    format!("http://{}/gossip/ingress_proof", peer.address.gossip),
                    &ingress_proof,
                )
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

    async fn send_data_internal<T: Serialize + ?Sized>(
        &self,
        url: String,
        data: &T,
    ) -> Result<Response, GossipError> {
        let req = self.create_request(data);
        self.client
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|error| GossipError::Network(error.to_string()))
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
                client: GossipClient::new(Duration::from_secs(1), Address::from([1u8; 20])),
            }
        }

        fn with_timeout(timeout: Duration) -> Self {
            Self {
                client: GossipClient::new(timeout, Address::from([1u8; 20])),
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
            match result.unwrap_err() {
                GossipClientError::GetRequestFailed(addr, reason) => {
                    assert_eq!(addr, peer.gossip.to_string());
                    assert!(
                        reason.to_lowercase().contains("connection refused"),
                        "Expected connection refused error, got: {}",
                        reason
                    );
                }
                err => panic!("Expected GetRequestFailed error, got: {:?}", err),
            }
        }

        #[tokio::test]
        async fn test_request_timeout() {
            let fixture = TestFixture::with_timeout(Duration::from_millis(1));
            // Use a non-routable IP address
            let peer = create_peer_address("192.0.2.1", 8080);

            let result = fixture.client.check_health(peer).await;

            assert!(result.is_err());
            match result.unwrap_err() {
                GossipClientError::GetRequestFailed(addr, reason) => {
                    assert_eq!(addr, peer.gossip.to_string());
                    assert!(!reason.is_empty(), "Expected timeout error message");
                }
                err => panic!(
                    "Expected GetRequestFailed error for timeout, got: {:?}",
                    err
                ),
            }
        }
    }

    mod health_check_error_tests {
        use super::*;

        async fn test_health_check_error_status(status_code: u16, expected_status: StatusCode) {
            let server = MockHttpServer::new_with_response(status_code, "", "text/plain");
            let fixture = TestFixture::new();
            let peer = create_peer_address("127.0.0.1", server.port());

            let result = fixture.client.check_health(peer).await;

            assert!(result.is_err());
            match result.unwrap_err() {
                GossipClientError::HealthCheckFailed(addr, status) => {
                    assert_eq!(addr, peer.gossip.to_string());
                    assert_eq!(status, expected_status);
                }
                err => panic!(
                    "Expected HealthCheckFailed error for status {}, got: {:?}",
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

            let result = fixture.client.check_health(peer).await;

            assert!(result.is_err());
            match result.unwrap_err() {
                GossipClientError::GetJsonResponsePayloadFailed(addr, reason) => {
                    assert_eq!(addr, peer.gossip.to_string());
                    assert!(
                        reason.contains("expected")
                            || reason.contains("EOF")
                            || reason.contains("invalid"),
                        "Expected JSON parsing error, got: {}",
                        reason
                    );
                }
                err => panic!(
                    "Expected GetJsonResponsePayloadFailed error, got: {:?}",
                    err
                ),
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

            let result = fixture.client.check_health(peer).await;

            assert!(result.is_err());
            assert!(matches!(
                result.unwrap_err(),
                GossipClientError::GetJsonResponsePayloadFailed(_, _)
            ));
        }
    }
}
