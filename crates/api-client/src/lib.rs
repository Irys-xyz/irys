use eyre::Result;
use irys_types::{
    BlockIndexItem, BlockIndexQuery, CombinedBlockHeader, CommitmentTransaction,
    DataTransactionHeader, IrysTransactionResponse, NodeInfo, H256,
};
pub use reqwest::{Client, StatusCode};
use serde::{de::DeserializeOwned, Serialize};
use std::net::SocketAddr;
use std::time::Duration;

pub mod ext;
pub use ext::ApiClientExt;

pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

pub fn peer_base_url(peer: SocketAddr) -> String {
    format!("http://{}/v1", peer)
}

pub enum Method {
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

    /// Post a commitment transaction to a node
    async fn post_commitment_transaction(
        &self,
        peer: SocketAddr,
        transaction: CommitmentTransaction,
    ) -> Result<()>;

    /// Fetch multiple transaction headers by their IDs from a peer
    async fn get_transactions(
        &self,
        peer: SocketAddr,
        tx_ids: &[H256],
    ) -> Result<Vec<IrysTransactionResponse>>;

    /// Gets block by hash
    async fn get_block_by_hash(
        &self,
        peer: SocketAddr,
        block_hash: H256,
        with_poa: bool,
    ) -> Result<Option<CombinedBlockHeader>>;

    /// Gets block by height
    async fn get_block_by_height(
        &self,
        peer: SocketAddr,
        block_height: u64,
        with_poa: bool,
    ) -> Result<Option<CombinedBlockHeader>>;

    async fn get_latest_block(
        &self,
        peer: SocketAddr,
        with_poa: bool,
    ) -> Result<Option<CombinedBlockHeader>>;

    async fn get_block_index(
        &self,
        peer: SocketAddr,
        block_index_query: BlockIndexQuery,
    ) -> Result<Vec<BlockIndexItem>>;

    async fn node_info(&self, peer: SocketAddr) -> Result<NodeInfo>;

    /// Get the transaction status (PENDING, INCLUDED, or CONFIRMED)
    async fn get_transaction_status(
        &self,
        peer: SocketAddr,
        tx_id: H256,
    ) -> Result<Option<TransactionStatusResponse>>;
}

pub use irys_types::{TransactionStatus, TransactionStatusResponse};

/// Real implementation of the API client that makes actual HTTP requests.
///
/// By default, requests are sent as `http://{peer}/v1{path}`. Use
/// [`IrysApiClient::make_request_url`] to override this pattern

#[derive(Clone, Debug)]
pub struct IrysApiClient {
    pub client: Client,
}

impl Default for IrysApiClient {
    fn default() -> Self {
        Self::new()
    }
}

