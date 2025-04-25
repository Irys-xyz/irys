use base58::ToBase58;
use eyre::Result;
use irys_types::{CombinedBlockHeader, IrysTransactionHeader, PeerResponse, VersionRequest, H256};
use reqwest::{Client, StatusCode};
use serde::{de::DeserializeOwned, Serialize};
use std::net::SocketAddr;
use tracing::error;

/// Trait defining the interface for the API client
#[async_trait::async_trait]
pub trait ApiClient: Send + Sync + Clone {
    /// Fetch a transaction header by its ID from a peer
    async fn get_transaction(
        &self,
        peer: SocketAddr,
        tx_id: H256,
    ) -> Result<Option<IrysTransactionHeader>>;

    /// Post a transaction header to a node
    async fn post_transaction(
        &self,
        peer: SocketAddr,
        transaction: IrysTransactionHeader,
    ) -> Result<()>;

    /// Fetch multiple transaction headers by their IDs from a peer
    async fn get_transactions(
        &self,
        peer: SocketAddr,
        tx_ids: &[H256],
    ) -> Result<Vec<Option<IrysTransactionHeader>>>;

    /// Post a version request to a peer. Version request contains protocol version and peer
    /// information.
    async fn post_version(&self, peer: SocketAddr, version: VersionRequest)
        -> Result<PeerResponse>;

    /// Gets block by hash
    async fn get_block_by_hash(&self, peer: SocketAddr, block_hash: H256) -> Result<Option<CombinedBlockHeader>>;
}

/// Real implementation of the API client that makes actual HTTP requests
#[derive(Clone, Debug, Default)]
pub struct IrysApiClient {
    client: Client,
}

impl IrysApiClient {
    pub fn new() -> Self {
        Self {
            client: Client::default(),
        }
    }

    async fn make_request<T: DeserializeOwned, B: Serialize>(
        &self,
        peer: SocketAddr,
        method: &str,
        path: &str,
        body: Option<&B>,
    ) -> Result<Option<T>> {
        let url = format!("http://{}/v1{}", peer, path);

        let mut request = match method {
            "GET" => self.client.get(&url),
            "POST" => self.client.post(&url),
            _ => return Err(eyre::eyre!("Unsupported HTTP method: {}", method)),
        };

        if let Some(body) = body {
            request = request.json(body);
        }

        let response = request.send().await?;
        let status = response.status();

        match status {
            StatusCode::OK => {
                if response.content_length().unwrap_or(0) == 0 {
                    return Ok(None);
                }
                let body = response.json::<T>().await?;
                Ok(Some(body))
            }
            StatusCode::NOT_FOUND => Ok(None),
            _ => {
                let error_text = response.text().await.unwrap_or_default();
                error!(
                    "API request failed with status: {}, message: {}",
                    status, error_text
                );
                Err(eyre::eyre!(
                    "API request failed with status: {} - {}",
                    status,
                    error_text
                ))
            }
        }
    }
}

#[async_trait::async_trait]
impl ApiClient for IrysApiClient {
    async fn get_transaction(
        &self,
        peer: SocketAddr,
        tx_id: H256,
    ) -> Result<Option<IrysTransactionHeader>> {
        // IMPORTANT: You have to keep the debug format here, since normal to_string of H256
        //  encodes just first 4 and last 4 bytes with a placeholder in the middle
        let path = format!("/tx/{}", tx_id.0.to_base58());
        self.make_request(peer, "GET", &path, None::<&()>).await
    }

    async fn post_transaction(
        &self,
        peer: SocketAddr,
        transaction: IrysTransactionHeader,
    ) -> Result<()> {
        let path = "/tx";
        let response = self
            .make_request::<(), _>(peer, "POST", path, Some(&transaction))
            .await;

        match response {
            Ok(_) => Ok(()),
            Err(e) => {
                error!("Failed to post transaction: {}", e);
                Err(e)
            }
        }
    }

    async fn get_transactions(
        &self,
        peer: SocketAddr,
        tx_ids: &[H256],
    ) -> Result<Vec<Option<IrysTransactionHeader>>> {
        let mut results = Vec::with_capacity(tx_ids.len());

        for &tx_id in tx_ids {
            let result = self.get_transaction(peer, tx_id).await?;
            results.push(result);
        }

        Ok(results)
    }

