use irys_types::{ChunkFormat, DataRoot, LedgerChunkOffset, PackedChunk, TxChunkOffset};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::Duration,
};

// Define a factory function type
pub type ChunkFetcherFactory = Box<dyn Fn(u32) -> Arc<dyn ChunkFetcher> + Send + Sync>;

#[derive(Debug, Clone, PartialEq)]
pub enum ChunkFetchError {
    /// Peer did not have the chunk body (ledger-offset or data_root path).
    NotFound {
        detail: String,
    },
    ServerError {
        message: String,
    },
    NetworkError {
        message: String,
    },
    Timeout,
    InvalidResponse {
        message: String,
    },
}

impl std::fmt::Display for ChunkFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { detail } => write!(f, "Chunk not found: {detail}"),
            Self::ServerError { message } => write!(f, "Server error: {message}"),
            Self::NetworkError { message } => write!(f, "Network error: {message}"),
            Self::Timeout => write!(f, "Request timed out"),
            Self::InvalidResponse { message } => write!(f, "Invalid response: {message}"),
        }
    }
}

impl std::error::Error for ChunkFetchError {}

/// Dual fetch addressing for data_sync:
/// - **Ledger-offset** — assignees with a storage module for the slot
/// - **Data-root + tx_offset** — ingress-proof signers that may only hold cache
///   and cannot serve `/chunk/ledger/...`
///
/// Ingress-proof gossip is not chunk replication; the data-root path is how
/// residual holes recover bodies from proof generators.
#[async_trait::async_trait]
pub trait ChunkFetcher: Send + Sync + std::fmt::Debug {
    async fn fetch_chunk_by_ledger_offset(
        &self,
        ledger_chunk_offset: LedgerChunkOffset,
        api_addr: SocketAddr,
        timeout: Duration,
    ) -> Result<ChunkFormat, ChunkFetchError>;

    async fn fetch_chunk_by_data_root(
        &self,
        data_root: DataRoot,
        tx_offset: TxChunkOffset,
        api_addr: SocketAddr,
        timeout: Duration,
    ) -> Result<ChunkFormat, ChunkFetchError>;
}

#[derive(Debug)]
pub struct HttpChunkFetcher {
    client: reqwest::Client,
    ledger_id: u32,
}

impl HttpChunkFetcher {
    pub fn new(ledger_id: u32) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        Self { client, ledger_id }
    }

    pub fn with_config(
        ledger_id: u32,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .build()
            .expect("Failed to create HTTP client");

        Self { client, ledger_id }
    }

    pub fn with_client(ledger_id: u32, client: reqwest::Client) -> Self {
        Self { client, ledger_id }
    }

    async fn get_chunk_json(
        &self,
        url: String,
        timeout: Duration,
        not_found_detail: String,
    ) -> Result<ChunkFormat, ChunkFetchError> {
        let response = match tokio::time::timeout(timeout, self.client.get(&url).send()).await {
            Ok(Ok(response)) => response,
            Ok(Err(e)) => {
                return Err(ChunkFetchError::NetworkError {
                    message: e.to_string(),
                });
            }
            Err(_) => return Err(ChunkFetchError::Timeout),
        };

        match response.status() {
            reqwest::StatusCode::OK => match response.json::<ChunkFormat>().await {
                Ok(chunk) => Ok(chunk),
                Err(e) => Err(ChunkFetchError::InvalidResponse {
                    message: format!("JSON parsing failed: {e}"),
                }),
            },
            reqwest::StatusCode::NOT_FOUND => Err(ChunkFetchError::NotFound {
                detail: not_found_detail,
            }),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR => {
                let error_body = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unknown error".to_string());
                Err(ChunkFetchError::ServerError {
                    message: error_body,
                })
            }
            status => Err(ChunkFetchError::ServerError {
                message: format!("Unexpected status code: {status}"),
            }),
        }
    }
}

#[async_trait::async_trait]
impl ChunkFetcher for HttpChunkFetcher {
    async fn fetch_chunk_by_ledger_offset(
        &self,
        ledger_chunk_offset: LedgerChunkOffset,
        api_addr: SocketAddr,
        timeout: Duration,
    ) -> Result<ChunkFormat, ChunkFetchError> {
        let url = format!(
            "http://{}/v1/chunk/ledger/{}/{}",
            api_addr, self.ledger_id, ledger_chunk_offset
        );
        self.get_chunk_json(url, timeout, format!("ledger_offset={ledger_chunk_offset}"))
            .await
    }

