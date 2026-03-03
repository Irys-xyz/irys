use irys_types::{LedgerChunkOffset, PackedChunk};
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
    NotFound { offset: LedgerChunkOffset },
    ServerError { message: String },
    NetworkError { message: String },
    Timeout,
    InvalidResponse { message: String },
}

impl std::fmt::Display for ChunkFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { offset } => {
                write!(f, "Chunk not found: offset {}", offset)
            }
            Self::ServerError { message } => {
                write!(f, "Server error: {}", message)
            }
            Self::NetworkError { message } => {
                write!(f, "Network error: {}", message)
            }
            Self::Timeout => {
                write!(f, "Request timed out")
            }
            Self::InvalidResponse { message } => {
                write!(f, "Invalid response: {}", message)
            }
        }
    }
}

impl std::error::Error for ChunkFetchError {}

// Simplified trait - no ledger_id parameter
#[async_trait::async_trait]
pub trait ChunkFetcher: Send + Sync + std::fmt::Debug {
    async fn fetch_chunk(
        &self,
        ledger_chunk_offset: LedgerChunkOffset,
        api_addr: SocketAddr,
        timeout: Duration,
    ) -> Result<PackedChunk, ChunkFetchError>;
}

#[derive(Debug)]
pub struct HttpChunkFetcher {
    client: reqwest::Client,
    ledger_id: u32, // Store ledger_id in the fetcher
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
}

#[async_trait::async_trait]
impl ChunkFetcher for HttpChunkFetcher {
    async fn fetch_chunk(
        &self,
        ledger_chunk_offset: LedgerChunkOffset,
        api_addr: SocketAddr,
        timeout: Duration,
    ) -> Result<PackedChunk, ChunkFetchError> {
        let url = format!(
            "http://{}/v1/chunk/ledger/{}/{}",
            api_addr,
            self.ledger_id,
            ledger_chunk_offset // Use stored ledger_id
        );

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
            reqwest::StatusCode::OK => match response.json().await {
                Ok(chunk) => Ok(chunk),
                Err(e) => Err(ChunkFetchError::InvalidResponse {
                    message: format!("JSON parsing failed: {}", e),
                }),
            },
            reqwest::StatusCode::NOT_FOUND => Err(ChunkFetchError::NotFound {
                offset: ledger_chunk_offset,
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
                message: format!("Unexpected status code: {}", status),
            }),
        }
    }
}

// Mock implementation
#[derive(Debug)]
pub struct MockChunkFetcher {
    pub ledger_id: usize, // Store ledger_id in the mock too
    pub responses: Arc<RwLock<HashMap<LedgerChunkOffset, Result<PackedChunk, ChunkFetchError>>>>, // Simplified key
    pub request_log: Arc<RwLock<Vec<(LedgerChunkOffset, SocketAddr)>>>, // Simplified log entry
}

#[async_trait::async_trait]
impl ChunkFetcher for MockChunkFetcher {
    async fn fetch_chunk(
        &self,
        ledger_chunk_offset: LedgerChunkOffset,
        api_addr: SocketAddr,
        _timeout: Duration,
    ) -> Result<PackedChunk, ChunkFetchError> {
        // Log the request (simpler without ledger_id)
        self.request_log
            .write()
            .unwrap()
            .push((ledger_chunk_offset, api_addr));

        // Return pre-configured response (simpler key)
        let responses = self.responses.read().unwrap();

        match responses.get(&ledger_chunk_offset) {
            Some(Ok(chunk)) => Ok(chunk.clone()),
            Some(Err(e)) => Err(e.clone()),
            None => Err(ChunkFetchError::NotFound {
                offset: ledger_chunk_offset,
            }),
        }
    }
}

// Updated builder methods
impl MockChunkFetcher {
    pub fn new(ledger_id: usize) -> Self {
        Self {
            ledger_id,
            responses: Default::default(),
            request_log: Default::default(),
        }
    }

    pub fn with_chunk(self, offset: LedgerChunkOffset, chunk: PackedChunk) -> Self {
        self.responses.write().unwrap().insert(offset, Ok(chunk));
        self
    }

    pub fn with_not_found(self, offset: LedgerChunkOffset) -> Self {
        self.responses
            .write()
            .unwrap()
            .insert(offset, Err(ChunkFetchError::NotFound { offset }));
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
