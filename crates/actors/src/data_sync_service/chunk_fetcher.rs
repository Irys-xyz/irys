use irys_domain::ChunkType;
use irys_types::{
    ChunkFormat, DataLedger, DataRoot, LedgerChunkOffset, PackedChunk, TxChunkOffset,
};
use std::{
    collections::{BTreeSet, HashMap},
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::Duration,
};

/// Largest intervals body read from a peer. An older peer ignores the window
/// and sends its whole list; past this the answer is dropped and the peer's
/// coverage stays unknown, which means per-offset probing as before.
const MAX_INTERVALS_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Data intervals one assignee reported for its slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataIntervalsPage {
    /// Partition-relative, inclusive, ascending, touching runs merged.
    pub intervals: Vec<(u32, u32)>,
    pub coverage: IntervalsCoverage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntervalsCoverage {
    /// Complete from the requested start through `end`. `assigned` is false
    /// when the peer has no storage module for the slot.
    Window { end: u32, assigned: bool },
    /// The peer ignored the window and sent its whole list: an older build.
    /// An empty full list cannot be told apart from an unassigned peer.
    FullList,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireInterval {
    start: u32,
    end: u32,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireIntervalsResponse {
    ledger: DataLedger,
    slot_index: usize,
    chunk_type: ChunkType,
    intervals: Vec<WireInterval>,
    #[serde(default)]
    range: Option<WireInterval>,
    #[serde(default)]
    assigned: Option<bool>,
}

/// Parse and check one `/storage/intervals` body. The peer is not trusted:
/// the echo must match the request, runs must be ordered and inside the
/// window, and a windowed answer must start where it was asked to.
pub fn parse_data_intervals(
    body: &[u8],
    ledger: DataLedger,
    slot_index: usize,
    start: u32,
    end: u32,
) -> Result<DataIntervalsPage, ChunkFetchError> {
    let invalid = |message: String| ChunkFetchError::InvalidResponse { message };
    let wire: WireIntervalsResponse =
        serde_json::from_slice(body).map_err(|e| invalid(format!("intervals JSON: {e}")))?;
    if wire.ledger != ledger || wire.slot_index != slot_index || wire.chunk_type != ChunkType::Data
    {
        return Err(invalid(format!(
            "intervals answer is for {:?} slot {} {:?}",
            wire.ledger, wire.slot_index, wire.chunk_type
        )));
    }
    let coverage = match wire.range {
        Some(range) => {
            if range.start != start || range.end < start || range.end > end {
                return Err(invalid(format!(
                    "intervals range {}..={} does not answer {start}..={end}",
                    range.start, range.end
                )));
            }
            IntervalsCoverage::Window {
                end: range.end,
                assigned: wire.assigned.unwrap_or(false),
            }
        }
        None => IntervalsCoverage::FullList,
    };
    let (low, high) = match coverage {
        IntervalsCoverage::Window { end, .. } => (start, end),
        IntervalsCoverage::FullList => (0, u32::MAX),
    };
    let mut intervals: Vec<(u32, u32)> = Vec::with_capacity(wire.intervals.len());
    for run in wire.intervals {
        if run.start > run.end || run.start < low || run.end > high {
            return Err(invalid(format!(
                "interval {}..={} is outside {low}..={high}",
                run.start, run.end
            )));
        }
        match intervals.last_mut() {
            Some(last) if run.start <= last.1 => {
                return Err(invalid(format!(
                    "interval {}..={} is not above {}",
                    run.start, run.end, last.1
                )));
            }
            Some(last) if last.1.checked_add(1) == Some(run.start) => last.1 = run.end,
            _ => intervals.push((run.start, run.end)),
        }
    }
    Ok(DataIntervalsPage {
        intervals,
        coverage,
    })
}

/// Most entries one persisted-placement query may carry. The route rejects
/// more, and the client splits a slot's offsets into requests of this size.
pub const MAX_PERSISTED_QUERY_ENTRIES: usize = 4_096;

/// One chunk a custodian asks an assignee about: a partition-relative offset
/// in the assignee's slot, and the chunk expected there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedQueryEntry {
    pub offset: u32,
    pub data_root: DataRoot,
    pub tx_offset: TxChunkOffset,
}

/// Answer of `POST /v1/storage/persisted/{ledger}/{slot_index}`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedOffsetsResponse {
    pub ledger: DataLedger,
    pub slot_index: usize,
    /// Whether the answering node has a storage module for the slot.
    pub assigned: bool,
    /// Requested offsets whose placement of the requested chunk survives a
    /// crash on the answering node: persisted `Data` in `intervals.json`,
    /// not mid-write, and indexed as that `(data_root, tx_offset)`.
    pub persisted: Vec<u32>,
}

/// Parse and check one persisted-placement answer. The peer is not trusted:
/// the echo must match the request, every listed offset must have been
/// asked about, and an unassigned node attests nothing. Returns the
/// attested offsets, ascending and unique.
pub fn parse_persisted_offsets(
    body: &[u8],
    ledger: DataLedger,
    slot_index: usize,
    requested: &[PersistedQueryEntry],
) -> Result<Vec<u32>, ChunkFetchError> {
    let invalid = |message: String| ChunkFetchError::InvalidResponse { message };
    let wire: PersistedOffsetsResponse =
        serde_json::from_slice(body).map_err(|e| invalid(format!("persisted JSON: {e}")))?;
    if wire.ledger != ledger || wire.slot_index != slot_index {
        return Err(invalid(format!(
            "persisted answer is for {:?} slot {}",
            wire.ledger, wire.slot_index
        )));
    }
    let asked: BTreeSet<u32> = requested.iter().map(|entry| entry.offset).collect();
    let mut persisted = wire.persisted;
    persisted.sort_unstable();
    persisted.dedup();
    if let Some(stray) = persisted.iter().find(|offset| !asked.contains(offset)) {
        return Err(invalid(format!(
            "persisted answer lists offset {stray}, which was not asked"
        )));
    }
    if !wire.assigned && !persisted.is_empty() {
        return Err(invalid(
            "an unassigned node cannot attest a placement".to_string(),
        ));
    }
    Ok(persisted)
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkFetchFailureKind {
    NotFound,
    Timeout,
    Transport,
    Server,
    InvalidResponse,
}

impl ChunkFetchFailureKind {
    pub const fn as_metric_label(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::Server => "server",
            Self::InvalidResponse => "invalid_response",
        }
    }
}

impl ChunkFetchError {
    pub const fn kind(&self) -> ChunkFetchFailureKind {
        match self {
            Self::NotFound { .. } => ChunkFetchFailureKind::NotFound,
            Self::Timeout => ChunkFetchFailureKind::Timeout,
            Self::NetworkError { .. } => ChunkFetchFailureKind::Transport,
            Self::ServerError { .. } => ChunkFetchFailureKind::Server,
            Self::InvalidResponse { .. } => ChunkFetchFailureKind::InvalidResponse,
        }
    }
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

/// Read a peer HTTP body up to `cap` bytes.
///
/// `Content-Length` above the cap is rejected before the body is buffered.
/// A body with no length is stopped at the first chunk that would pass the cap.
/// This is a memory guard for the whole response. It does not limit any one
/// JSON field.
pub(crate) async fn read_capped_bytes(
    mut response: reqwest::Response,
    cap: u64,
) -> Result<Vec<u8>, String> {
    if let Some(len) = response.content_length()
        && len > cap
    {
        return Err(format!(
            "peer response Content-Length {len} exceeds cap of {cap} bytes"
        ));
    }
    let mut buf = Vec::new();
    let mut total = 0_u64;
    loop {
        let next = response.chunk().await.map_err(|err| err.to_string())?;
        let Some(chunk) = next else {
            break;
        };
        let chunk_len = u64::try_from(chunk.len())
            .map_err(|_| "peer response chunk length does not fit in u64".to_string())?;
        total = irys_types::accumulate_response_bytes(total, chunk_len, cap)
            .map_err(|_| format!("peer response exceeds cap of {cap} bytes"))?;
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

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

    /// The Data intervals an assignee holds for `slot_index` in
    /// `start..=end`. A fetcher that cannot ask leaves every peer's coverage
    /// unknown, so data sync probes offsets one by one as before.
    async fn fetch_data_intervals(
        &self,
        _slot_index: usize,
        _start: u32,
        _end: u32,
        _api_addr: SocketAddr,
        _timeout: Duration,
    ) -> Result<DataIntervalsPage, ChunkFetchError> {
        Err(ChunkFetchError::NotFound {
            detail: "this fetcher does not read peer intervals".to_string(),
        })
    }
    /// Offsets of `entries` that the assignee at `api_addr` holds for its
    /// `slot_index` on this fetcher's ledger, each as a crash-safe placement
    /// of exactly the requested chunk. A fetcher that cannot ask attests
    /// nothing, which keeps the body that asked.
    async fn fetch_persisted_offsets(
        &self,
        _slot_index: usize,
        _entries: &[PersistedQueryEntry],
        _api_addr: SocketAddr,
        _timeout: Duration,
    ) -> Result<Vec<u32>, ChunkFetchError> {
        Err(ChunkFetchError::NotFound {
            detail: "this fetcher does not ask for persisted placements".to_string(),
        })
    }
}

#[derive(Debug)]
pub struct HttpChunkFetcher {
    client: reqwest::Client,
    ledger_id: u32,
    max_peer_response_bytes: u64,
}

impl HttpChunkFetcher {
    pub fn new(ledger_id: u32, max_peer_response_bytes: u64) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            ledger_id,
            max_peer_response_bytes,
        }
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

        let cap = self.max_peer_response_bytes;
        match response.status() {
            reqwest::StatusCode::OK => {
                let body = read_capped_bytes(response, cap)
                    .await
                    .map_err(|message| ChunkFetchError::InvalidResponse { message })?;
                match serde_json::from_slice::<ChunkFormat>(&body) {
                    Ok(chunk) => Ok(chunk),
                    Err(e) => Err(ChunkFetchError::InvalidResponse {
                        message: format!("JSON parsing failed: {e}"),
                    }),
                }
            }
            reqwest::StatusCode::NOT_FOUND => Err(ChunkFetchError::NotFound {
                detail: not_found_detail,
            }),
            // The error body is the same memory guard as a success body. A body
            // past the cap is an invalid response, not a server message.
            reqwest::StatusCode::INTERNAL_SERVER_ERROR => {
                match read_capped_bytes(response, cap).await {
                    Ok(body) => {
                        let message =
                            String::from_utf8(body).unwrap_or_else(|_| "Unknown error".to_string());
                        Err(ChunkFetchError::ServerError { message })
                    }
                    Err(message) => Err(ChunkFetchError::InvalidResponse { message }),
                }
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

    async fn fetch_data_intervals(
        &self,
        slot_index: usize,
        start: u32,
        end: u32,
        api_addr: SocketAddr,
        timeout: Duration,
    ) -> Result<DataIntervalsPage, ChunkFetchError> {
        let ledger =
            DataLedger::try_from(self.ledger_id).map_err(|e| ChunkFetchError::InvalidResponse {
                message: e.to_string(),
            })?;
        // The route takes the ledger's serde name, e.g. `ThirtyDay`.
        let url = format!(
            "http://{api_addr}/v1/storage/intervals/{ledger:?}/{slot_index}/Data?start={start}&end={end}"
        );
        let read =
            async {
                let mut response = self.client.get(&url).send().await.map_err(|e| {
                    ChunkFetchError::NetworkError {
                        message: e.to_string(),
                    }
                })?;
                match response.status() {
                    reqwest::StatusCode::OK => {}
                    reqwest::StatusCode::NOT_FOUND => {
                        return Err(ChunkFetchError::NotFound {
                            detail: "peer does not serve storage intervals".to_string(),
                        });
                    }
                    status => {
                        return Err(ChunkFetchError::ServerError {
                            message: format!("intervals status {status}"),
                        });
                    }
                }
                let too_large = || ChunkFetchError::InvalidResponse {
                    message: format!("intervals body exceeds {MAX_INTERVALS_BODY_BYTES} bytes"),
                };
                if response
                    .content_length()
                    .is_some_and(|length| length > MAX_INTERVALS_BODY_BYTES as u64)
                {
                    return Err(too_large());
                }
                let mut body = Vec::new();
                while let Some(piece) =
                    response
                        .chunk()
                        .await
                        .map_err(|e| ChunkFetchError::NetworkError {
                            message: e.to_string(),
                        })?
                {
                    if body.len() + piece.len() > MAX_INTERVALS_BODY_BYTES {
                        return Err(too_large());
                    }
                    body.extend_from_slice(&piece);
                }
                Ok(body)
            };
        let body = tokio::time::timeout(timeout, read)
            .await
            .map_err(|_| ChunkFetchError::Timeout)??;
        parse_data_intervals(&body, ledger, slot_index, start, end)
    }
    async fn fetch_persisted_offsets(
        &self,
        slot_index: usize,
        entries: &[PersistedQueryEntry],
        api_addr: SocketAddr,
        timeout: Duration,
    ) -> Result<Vec<u32>, ChunkFetchError> {
        let ledger =
            DataLedger::try_from(self.ledger_id).map_err(|e| ChunkFetchError::InvalidResponse {
                message: e.to_string(),
            })?;
        // The route takes the ledger's serde name, e.g. `ThirtyDay`.
        let invalid_url = || ChunkFetchError::InvalidResponse {
            message: "invalid persisted-placement URL".into(),
        };
        let mut url = reqwest::Url::parse("http://localhost/").map_err(|_| invalid_url())?;
        url.set_ip_host(api_addr.ip()).map_err(|()| invalid_url())?;
        url.set_port(Some(api_addr.port()))
            .map_err(|()| invalid_url())?;
        url.path_segments_mut()
            .map_err(|()| invalid_url())?
            .extend([
                "v1",
                "storage",
                "persisted",
                &format!("{ledger:?}"),
                &slot_index.to_string(),
            ]);
        let cap = self.max_peer_response_bytes;
        let read = async {
            let response = self
                .client
                .post(url)
                .json(entries)
                .send()
                .await
                .map_err(|e| ChunkFetchError::NetworkError {
                    message: e.to_string(),
                })?;
            match response.status() {
                reqwest::StatusCode::OK => {}
                reqwest::StatusCode::NOT_FOUND => {
                    return Err(ChunkFetchError::NotFound {
                        detail: "peer does not serve persisted placements".to_string(),
                    });
                }
                status => {
                    return Err(ChunkFetchError::ServerError {
                        message: format!("persisted status {status}"),
                    });
                }
            }
            read_capped_bytes(response, cap)
                .await
                .map_err(|message| ChunkFetchError::InvalidResponse { message })
        };
        let body = tokio::time::timeout(timeout, read)
            .await
            .map_err(|_| ChunkFetchError::Timeout)??;
        parse_persisted_offsets(&body, ledger, slot_index, entries)
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
    /// Answer per peer. A peer with no entry has no intervals route.
    pub interval_responses:
        Arc<RwLock<HashMap<SocketAddr, Result<DataIntervalsPage, ChunkFetchError>>>>,
    pub interval_request_log: Arc<RwLock<Vec<(usize, u32, u32, SocketAddr)>>>,
    /// Persisted-placement answer per peer: the offsets it would attest.
    /// A peer with no entry has no persisted route.
    pub persisted_responses: Arc<RwLock<HashMap<SocketAddr, Result<Vec<u32>, ChunkFetchError>>>>,
    /// Delay before a peer's persisted answer, to model a slow peer.
    pub persisted_delays: Arc<RwLock<HashMap<SocketAddr, Duration>>>,
    pub persisted_request_log: Arc<RwLock<Vec<(usize, Vec<PersistedQueryEntry>, SocketAddr)>>>,
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

    async fn fetch_data_intervals(
        &self,
        slot_index: usize,
        start: u32,
        end: u32,
        api_addr: SocketAddr,
        _timeout: Duration,
    ) -> Result<DataIntervalsPage, ChunkFetchError> {
        self.interval_request_log
            .write()
            .unwrap()
            .push((slot_index, start, end, api_addr));
        self.interval_responses
            .read()
            .unwrap()
            .get(&api_addr)
            .cloned()
            .unwrap_or_else(|| {
                Err(ChunkFetchError::NotFound {
                    detail: "no intervals route".to_string(),
                })
            })
    }
    async fn fetch_persisted_offsets(
        &self,
        slot_index: usize,
        entries: &[PersistedQueryEntry],
        api_addr: SocketAddr,
        _timeout: Duration,
    ) -> Result<Vec<u32>, ChunkFetchError> {
        self.persisted_request_log
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((slot_index, entries.to_vec(), api_addr));
        let delay = self
            .persisted_delays
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&api_addr)
            .copied();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        let asked: BTreeSet<u32> = entries.iter().map(|entry| entry.offset).collect();
        self.persisted_responses
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&api_addr)
            .cloned()
            .unwrap_or_else(|| {
                Err(ChunkFetchError::NotFound {
                    detail: "no persisted route".to_string(),
                })
            })
            .map(|offsets| {
                offsets
                    .into_iter()
                    .filter(|offset| asked.contains(offset))
                    .collect()
            })
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
            interval_responses: Default::default(),
            interval_request_log: Default::default(),
            persisted_responses: Default::default(),
            persisted_delays: Default::default(),
            persisted_request_log: Default::default(),
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

    pub fn with_timeout(self, offset: LedgerChunkOffset) -> Self {
        self.responses
            .write()
            .unwrap()
            .insert(offset, Err(ChunkFetchError::Timeout));
        self
    }
    /// `peer` attests every offset in `offsets` that it is asked about.
    pub fn with_persisted(self, peer: SocketAddr, offsets: Vec<u32>) -> Self {
        self.persisted_responses
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(peer, Ok(offsets));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(
        body: serde_json::Value,
        start: u32,
        end: u32,
    ) -> Result<DataIntervalsPage, ChunkFetchError> {
        parse_data_intervals(
            body.to_string().as_bytes(),
            DataLedger::ThirtyDay,
            0,
            start,
            end,
        )
    }

    fn body(intervals: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "ledger": "ThirtyDay",
            "slotIndex": 0,
            "chunkType": "Data",
            "intervals": intervals,
        })
    }

    /// Serve one canned HTTP response and hand back the request line.
    async fn serve_once(
        status: &'static str,
        body: String,
    ) -> (SocketAddr, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0_u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buf).await.unwrap();
                request.extend_from_slice(&buf[..n]);
            }
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string()
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn http_fetcher_asks_the_windowed_route_and_parses_the_answer() {
        let mut answer = body(serde_json::json!([{"start": 12, "end": 14}]));
        answer["range"] = serde_json::json!({"start": 10, "end": 20});
        answer["assigned"] = serde_json::json!(true);
        let (addr, request) = serve_once("200 OK", answer.to_string()).await;
        let fetcher = HttpChunkFetcher::new(
            DataLedger::ThirtyDay.into(),
            irys_types::DEFAULT_MAX_PEER_RESPONSE_BYTES,
        );

        let page = fetcher
            .fetch_data_intervals(0, 10, 20, addr, Duration::from_secs(5))
            .await
            .unwrap();

        assert_eq!(
            request.await.unwrap(),
            "GET /v1/storage/intervals/ThirtyDay/0/Data?start=10&end=20 HTTP/1.1"
        );
        assert_eq!(page.intervals, vec![(12, 14)]);
        assert_eq!(
            page.coverage,
            IntervalsCoverage::Window {
                end: 20,
                assigned: true
            }
        );
    }

    #[tokio::test]
    async fn http_fetcher_reports_a_missing_route_as_not_found() {
        let (addr, _request) = serve_once("404 Not Found", String::new()).await;
        let fetcher = HttpChunkFetcher::new(
            DataLedger::ThirtyDay.into(),
            irys_types::DEFAULT_MAX_PEER_RESPONSE_BYTES,
        );
        let result = fetcher
            .fetch_data_intervals(0, 10, 20, addr, Duration::from_secs(5))
            .await;
        assert!(matches!(result, Err(ChunkFetchError::NotFound { .. })));
    }

    #[test]
    fn an_answer_without_a_range_is_an_older_full_list() {
        let page = parse(body(serde_json::json!([{"start": 0, "end": 3}])), 10, 20).unwrap();
        assert_eq!(page.coverage, IntervalsCoverage::FullList);
        assert_eq!(page.intervals, vec![(0, 3)]);
    }

    #[test]
    fn a_windowed_answer_reports_how_far_it_is_complete() {
        let mut value =
            body(serde_json::json!([{"start": 10, "end": 12}, {"start": 13, "end": 15}]));
        value["range"] = serde_json::json!({"start": 10, "end": 18});
        value["assigned"] = serde_json::json!(true);
        let page = parse(value, 10, 20).unwrap();
        assert_eq!(
            page.coverage,
            IntervalsCoverage::Window {
                end: 18,
                assigned: true
            }
        );
        assert_eq!(page.intervals, vec![(10, 15)], "touching runs merge");
    }

    #[test]
    fn a_window_without_assigned_counts_as_unassigned() {
        let mut value = body(serde_json::json!([]));
        value["range"] = serde_json::json!({"start": 10, "end": 20});
        let page = parse(value, 10, 20).unwrap();
        assert_eq!(
            page.coverage,
            IntervalsCoverage::Window {
                end: 20,
                assigned: false
            }
        );
    }

    #[test]
    fn untrusted_answers_are_rejected() {
        let mut wrong_slot = body(serde_json::json!([]));
        wrong_slot["slotIndex"] = serde_json::json!(1);
        let mut wrong_type = body(serde_json::json!([]));
        wrong_type["chunkType"] = serde_json::json!("Entropy");
        let mut wrong_start = body(serde_json::json!([]));
        wrong_start["range"] = serde_json::json!({"start": 11, "end": 20});
        let mut past_end = body(serde_json::json!([]));
        past_end["range"] = serde_json::json!({"start": 10, "end": 21});
        let mut outside = body(serde_json::json!([{"start": 5, "end": 12}]));
        outside["range"] = serde_json::json!({"start": 10, "end": 20});
        let unordered = body(serde_json::json!([{"start": 8, "end": 9}, {"start": 2, "end": 3}]));
        let inverted = body(serde_json::json!([{"start": 9, "end": 8}]));
        for (name, value) in [
            ("wrong slot", wrong_slot),
            ("wrong chunk type", wrong_type),
            ("range starts elsewhere", wrong_start),
            ("range past the window", past_end),
            ("run outside the window", outside),
            ("runs out of order", unordered),
            ("inverted run", inverted),
        ] {
            assert!(
                matches!(
                    parse(value, 10, 20),
                    Err(ChunkFetchError::InvalidResponse { .. })
                ),
                "{name} must be rejected"
            );
        }
    }
    /// Serve one canned answer to a request that carries a body, and hand
    /// back the request line and the body. Reading the whole body first
    /// keeps the close from resetting the client mid-request.
    async fn serve_post_once(
        status: &'static str,
        answer: String,
    ) -> (SocketAddr, tokio::task::JoinHandle<(String, Vec<u8>)>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0_u8; 4096];
            let header_end = loop {
                let n = socket.read(&mut buf).await.unwrap();
                request.extend_from_slice(&buf[..n]);
                if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at + 4;
                }
            };
            let head = String::from_utf8_lossy(&request[..header_end]).to_string();
            let length: usize = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if name.eq_ignore_ascii_case("content-length") {
                        value.trim().parse().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            while request.len() < header_end + length {
                let n = socket.read(&mut buf).await.unwrap();
                request.extend_from_slice(&buf[..n]);
            }
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
                answer.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            let line = head.lines().next().unwrap_or_default().to_string();
            (line, request[header_end..header_end + length].to_vec())
        });
        (addr, handle)
    }

    fn query(offsets: &[u32]) -> Vec<PersistedQueryEntry> {
        offsets
            .iter()
            .map(|offset| PersistedQueryEntry {
                offset: *offset,
                data_root: DataRoot::repeat_byte(1),
                tx_offset: TxChunkOffset::from(*offset),
            })
            .collect()
    }

    fn persisted_answer(slot_index: usize, assigned: bool, persisted: &[u32]) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "ledger": "ThirtyDay",
            "slotIndex": slot_index,
            "assigned": assigned,
            "persisted": persisted,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn http_fetcher_posts_the_persisted_route_and_parses_the_answer() {
        let answer = String::from_utf8(persisted_answer(3, true, &[7, 5])).unwrap();
        let (addr, request) = serve_post_once("200 OK", answer).await;
        let fetcher = HttpChunkFetcher::new(
            DataLedger::ThirtyDay.into(),
            irys_types::DEFAULT_MAX_PEER_RESPONSE_BYTES,
        );
        let asked = query(&[5, 6, 7]);

        let persisted = fetcher
            .fetch_persisted_offsets(3, &asked, addr, Duration::from_secs(5))
            .await
            .unwrap();

        let (line, body) = request.await.unwrap();
        assert_eq!(line, "POST /v1/storage/persisted/ThirtyDay/3 HTTP/1.1");
        let sent: Vec<PersistedQueryEntry> = serde_json::from_slice(&body).unwrap();
        assert_eq!(sent, asked);
        assert_eq!(persisted, vec![5, 7], "ascending, only what was attested");
    }

    #[tokio::test]
    async fn http_fetcher_reports_a_missing_persisted_route_as_not_found() {
        let (addr, _request) = serve_post_once("404 Not Found", String::new()).await;
        let fetcher = HttpChunkFetcher::new(
            DataLedger::ThirtyDay.into(),
            irys_types::DEFAULT_MAX_PEER_RESPONSE_BYTES,
        );
        let result = fetcher
            .fetch_persisted_offsets(0, &query(&[1]), addr, Duration::from_secs(5))
            .await;
        assert!(matches!(result, Err(ChunkFetchError::NotFound { .. })));
    }

    #[test]
    fn persisted_answers_are_deduplicated_and_sorted() {
        let body = persisted_answer(0, true, &[9, 2, 9]);
        let persisted =
            parse_persisted_offsets(&body, DataLedger::ThirtyDay, 0, &query(&[2, 9])).unwrap();
        assert_eq!(persisted, vec![2, 9]);
    }

    #[test]
    fn untrusted_persisted_answers_are_rejected() {
        let asked = query(&[1, 2, 3]);
        let mut wrong_ledger: serde_json::Value =
            serde_json::from_slice(&persisted_answer(0, true, &[1])).unwrap();
        wrong_ledger["ledger"] = serde_json::json!("Submit");
        for (name, body) in [
            ("wrong ledger", serde_json::to_vec(&wrong_ledger).unwrap()),
            ("wrong slot", persisted_answer(1, true, &[1])),
            ("offset never asked", persisted_answer(0, true, &[1, 4])),
            ("unassigned yet attesting", persisted_answer(0, false, &[1])),
            ("not JSON", b"nope".to_vec()),
        ] {
            assert!(
                matches!(
                    parse_persisted_offsets(&body, DataLedger::ThirtyDay, 0, &asked),
                    Err(ChunkFetchError::InvalidResponse { .. })
                ),
                "{name} must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn mock_fetcher_answers_persisted_offsets_per_peer() {
        let attesting: SocketAddr = "127.0.0.1:9001".parse().unwrap();
        let silent: SocketAddr = "127.0.0.1:9002".parse().unwrap();
        let fetcher = MockChunkFetcher::new(
            usize::try_from(DataLedger::ThirtyDay).expect("ledger id fits usize"),
        )
        .with_persisted(attesting, vec![1, 3, 8]);

        let answered = fetcher
            .fetch_persisted_offsets(0, &query(&[1, 2, 3]), attesting, Duration::from_secs(1))
            .await
            .unwrap();
        let missing = fetcher
            .fetch_persisted_offsets(0, &query(&[1]), silent, Duration::from_secs(1))
            .await;

        assert_eq!(answered, vec![1, 3], "only offsets that were asked");
        assert!(matches!(missing, Err(ChunkFetchError::NotFound { .. })));
        assert_eq!(fetcher.persisted_request_log.read().unwrap().len(), 2);
    }
}

#[cfg(test)]
mod response_cap_tests {
    use super::{ChunkFetchError, HttpChunkFetcher};
    use crate::chunk_fetcher::ChunkFetcher as _;
    use irys_types::{
        Base64, ChunkFormat, ConsensusConfig, DEFAULT_MAX_PEER_RESPONSE_BYTES, H256,
        LedgerChunkOffset, TxChunkOffset, UnpackedChunk,
    };
    use std::{
        io::{Read as _, Write as _},
        net::TcpListener,
        time::Duration,
    };

    fn serve(body: Vec<u8>, with_content_length: bool) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0_u8; 8192];
            let _ = stream.read(&mut buf);
            let len_header = if with_content_length {
                format!("Content-Length: {}\r\n", body.len())
            } else {
                String::new()
            };
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{len_header}Connection: close\r\n\r\n"
            );
            stream.write_all(header.as_bytes()).expect("header");
            stream.write_all(&body).expect("body");
        });
        addr
    }

    fn chunk_json(byte_len: usize, data_path_len: usize) -> Vec<u8> {
        let chunk = ChunkFormat::Unpacked(UnpackedChunk {
            data_root: H256::zero(),
            data_size: u64::try_from(byte_len).expect("len"),
            data_path: Base64(vec![0xAB; data_path_len]),
            bytes: Base64(vec![0x11; byte_len]),
            tx_offset: TxChunkOffset::from(0_u32),
        });
        serde_json::to_vec(&chunk).expect("chunk json")
    }

    #[tokio::test]
    async fn response_above_the_cap_is_an_invalid_response() {
        let body = chunk_json(64, 64);
        assert!(body.len() > 128, "fixture must pass the cap");
        let addr = serve(body, false);
        let fetcher = HttpChunkFetcher::new(0, 128);
        let error = fetcher
            .fetch_chunk_by_ledger_offset(LedgerChunkOffset::from(0), addr, Duration::from_secs(2))
            .await
            .expect_err("a body over the cap is rejected");
        assert!(
            matches!(error, ChunkFetchError::InvalidResponse { .. }),
            "{error}"
        );
    }

    #[tokio::test]
    async fn full_chunk_with_a_large_data_path_decodes_under_the_cap() {
        let chunk_size = ConsensusConfig::CHUNK_SIZE as usize;
        let data_path_len = 8 * 1024;
        let body = chunk_json(chunk_size, data_path_len);
        assert!(
            u64::try_from(body.len()).expect("len") < DEFAULT_MAX_PEER_RESPONSE_BYTES,
            "honest chunk response is {} bytes",
            body.len()
        );
        let addr = serve(body, true);
        let fetcher = HttpChunkFetcher::new(0, DEFAULT_MAX_PEER_RESPONSE_BYTES);
        let chunk = fetcher
            .fetch_chunk_by_ledger_offset(LedgerChunkOffset::from(0), addr, Duration::from_secs(5))
            .await
            .expect("honest chunk decodes");
        let unpacked = chunk.as_unpacked().expect("unpacked chunk");
        assert_eq!(unpacked.bytes.0.len(), chunk_size);
        assert_eq!(unpacked.data_path.0.len(), data_path_len);
    }
}