    async fn post_version(
        &self,
        peer: SocketAddr,
        version: VersionRequest,
    ) -> Result<PeerResponse> {
        let path = "/version";
        let response = self
            .make_request::<PeerResponse, _>(peer, "POST", path, Some(&version))
            .await;
        match response {
            Ok(Some(peer_response)) => Ok(peer_response),
            Ok(None) => Err(eyre::eyre!("No response from peer")),
            Err(e) => {
                error!("Failed to post version: {}", e);
                Err(e)
            }
        }
    }

    async fn get_block_by_hash(&self, peer: SocketAddr, block_hash: H256) -> Result<Option<CombinedBlockHeader>> {
        let path = format!("/block/{}", block_hash.0.to_base58());
        let response = self.make_request::<CombinedBlockHeader, _>(peer, "GET", &path, None::<&()>).await;
        match response {
            Ok(Some(block)) => Ok(Some(block)),
            Ok(None) => Ok(None),
            Err(e) => {
                error!("Failed to get block: {}", e);
                Err(e)
            }
        }
    }
}

#[cfg(feature = "test-utils")]
pub mod test_utils {
    use super::*;
    use async_trait::async_trait;
    use irys_types::AcceptedResponse;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[derive(Default, Clone)]
    pub struct CountingMockClient {
        pub post_version_calls: Arc<Mutex<Vec<std::net::SocketAddr>>>,
    }

    #[async_trait]
    impl ApiClient for CountingMockClient {
        async fn get_transaction(
            &self,
            _peer: std::net::SocketAddr,
            _tx_id: H256,
        ) -> eyre::Result<Option<IrysTransactionHeader>> {
            Ok(None)
        }
        async fn get_transactions(
            &self,
            _peer: std::net::SocketAddr,
            _tx_ids: &[H256],
        ) -> eyre::Result<Vec<Option<IrysTransactionHeader>>> {
            Ok(vec![])
        }
        async fn post_version(
            &self,
            peer: std::net::SocketAddr,
            _version: VersionRequest,
        ) -> eyre::Result<PeerResponse> {
            let mut calls = self.post_version_calls.lock().await;
            calls.push(peer);
            Ok(PeerResponse::Accepted(AcceptedResponse::default()))
        }

        async fn post_transaction(
            &self,
            _peer: std::net::SocketAddr,
            _transaction: IrysTransactionHeader,
        ) -> eyre::Result<()> {
            Ok(())
        }
        
        async fn get_block_by_hash(&self, _peer: std::net::SocketAddr, _block_hash: H256) -> eyre::Result<Option<CombinedBlockHeader>> {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::AcceptedResponse;

    /// Mock implementation of the API client for testing
    #[derive(Default, Clone)]
    pub struct MockApiClient {
        pub expected_transactions: std::collections::HashMap<H256, Option<IrysTransactionHeader>>,
    }

    #[async_trait::async_trait]
    impl ApiClient for MockApiClient {
        async fn get_transaction(
            &self,
            _peer: SocketAddr,
            tx_id: H256,
        ) -> Result<Option<IrysTransactionHeader>> {
            Ok(self.expected_transactions.get(&tx_id).cloned().flatten())
        }

        async fn get_transactions(
            &self,
            peer: SocketAddr,
            tx_ids: &[H256],
        ) -> Result<Vec<Option<IrysTransactionHeader>>> {
            let mut results = Vec::with_capacity(tx_ids.len());

            for &tx_id in tx_ids {
                let result = self.get_transaction(peer, tx_id).await?;
                results.push(result);
            }

            Ok(results)
        }

        async fn post_version(
            &self,
            _peer: SocketAddr,
            _version: VersionRequest,
        ) -> Result<PeerResponse> {
            Ok(PeerResponse::Accepted(AcceptedResponse::default())) // Mock response
        }

        async fn post_transaction(
            &self,
            _peer: SocketAddr,
            _tx: IrysTransactionHeader,
        ) -> Result<()> {
            Ok(())
        }

        async fn get_block_by_hash(&self, _peer: SocketAddr, _block_hash: H256) -> Result<Option<CombinedBlockHeader>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn test_mock_client() {
        let mut mock = MockApiClient::default();
        let tx_id = H256::random();
        let tx_header = IrysTransactionHeader::default();
        mock.expected_transactions
            .insert(tx_id, Some(tx_header.clone()));

        let result = mock
            .get_transaction("127.0.0.1:8080".parse().unwrap(), tx_id)
            .await
            .unwrap();
        assert_eq!(result, Some(tx_header));
    }
}