impl IrysApiClient {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(CLIENT_TIMEOUT)
                .read_timeout(CLIENT_TIMEOUT)
                .build()
                .unwrap(),
        }
    }

    async fn make_request<RESBODY: DeserializeOwned, REQBODY: Serialize>(
        &self,
        peer: SocketAddr,
        method: Method,
        path: &str,
        body: Option<&REQBODY>,
    ) -> Result<Option<RESBODY>> {
        self.make_request_url(&format!("{}{}", peer_base_url(peer), path), method, body)
            .await
    }

    pub async fn make_request_url<RESBODY: DeserializeOwned, REQBODY: Serialize>(
        &self,
        url: &str,
        method: Method,
        body: Option<&REQBODY>,
    ) -> Result<Option<RESBODY>> {
        let mut request = match method {
            Method::GET => self.client.get(url),
            Method::POST => self.client.post(url),
        };

        if let Some(body) = body {
            request = request.json(body);
        }

        let response = request.send().await?;
        let status = response.status();

        match status {
            StatusCode::OK => {
                let text = response.text().await?;
                if text.trim().is_empty() {
                    return Ok(None);
                }
                let body: RESBODY = serde_json::from_str(&text).map_err(|e| {
                    eyre::eyre!("{}: Failed to parse JSON: {} - Response: {}", url, e, text)
                })?;
                Ok(Some(body))
            }
            StatusCode::NOT_FOUND => Ok(None),
            _ => {
                let error_text = response.text().await.unwrap_or_default();
                Err(eyre::eyre!(
                    "API request {} failed with status: {} - {}",
                    url,
                    status,
                    error_text
                ))
            }
        }
    }

    pub async fn get_transaction_url(
        &self,
        url: &str,
        tx_id: H256,
    ) -> Result<IrysTransactionResponse> {
        let url = format!("{}/tx/{}", url, tx_id);
        self.make_request_url(&url, Method::GET, None::<&()>)
            .await?
            .ok_or_else(|| eyre::eyre!("Expected transaction response to have a body: {}", tx_id))
    }

    pub async fn post_transaction_url(
        &self,
        url: &str,
        transaction: DataTransactionHeader,
    ) -> Result<()> {
        let url = format!("{}/tx", url);
        self.make_request_url::<(), _>(&url, Method::POST, Some(&transaction))
            .await?;
        Ok(())
    }

    pub async fn post_commitment_transaction_url(
        &self,
        url: &str,
        transaction: CommitmentTransaction,
    ) -> Result<()> {
        let url = format!("{}/commitment-tx", url);
        self.make_request_url::<(), _>(&url, Method::POST, Some(&transaction))
            .await?;
        Ok(())
    }

    pub async fn get_transactions_url(
        &self,
        url: &str,
        tx_ids: &[H256],
    ) -> Result<Vec<IrysTransactionResponse>> {
        let mut results = Vec::with_capacity(tx_ids.len());
        for &tx_id in tx_ids {
            let result = self.get_transaction_url(url, tx_id).await?;
            results.push(result);
        }
        Ok(results)
    }

    pub async fn get_block_by_hash_url(
        &self,
        url: &str,
        block_hash: H256,
        with_poa: bool,
    ) -> Result<Option<CombinedBlockHeader>> {
        let url = if with_poa {
            format!("{}/block/{}/full", url, block_hash)
        } else {
            format!("{}/block/{}", url, block_hash)
        };
        self.make_request_url::<CombinedBlockHeader, _>(&url, Method::GET, None::<&()>)
            .await
    }

    pub async fn get_block_by_height_url(
        &self,
        url: &str,
        block_height: u64,
        with_poa: bool,
    ) -> Result<Option<CombinedBlockHeader>> {
        let url = if with_poa {
            format!("{}/block/{}/full", url, block_height)
        } else {
            format!("{}/block/{}", url, block_height)
        };
        self.make_request_url::<CombinedBlockHeader, _>(&url, Method::GET, None::<&()>)
            .await
    }

    pub async fn get_latest_block_url(
        &self,
        url: &str,
        with_poa: bool,
    ) -> Result<Option<CombinedBlockHeader>> {
        let url = if with_poa {
            format!("{}/block/latest/full", url)
        } else {
            format!("{}/block/latest", url)
        };
        self.make_request_url::<CombinedBlockHeader, _>(&url, Method::GET, None::<&()>)
            .await
    }

    pub async fn get_block_index_url(
        &self,
        url: &str,
        block_index_query: BlockIndexQuery,
    ) -> Result<Vec<BlockIndexItem>> {
        let url = format!(
            "{}/block-index?height={}&limit={}",
            url, block_index_query.height, block_index_query.limit
        );
        let response = self
            .make_request_url::<Vec<BlockIndexItem>, _>(&url, Method::GET, Some(&block_index_query))
            .await;
        match response {
            Ok(Some(block_index)) => Ok(block_index),
            Ok(None) => Ok(vec![]),
            Err(e) => Err(e),
        }
    }

    pub async fn node_info_url(&self, url: &str) -> Result<NodeInfo> {
        let url = format!("{}/info", url);
        let response = self
            .make_request_url::<NodeInfo, _>(&url, Method::GET, Some(&()))
            .await;
        match response {
            Ok(Some(node_info)) => Ok(node_info),
            Ok(None) => Err(eyre::eyre!("No response from peer")),
            Err(e) => Err(e),
        }
    }

    pub async fn get_transaction_status_url(
        &self,
        url: &str,
        tx_id: H256,
    ) -> Result<Option<TransactionStatusResponse>> {
        let url = format!("{}/tx/{}/status", url, tx_id);
        self.make_request_url::<TransactionStatusResponse, _>(&url, Method::GET, None::<&()>)
            .await
    }
}

#[async_trait::async_trait]
impl ApiClient for IrysApiClient {
    async fn get_transaction(
        &self,
        peer: SocketAddr,
        tx_id: H256,
    ) -> Result<IrysTransactionResponse> {
        self.get_transaction_url(&peer_base_url(peer), tx_id).await
    }

    async fn post_transaction(
        &self,
        peer: SocketAddr,
        transaction: DataTransactionHeader,
    ) -> Result<()> {
        self.post_transaction_url(&peer_base_url(peer), transaction)
            .await
    }

    async fn post_commitment_transaction(
        &self,
        peer: SocketAddr,
        transaction: CommitmentTransaction,
    ) -> Result<()> {
        self.post_commitment_transaction_url(&peer_base_url(peer), transaction)
            .await
    }