    async fn fetch_chunk_by_data_root(
        &self,
        data_root: DataRoot,
        tx_offset: TxChunkOffset,
        api_addr: SocketAddr,
        timeout: Duration,
    ) -> Result<ChunkFormat, ChunkFetchError> {
        // Non-assignee proof generators serve from cache via this route (PR1).
        let url = format!(
            "http://{}/v1/chunk/data-root/{}/{}/{}",
            api_addr, self.ledger_id, data_root, *tx_offset
        );
        self.get_chunk_json(
            url,
            timeout,
            format!("data_root={data_root} tx_offset={tx_offset}"),
        )
        .await
    }
}

// Mock implementation
#[derive(Debug)]
pub struct MockChunkFetcher {
    pub ledger_id: usize,
    pub responses: Arc<RwLock<HashMap<LedgerChunkOffset, Result<PackedChunk, ChunkFetchError>>>>,
    pub data_root_responses:
        Arc<RwLock<HashMap<(DataRoot, u32), Result<ChunkFormat, ChunkFetchError>>>>,
    pub request_log: Arc<RwLock<Vec<(LedgerChunkOffset, SocketAddr)>>>,
    pub data_root_request_log: Arc<RwLock<Vec<(DataRoot, u32, SocketAddr)>>>,
}

#[async_trait::async_trait]
impl ChunkFetcher for MockChunkFetcher {
    async fn fetch_chunk_by_ledger_offset(
        &self,
        ledger_chunk_offset: LedgerChunkOffset,
        api_addr: SocketAddr,
        _timeout: Duration,
    ) -> Result<ChunkFormat, ChunkFetchError> {
        self.request_log
            .write()
            .unwrap()
            .push((ledger_chunk_offset, api_addr));

        let responses = self.responses.read().unwrap();
        match responses.get(&ledger_chunk_offset) {
            Some(Ok(chunk)) => Ok(ChunkFormat::Packed(chunk.clone())),
            Some(Err(e)) => Err(e.clone()),
            None => Err(ChunkFetchError::NotFound {
                detail: format!("ledger_offset={ledger_chunk_offset}"),
            }),
        }
    }

    async fn fetch_chunk_by_data_root(
        &self,
        data_root: DataRoot,
        tx_offset: TxChunkOffset,
        api_addr: SocketAddr,
        _timeout: Duration,
    ) -> Result<ChunkFormat, ChunkFetchError> {
        let key = (data_root, *tx_offset);
        self.data_root_request_log
            .write()
            .unwrap()
            .push((data_root, *tx_offset, api_addr));

        let responses = self.data_root_responses.read().unwrap();
        match responses.get(&key) {
            Some(Ok(chunk)) => Ok(chunk.clone()),
            Some(Err(e)) => Err(e.clone()),
            None => Err(ChunkFetchError::NotFound {
                detail: format!("data_root={data_root} tx_offset={tx_offset}"),
            }),
        }
    }
}

impl MockChunkFetcher {
    pub fn new(ledger_id: usize) -> Self {
        Self {
            ledger_id,
            responses: Default::default(),
            data_root_responses: Default::default(),
            request_log: Default::default(),
            data_root_request_log: Default::default(),
        }
    }

    pub fn with_chunk(self, offset: LedgerChunkOffset, chunk: PackedChunk) -> Self {
        self.responses.write().unwrap().insert(offset, Ok(chunk));
        self
    }

    pub fn with_data_root_chunk(
        self,
        data_root: DataRoot,
        tx_offset: TxChunkOffset,
        chunk: ChunkFormat,
    ) -> Self {
        self.data_root_responses
            .write()
            .unwrap()
            .insert((data_root, *tx_offset), Ok(chunk));
        self
    }

    pub fn with_not_found(self, offset: LedgerChunkOffset) -> Self {
        self.responses.write().unwrap().insert(
            offset,
            Err(ChunkFetchError::NotFound {
                detail: format!("ledger_offset={offset}"),
            }),
        );
        self
    }

    pub fn with_server_error(self, offset: LedgerChunkOffset, message: String) -> Self {
        self.responses
            .write()
            .unwrap()
            .insert(offset, Err(ChunkFetchError::ServerError { message }));
        self
    }

    pub fn with_timeout(self, offset: LedgerChunkOffset) -> Self {
        self.responses
            .write()
            .unwrap()
            .insert(offset, Err(ChunkFetchError::Timeout));
        self
    }

    pub fn ledger_id(&self) -> usize {
        self.ledger_id
    }
}
