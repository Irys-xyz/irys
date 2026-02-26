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
/// [`IrysApiClient::with_base_url`] to override the base URL — this is
/// necessary for HTTPS endpoints where the original hostname must be
/// preserved
#[derive(Clone, Debug)]
pub struct IrysApiClient {
    pub client: Client,
    /// When set, used as the URL prefix instead of `http://{peer}/v1`.
    base_url: Option<String>,
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
            base_url: None,
        }
    }

    /// Create a client that sends requests to the given base URL
    /// (e.g. `https://testnet-rpc.irys.xyz/v1`) instead of deriving
    /// it from the peer `SocketAddr`.
    pub fn with_base_url(base_url: String) -> Self {
        let base_url = base_url.strip_suffix('/').unwrap_or(&base_url).to_string();
        Self {
            client: Client::builder()
                .timeout(CLIENT_TIMEOUT)
                .read_timeout(CLIENT_TIMEOUT)
                .build()
                .unwrap(),
            base_url: Some(base_url),
        }
    }

    async fn make_request<RESBODY: DeserializeOwned, REQBODY: Serialize>(
        &self,
        peer: SocketAddr,
        method: Method,
        path: &str,
        body: Option<&REQBODY>,
    ) -> Result<Option<RESBODY>> {
        let url = match &self.base_url {
            Some(base) => format!("{}{}", base, path),
            None => format!("http://{}/v1{}", peer, path),
        };

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
}

#[async_trait::async_trait]
impl ApiClient for IrysApiClient {
    async fn get_transaction(
        &self,
        peer: SocketAddr,
        tx_id: H256,
    ) -> Result<IrysTransactionResponse> {
        // H256 Display prints full base58; using Display here is correct.
        let path = format!("/tx/{}", tx_id);
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

    async fn post_commitment_transaction(
        &self,
        peer: SocketAddr,
        transaction: CommitmentTransaction,
    ) -> Result<()> {
        let path = "/commitment-tx";
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

    async fn get_block_by_hash(
        &self,
        peer: SocketAddr,
        block_hash: H256,
        with_poa: bool,
    ) -> Result<Option<CombinedBlockHeader>> {
        let path = if with_poa {
            format!("/block/{}/full", block_hash)
        } else {
            format!("/block/{}", block_hash)
        };

        self.make_request::<CombinedBlockHeader, _>(peer, Method::GET, &path, None::<&()>)
            .await
    }

    async fn get_block_by_height(
        &self,
        peer: SocketAddr,
        block_height: u64,
        with_poa: bool,
    ) -> Result<Option<CombinedBlockHeader>> {
        let path = if with_poa {
            format!("/block/{}/full", block_height)
        } else {
            format!("/block/{}", block_height)
        };

        self.make_request::<CombinedBlockHeader, _>(peer, Method::GET, &path, None::<&()>)
            .await
    }

    async fn get_latest_block(
        &self,
        peer: SocketAddr,
        with_poa: bool,
    ) -> Result<Option<CombinedBlockHeader>> {
        let path = if with_poa {
            "/block/latest/full"
        } else {
            "/block/latest"
        };

        self.make_request::<CombinedBlockHeader, _>(peer, Method::GET, path, None::<&()>)
            .await
    }

    async fn get_block_index(
        &self,
        peer: SocketAddr,
        block_index_query: BlockIndexQuery,
    ) -> Result<Vec<BlockIndexItem>> {
        let path = format!(
            "/block-index?height={}&limit={}",
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

    async fn get_transaction_status(
        &self,
        peer: SocketAddr,
        tx_id: H256,
    ) -> Result<Option<TransactionStatusResponse>> {
        // H256 Display prints full base58; using Display here is correct.
        let path = format!("/tx/{}/status", tx_id);
        self.make_request::<TransactionStatusResponse, _>(peer, Method::GET, &path, None::<&()>)
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
