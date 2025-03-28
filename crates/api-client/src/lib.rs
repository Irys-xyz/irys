use reqwest::{Client, StatusCode};
use eyre::Result;
use irys_types::{IrysTransactionHeader, H256};
use serde::{de::DeserializeOwned, Serialize};
use std::net::SocketAddr;
use tracing::{debug, error};

/// Trait defining the interface for the API client
#[async_trait::async_trait]
pub trait ApiClient: Send + Sync {
    /// Fetch a transaction header by its ID from a peer
    async fn get_transaction(&self, peer: SocketAddr, tx_id: H256) -> Result<Option<IrysTransactionHeader>>;
    
    /// Fetch multiple transaction headers by their IDs from a peer
    async fn get_transactions(&self, peer: SocketAddr, tx_ids: &[H256]) -> Result<Vec<Option<IrysTransactionHeader>>>;
}

/// Real implementation of the API client that makes actual HTTP requests
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
        debug!("Making {} request to {}", method, url);

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
                let body = response.json::<T>().await?;
                Ok(Some(body))
            }
            StatusCode::NOT_FOUND => Ok(None),
            _ => {
                let error_text = response.text().await.unwrap_or_default();
                error!("API request failed with status: {}, message: {}", status, error_text);
                Err(eyre::eyre!("API request failed with status: {} - {}", status, error_text))
            }
        }
    }
}

#[async_trait::async_trait]
impl ApiClient for IrysApiClient {
    async fn get_transaction(&self, peer: SocketAddr, tx_id: H256) -> Result<Option<IrysTransactionHeader>> {
        debug!("Fetching transaction {} from peer {}", tx_id, peer);
        let path = format!("/tx/{}", tx_id);
        self.make_request(peer, "GET", &path, None::<&()>).await
    }

    async fn get_transactions(&self, peer: SocketAddr, tx_ids: &[H256]) -> Result<Vec<Option<IrysTransactionHeader>>> {
        debug!("Fetching {} transactions from peer {}", tx_ids.len(), peer);
        let mut results = Vec::with_capacity(tx_ids.len());
        
        for &tx_id in tx_ids {
            let result = self.get_transaction(peer, tx_id).await?;
            results.push(result);
        }
        
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockall::predicate::*;

    /// Mock implementation of the API client for testing
    #[derive(Default)]
    pub struct MockApiClient {
        pub expected_transactions: std::collections::HashMap<H256, Option<IrysTransactionHeader>>,
    }

    #[async_trait::async_trait]
    impl ApiClient for MockApiClient {
        async fn get_transaction(&self, _peer: SocketAddr, tx_id: H256) -> Result<Option<IrysTransactionHeader>> {
            Ok(self.expected_transactions.get(&tx_id).cloned().flatten())
        }

        async fn get_transactions(&self, peer: SocketAddr, tx_ids: &[H256]) -> Result<Vec<Option<IrysTransactionHeader>>> {
            let mut results = Vec::with_capacity(tx_ids.len());
            
            for &tx_id in tx_ids {
                let result = self.get_transaction(peer, tx_id).await?;
                results.push(result);
            }
            
            Ok(results)
        }
    }

    #[tokio::test]
    async fn test_mock_client() {
        let mut mock = MockApiClient::default();
        let tx_id = H256::random();
        let tx_header = IrysTransactionHeader::default();
        mock.expected_transactions.insert(tx_id, Some(tx_header.clone()));

        let result = mock.get_transaction("127.0.0.1:8080".parse().unwrap(), tx_id).await.unwrap();
        assert_eq!(result, Some(tx_header));
    }
}
