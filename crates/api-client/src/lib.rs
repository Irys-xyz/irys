use base58::ToBase58 as _;
use eyre::Result;
use irys_types::{
    BlockIndexItem, BlockIndexQuery, CombinedBlockHeader, DataTransactionHeader,
    IrysTransactionResponse, NodeInfo, PeerResponse, VersionRequest, H256,
};
use reqwest::{Client, StatusCode};
use serde::{de::DeserializeOwned, Serialize};
use std::net::SocketAddr;

#[expect(clippy::upper_case_acronyms, reason = "Canonical HTTP method names")]
enum Method {
    GET,
    POST,
}

/// Trait defining the interface for the API client
#[async_trait::async_trait]
pub trait ApiClient: Clone + Unpin + Default + Send + Sync + 'static {
    /// Fetch a transaction header by its ID from a peer
    async fn get_transaction(
        &self,
        peer: SocketAddr,
        tx_id: H256,
    ) -> Result<IrysTransactionResponse>;

    /// Post a transaction header to a node
    async fn post_transaction(
        &self,
        peer: SocketAddr,
        transaction: DataTransactionHeader,
    ) -> Result<()>;

    /// Fetch multiple transaction headers by their IDs from a peer
    async fn get_transactions(
        &self,
        peer: SocketAddr,
        tx_ids: &[H256],
    ) -> Result<Vec<IrysTransactionResponse>>;

    /// Post a version request to a peer. Version request contains protocol version and peer
    /// information.
    async fn post_version(&self, peer: SocketAddr, version: VersionRequest)
        -> Result<PeerResponse>;

    /// Gets block by hash
    async fn get_block_by_hash(
        &self,
        peer: SocketAddr,
        block_hash: H256,
    ) -> Result<Option<CombinedBlockHeader>>;

    async fn get_block_index(
        &self,
        peer: SocketAddr,
        block_index_query: BlockIndexQuery,
    ) -> Result<Vec<BlockIndexItem>>;

    async fn node_info(&self, peer: SocketAddr) -> Result<NodeInfo>;
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
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<Option<T>> {
        let url = format!("http://{}/v1{}", peer, path);

        let mut request = match method {
            Method::GET => self.client.get(&url),
            Method::POST => self.client.post(&url),
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
    ) -> Result<IrysTransactionResponse> {
        // IMPORTANT: You have to keep the debug format here, since normal to_string of H256
        //  encodes just first 4 and last 4 bytes with a placeholder in the middle
        let path = format!("/tx/{}", tx_id.0.to_base58());
        self.make_request(peer, Method::GET, &path, None::<&()>)
            .await?
            .ok_or_else(|| eyre::eyre!("Expected transaction response to have a body: {}", tx_id))
    }

    async fn post_transaction(
        &self,
        peer: SocketAddr,
        transaction: DataTransactionHeader,
    ) -> Result<()> {
        let path = "/tx";
        let response = self
            .make_request::<(), _>(peer, Method::POST, path, Some(&transaction))
            .await;

        match response {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn get_transactions(
        &self,
        peer: SocketAddr,
        tx_ids: &[H256],
    ) -> Result<Vec<IrysTransactionResponse>> {
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
            .make_request::<PeerResponse, _>(peer, Method::POST, path, Some(&version))
            .await;
        match response {
            Ok(Some(peer_response)) => Ok(peer_response),
            Ok(None) => Err(eyre::eyre!("No response from peer")),
            Err(e) => Err(e),
        }
    }

    async fn get_block_by_hash(
        &self,
        peer: SocketAddr,
        block_hash: H256,
    ) -> Result<Option<CombinedBlockHeader>> {
        let path = format!("/block/{}", block_hash.0.to_base58());

        self.make_request::<CombinedBlockHeader, _>(peer, Method::GET, &path, None::<&()>)
            .await
    }

    async fn get_block_index(
        &self,
        peer: SocketAddr,
        block_index_query: BlockIndexQuery,
    ) -> Result<Vec<BlockIndexItem>> {
        let path = format!(
            "/block_index?height={}&limit={}",
            block_index_query.height, block_index_query.limit
        );

        let response = self
            .make_request::<Vec<BlockIndexItem>, _>(
                peer,
                Method::GET,
                &path,
                Some(&block_index_query),
            )
            .await;
        match response {
            Ok(Some(block_index)) => Ok(block_index),
            Ok(None) => Ok(vec![]),
            Err(e) => Err(e),
        }
    }

    async fn node_info(&self, peer: SocketAddr) -> Result<NodeInfo> {
        let path = "/info";
        let response = self
            .make_request::<NodeInfo, _>(peer, Method::GET, path, Some(&()))
            .await;
        match response {
            Ok(Some(node_info)) => Ok(node_info),
            Ok(None) => Err(eyre::eyre!("No response from peer")),
            Err(e) => Err(e),
        }
    }
}

#[cfg(feature = "test-utils")]
pub mod test_utils {
    use super::*;
    use async_trait::async_trait;
    use eyre::eyre;
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
        ) -> eyre::Result<IrysTransactionResponse> {
            Err(eyre!("No transactions found"))
        }
        async fn get_transactions(
            &self,
            _peer: std::net::SocketAddr,
            _tx_ids: &[H256],
        ) -> eyre::Result<Vec<IrysTransactionResponse>> {
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
            _transaction: DataTransactionHeader,
        ) -> eyre::Result<()> {
            Ok(())
        }

        async fn get_block_by_hash(
            &self,
            _peer: std::net::SocketAddr,
            _block_hash: H256,
        ) -> eyre::Result<Option<CombinedBlockHeader>> {
            Ok(None)
        }

        async fn get_block_index(
            &self,
            _peer: SocketAddr,
            _block_index_query: BlockIndexQuery,
        ) -> eyre::Result<Vec<BlockIndexItem>> {
            Ok(vec![])
        }

        async fn node_info(&self, _peer: SocketAddr) -> eyre::Result<NodeInfo> {
            Ok(NodeInfo::default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::AcceptedResponse;

    /// Mock implementation of the API client for testing
    #[derive(Default, Clone)]
    pub(crate) struct MockApiClient {
        pub expected_transactions: std::collections::HashMap<H256, IrysTransactionResponse>,
    }

    #[async_trait::async_trait]
    impl ApiClient for MockApiClient {
        async fn get_transaction(
            &self,
            _peer: SocketAddr,
            tx_id: H256,
        ) -> Result<IrysTransactionResponse> {
            Ok(self
                .expected_transactions
                .get(&tx_id)
                .ok_or(eyre::eyre!("Transaction isn't found: {}", tx_id))?
                .clone())
        }

        async fn get_transactions(
            &self,
            peer: SocketAddr,
            tx_ids: &[H256],
        ) -> Result<Vec<IrysTransactionResponse>> {
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
            _tx: DataTransactionHeader,
        ) -> Result<()> {
            Ok(())
        }

        async fn get_block_by_hash(
            &self,
            _peer: SocketAddr,
            _block_hash: H256,
        ) -> Result<Option<CombinedBlockHeader>> {
            Ok(None)
        }

        async fn get_block_index(
            &self,
            _peer: SocketAddr,
            _block_index_query: BlockIndexQuery,
        ) -> Result<Vec<BlockIndexItem>> {
            Ok(vec![])
        }

        async fn node_info(&self, _peer: SocketAddr) -> Result<NodeInfo> {
            Ok(NodeInfo::default())
        }
    }

    #[tokio::test]
    async fn test_mock_client() {
        let mut mock = MockApiClient::default();
        let tx_id = H256::random();
        let tx_header = DataTransactionHeader::default();
        mock.expected_transactions
            .insert(tx_id, IrysTransactionResponse::Storage(tx_header.clone()));

        let result = mock
            .get_transaction("127.0.0.1:8080".parse().unwrap(), tx_id)
            .await
            .unwrap();
        assert_eq!(result, IrysTransactionResponse::Storage(tx_header));
    }
}