    async fn get_transactions(
        &self,
        peer: SocketAddr,
        tx_ids: &[H256],
    ) -> Result<Vec<IrysTransactionResponse>> {
        self.get_transactions_url(&peer_base_url(peer), tx_ids)
            .await
    }

    async fn get_block_by_hash(
        &self,
        peer: SocketAddr,
        block_hash: H256,
        with_poa: bool,
    ) -> Result<Option<CombinedBlockHeader>> {
        self.get_block_by_hash_url(&peer_base_url(peer), block_hash, with_poa)
            .await
    }

    async fn get_block_by_height(
        &self,
        peer: SocketAddr,
        block_height: u64,
        with_poa: bool,
    ) -> Result<Option<CombinedBlockHeader>> {
        self.get_block_by_height_url(&peer_base_url(peer), block_height, with_poa)
            .await
    }

    async fn get_latest_block(
        &self,
        peer: SocketAddr,
        with_poa: bool,
    ) -> Result<Option<CombinedBlockHeader>> {
        self.get_latest_block_url(&peer_base_url(peer), with_poa)
            .await
    }

    async fn get_block_index(
        &self,
        peer: SocketAddr,
        block_index_query: BlockIndexQuery,
    ) -> Result<Vec<BlockIndexItem>> {
        self.get_block_index_url(&peer_base_url(peer), block_index_query)
            .await
    }

    async fn node_info(&self, peer: SocketAddr) -> Result<NodeInfo> {
        self.node_info_url(&peer_base_url(peer)).await
    }

    async fn get_transaction_status(
        &self,
        peer: SocketAddr,
        tx_id: H256,
    ) -> Result<Option<TransactionStatusResponse>> {
        self.get_transaction_status_url(&peer_base_url(peer), tx_id)
            .await
    }
}

#[cfg(feature = "test-utils")]
pub mod test_utils {
    use super::*;
    use async_trait::async_trait;
    use eyre::eyre;

    #[derive(Default, Clone)]
    pub struct CountingMockClient {}

    #[async_trait]
    impl ApiClient for CountingMockClient {
        async fn get_transaction(
            &self,
            _peer: std::net::SocketAddr,
            _tx_id: H256,
        ) -> eyre::Result<IrysTransactionResponse> {
            Err(eyre!("No transactions found"))
        }
        async fn post_transaction(
            &self,
            _peer: std::net::SocketAddr,
            _transaction: DataTransactionHeader,
        ) -> eyre::Result<()> {
            Ok(())
        }

        async fn post_commitment_transaction(
            &self,
            _peer: SocketAddr,
            _transaction: CommitmentTransaction,
        ) -> Result<()> {
            Ok(())
        }

        async fn get_transactions(
            &self,
            _peer: std::net::SocketAddr,
            _tx_ids: &[H256],
        ) -> eyre::Result<Vec<IrysTransactionResponse>> {
            Ok(vec![])
        }

        async fn get_block_by_hash(
            &self,
            _peer: std::net::SocketAddr,
            _block_hash: H256,
            _with_poa: bool,
        ) -> eyre::Result<Option<CombinedBlockHeader>> {
            Ok(None)
        }

        async fn get_block_by_height(
            &self,
            _peer: SocketAddr,
            _block_height: u64,
            _with_poa: bool,
        ) -> Result<Option<CombinedBlockHeader>> {
            Ok(None)
        }

        async fn get_latest_block(
            &self,
            _peer: std::net::SocketAddr,
            _with_poa: bool,
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

        async fn get_transaction_status(
            &self,
            _peer: SocketAddr,
            _tx_id: H256,
        ) -> Result<Option<TransactionStatusResponse>> {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            _with_poa: bool,
        ) -> Result<Option<CombinedBlockHeader>> {
            Ok(None)
        }

        async fn get_latest_block(
            &self,
            _peer: SocketAddr,
            _with_poa: bool,
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

        async fn post_commitment_transaction(
            &self,
            _peer: SocketAddr,
            _transaction: CommitmentTransaction,
        ) -> Result<()> {
            Ok(())
        }

        async fn get_block_by_height(
            &self,
            _peer: SocketAddr,
            _block_height: u64,
            _with_poa: bool,
        ) -> Result<Option<CombinedBlockHeader>> {
            Ok(None)
        }

        async fn get_transaction_status(
            &self,
            _peer: SocketAddr,
            _tx_id: H256,
        ) -> Result<Option<TransactionStatusResponse>> {
            Ok(None)
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

        let result_header = match &result {
            IrysTransactionResponse::Storage(header) => header,
            _ => panic!("Expected storage transaction"),
        };
        assert!(result_header.eq_tx(&tx_header));
    }
}
