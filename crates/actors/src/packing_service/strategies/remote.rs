use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use irys_domain::{EntropyRejection, EntropyWrite, StorageModule};
use irys_types::{
    Config, IrysAddress, PartitionChunkOffset, PartitionChunkRange, RemotePackingConfig, ii,
    partition::PartitionHash, partition_chunk_offset_ii, remote_packing::RemotePackingRequest,
};
use reth::revm::primitives::bytes::{Bytes, BytesMut};
use tokio::sync::Notify;
use tracing::{debug, error};

use crate::packing_service::{
    REMOTE_STREAM_BUFFER_MULTIPLIER, client_pool::HttpClientPool, config::PackingConfig,
};

/// Chunks one remote `/pack` request may cover. The next span is a new request.
const REMOTE_PACK_WINDOW_CHUNKS: u32 = 1024;
/// Stream budget, in configured timeouts. Not one timeout per chunk.
const REMOTE_STREAM_BUDGET_TIMEOUTS: u32 = 4;

/// Remote packing strategy that delegates to external services
pub(crate) struct RemotePackingStrategy {
    config: Arc<Config>,
    packing_config: PackingConfig,
    client_pool: HttpClientPool,
    notify: Arc<Notify>,
}

impl RemotePackingStrategy {
    pub(crate) fn new(
        config: Arc<Config>,
        packing_config: PackingConfig,
        notify: Arc<Notify>,
    ) -> Self {
        Self {
            config,
            packing_config,
            client_pool: HttpClientPool::new(),
            notify,
        }
    }

    /// Process a stream of packed chunks from a remote service
    #[tracing::instrument(level = "trace", skip_all, fields(storage_module.id = storage_module_id, chunk.range_start = range_start, chunk.range = ?current_chunk_range, partition.hash = %partition_hash))]
    async fn process_chunk_stream(
        &self,
        mut stream: impl futures::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
        storage_module: &Arc<StorageModule>,
        current_chunk_range: &PartitionChunkRange,
        mut range_start: u32,
        short_writes_before_sync: u32,
        storage_module_id: usize,
        partition_hash: PartitionHash,
        mining_address: IrysAddress,
        stream_budget: Duration,
    ) -> Result<u32, String> {
        let chunk_size = self.config.consensus.chunk_size as usize;
        // `read_timeout` resets after every read. This deadline does not.
        let stream_deadline = tokio::time::Instant::now() + stream_budget;
        let mut buffer = BytesMut::with_capacity(
            (self.config.consensus.chunk_size * REMOTE_STREAM_BUFFER_MULTIPLIER)
                .try_into()
                .unwrap(),
        );

        let notify = self.notify.clone();
        let range_end = *current_chunk_range.0.end();

        let mut process_chunk = |chunk_bytes: Bytes| -> Result<(), String> {
            if range_start > range_end {
                return Err(format!(
                    "remote packing produced a chunk past inclusive end {range_end}"
                ));
            }
            // The stream is positional. A Data or Entropy offset consumes its
            // bytes and is not written. A hash change fails the body.
            match storage_module.write_entropy_chunk(
                PartitionChunkOffset(range_start),
                chunk_bytes.to_vec(),
                partition_hash,
            ) {
                EntropyWrite::Queued
                | EntropyWrite::Rejected(EntropyRejection::NotUninitialized(_)) => {}
                EntropyWrite::Rejected(EntropyRejection::NotAssigned) => {
                    return Err(format!(
                        "entropy write refused at offset {range_start} for partition {partition_hash}"
                    ));
                }
            }
            notify.notify_waiters();

            crate::packing_service::log_packing_progress(
                "Remote",
                range_start,
                current_chunk_range,
                storage_module_id,
                &partition_hash,
                &mining_address,
            );

            if range_start.is_multiple_of(short_writes_before_sync) {
                debug!("triggering sync");
                crate::packing_service::sync_with_warning(
                    storage_module,
                    "remote packing (streaming)",
                );
                notify.notify_waiters();
            }

            range_start += 1;
            Ok(())
        };

        // Process streaming chunks. One item can hold many chunks, so the
        // ceiling is checked again before each write.
        loop {
            let Some(chunk_result) =
                within_stream_deadline(stream_deadline, stream_budget, stream.next()).await?
            else {
                break;
            };
            // Wait for the disk before taking this item's bytes into the module.
            wait_for_stream_room(storage_module, stream_deadline, stream_budget).await?;
            let chunk = chunk_result.map_err(|e| format!("Error getting chunk: {:?}", e))?;
            buffer.extend_from_slice(&chunk);

            while buffer.len() >= chunk_size {
                wait_for_stream_room(storage_module, stream_deadline, stream_budget).await?;
                let chunk_to_process = buffer.split_to(chunk_size).freeze();
                process_chunk(chunk_to_process)?;
            }
        }

        // A short tail is not a chunk. Writing it would store a partial
        // buffer as a full entropy chunk.
        if !buffer.is_empty() {
            return Err(format!(
                "remote packing stream ended with a short buffer of {} bytes; chunk size is {chunk_size}",
                buffer.len()
            ));
        }

        Ok(range_start)
    }

    /// Try packing with a specific remote service
    #[tracing::instrument(level = "trace", skip_all, fields(storage_module.id = storage_module_id, chunk.range_start = range_start, chunk.range_end = range_end, partition.hash = %partition_hash, remote.url = %remote.url))]
    async fn try_remote(
        &self,
        remote: &RemotePackingConfig,
        storage_module: &Arc<StorageModule>,
        range_start: u32,
        range_end: u32,
        mining_address: IrysAddress,
        partition_hash: PartitionHash,
        storage_module_id: usize,
        short_writes_before_sync: u32,
    ) -> Result<u32, String> {
        let current_chunk_range =
            PartitionChunkRange(partition_chunk_offset_ii!(range_start, range_end));

        // No timeout means no connect timeout and no read timeout. The
        // stream can then hold this controller until the process stops.
        let Some(timeout) = remote.timeout else {
            return Err(format!("remote packing host {} has no timeout", remote.url));
        };
        let url = &remote.url;
        let v1_url = format!("{url}/v1");

        let client = self
            .client_pool
            .get_or_create_client(&v1_url, Some(&timeout));

        // Check connectivity
        debug!("Attempting to connect to remote packing host {}", &v1_url);
        if let Err(e) = client.get(format!("{}/info", v1_url)).send().await {
            return Err(format!(
                "Unable to connect to remote packing host {} - {}",
                url, e
            ));
        }

        // Prepare request
        let request = RemotePackingRequest {
            mining_address,
            partition_hash,
            chunk_range: current_chunk_range,
            chain_id: self.packing_config.chain_id,
            chunk_size: self.config.consensus.chunk_size,
            entropy_packing_iterations: self.config.consensus.entropy_packing_iterations,
        };

        // Send packing request
        let response = client
            .post(format!("{}/pack", v1_url))
            .json(&request)
            .send()
            .await
            .map_err(|e| format!("Error sending packing request to {} - {}", v1_url, e))?;

        // An error status still has a body. Do not pack those bytes.
        let response = response.error_for_status().map_err(|e| {
            let status = match e.status() {
                Some(status) => status.to_string(),
                None => e.to_string(),
            };
            format!("Remote packing host {v1_url} returned HTTP {status}")
        })?;

        let stream = response.bytes_stream();

        // `read_timeout` resets after every byte. This budget does not. It is
        // a few configured timeouts, so a dribble cannot hold the controller
        // for one timeout per chunk. The request itself is at most one window.
        let chunks_in_range = u64::from(range_end.saturating_sub(range_start)).saturating_add(1);
        let budget_chunks = chunks_in_range.min(u64::from(REMOTE_STREAM_BUDGET_TIMEOUTS));
        let stream_budget =
            timeout.saturating_mul(u32::try_from(budget_chunks).unwrap_or(u32::MAX));

        // Process response stream
        self.process_chunk_stream(
            stream,
            storage_module,
            &current_chunk_range,
            range_start,
            short_writes_before_sync,
            storage_module_id,
            partition_hash,
            mining_address,
            stream_budget,
        )
        .await
    }
}

/// Stop a remote body that outlives its budget. The clock is monotonic.
async fn within_stream_deadline<T>(
    deadline: tokio::time::Instant,
    budget: Duration,
    future: impl std::future::Future<Output = T>,
) -> Result<T, String> {
    tokio::time::timeout_at(deadline, future)
        .await
        .map_err(|_| format!("remote packing stream exceeded monotonic deadline {budget:?}"))
}

/// Wait until the module can take another chunk, but not past the stream budget.
async fn wait_for_stream_room(
    storage_module: &StorageModule,
    deadline: tokio::time::Instant,
    budget: Duration,
) -> Result<(), String> {
    if storage_module.has_pending_room() {
        return Ok(());
    }
    within_stream_deadline(
        deadline,
        budget,
        crate::packing_service::wait_for_pending_room(storage_module, "remote packing (streaming)"),
    )
    .await?
}

#[async_trait]
impl super::PackingStrategy for RemotePackingStrategy {
    #[tracing::instrument(level = "trace", skip_all, fields(storage_module.id = storage_module_id, chunk.range_start = *chunk_range.0.start(), chunk.range_end = *chunk_range.0.end(), partition.hash = %partition_hash))]
    async fn pack(
        &self,
        storage_module: &Arc<StorageModule>,
        chunk_range: PartitionChunkRange,
        mining_address: IrysAddress,
        partition_hash: PartitionHash,
        storage_module_id: usize,
        short_writes_before_sync: u32,
    ) -> Result<(), String> {
        let range_start = *chunk_range.0.start();
        let range_end = *chunk_range.0.end();
        let mut window_start = range_start;

        // One request per window. A window that misses its budget returns Err
        // so the caller can pack locally. The next window is a new request.
        while window_start <= range_end {
            let window_end = window_start
                .saturating_add(REMOTE_PACK_WINDOW_CHUNKS - 1)
                .min(range_end);
            self.pack_window(
                storage_module,
                window_start,
                window_end,
                mining_address,
                partition_hash,
                storage_module_id,
                short_writes_before_sync,
            )
            .await?;
            window_start = match window_end.checked_add(1) {
                Some(next) => next,
                None => break,
            };
        }
        Ok(())
    }
}

impl RemotePackingStrategy {
    async fn pack_window(
        &self,
        storage_module: &Arc<StorageModule>,
        window_start: u32,
        window_end: u32,
        mining_address: IrysAddress,
        partition_hash: PartitionHash,
        storage_module_id: usize,
        short_writes_before_sync: u32,
    ) -> Result<(), String> {
        let mut current_start = window_start;

        // Try each remote in order
        for remote in &self.packing_config.remotes {
            match self
                .try_remote(
                    remote,
                    storage_module,
                    current_start,
                    window_end,
                    mining_address,
                    partition_hash,
                    storage_module_id,
                    short_writes_before_sync,
                )
                .await
            {
                Ok(new_start) => {
                    // Inclusive range is done only when the next offset is one
                    // past the end. `>= window_end` treats a stream that
                    // stopped on the last offset as success.
                    let Some(done) = window_end.checked_add(1) else {
                        return Err(format!(
                            "remote packing range end {window_end} has no following chunk"
                        ));
                    };
                    if new_start == done {
                        return Ok(());
                    }
                    if new_start > done {
                        return Err(format!(
                            "remote packing advanced to {new_start}, past inclusive end {window_end}"
                        ));
                    }
                    current_start = new_start;
                }
                Err(e) => {
                    error!(
                        "Remote packing failed for storage_module {} partition_hash {}: {}",
                        storage_module_id, partition_hash, e
                    );
                    continue;
                }
            }
        }

        Err("All remote packing services failed".to_string())
    }
}

#[cfg(test)]
mod short_stream_tests {
    use std::io::{Read as _, Write as _};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use irys_domain::{ChunkType, StorageModule, StorageModuleInfo};
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{
        ConsensusConfig, NodeConfig, PartitionChunkOffset, PartitionChunkRange, ii,
        partition::PartitionHash, partition_chunk_offset_ii,
    };
    use reth::revm::primitives::bytes::Bytes;
    use tokio::sync::Notify;

    use super::super::PackingStrategy as _;
    use super::RemotePackingStrategy;

    fn test_module(
        prefix: &str,
    ) -> (
        irys_testing_utils::tempfile::TempDir,
        Arc<irys_types::Config>,
        Arc<StorageModule>,
        PartitionHash,
    ) {
        test_module_with_ceiling(prefix, None)
    }

    fn test_module_with_chunks(
        prefix: &str,
        num_chunks: u64,
    ) -> (
        irys_testing_utils::tempfile::TempDir,
        Arc<irys_types::Config>,
        Arc<StorageModule>,
        PartitionHash,
    ) {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).build();
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = tmp_dir.path().to_path_buf();
        let consensus = node_config.consensus_config();
        node_config.consensus = irys_types::ConsensusOptions::Custom(ConsensusConfig {
            entropy_packing_iterations: 1,
            num_chunks_in_partition: num_chunks,
            chunk_size: 32,
            ..consensus
        });
        let config = Arc::new(irys_types::Config::new_with_random_peer_id(node_config));
        let partition_hash = PartitionHash::repeat_byte(0x3C);
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(irys_types::partition::PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(
                irys_types::partition_chunk_offset_ie!(0, num_chunks as u32),
                "hdd0".into(),
            )],
        };
        let module = Arc::new(StorageModule::new(&info, &config).expect("storage module"));
        (tmp_dir, config, module, partition_hash)
    }

    fn test_module_with_ceiling(
        prefix: &str,
        ceiling_bytes: Option<u64>,
    ) -> (
        irys_testing_utils::tempfile::TempDir,
        Arc<irys_types::Config>,
        Arc<StorageModule>,
        PartitionHash,
    ) {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).build();
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = tmp_dir.path().to_path_buf();
        let consensus = node_config.consensus_config();
        node_config.consensus = irys_types::ConsensusOptions::Custom(ConsensusConfig {
            entropy_packing_iterations: 1,
            num_chunks_in_partition: 4,
            chunk_size: 32,
            ..consensus
        });
        if let Some(ceiling) = ceiling_bytes {
            node_config.storage.max_pending_write_bytes = Some(ceiling);
        }
        let config = Arc::new(irys_types::Config::new_with_random_peer_id(node_config));
        let partition_hash = PartitionHash::repeat_byte(0x3C);
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(irys_types::partition::PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(irys_types::partition_chunk_offset_ie!(0, 4), "hdd0".into())],
        };
        let module = Arc::new(StorageModule::new(&info, &config).expect("storage module"));
        (tmp_dir, config, module, partition_hash)
    }

    fn bind_local(name: &'static str) -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").expect(name);
        listener.set_nonblocking(true).expect(name);
        let addr = listener.local_addr().expect(name);
        (listener, addr)
    }

    /// Nonblocking accept until `window` ends. A hard error stops the host.
    fn serve_for(listener: TcpListener, window: Duration, mut on_conn: impl FnMut(TcpStream)) {
        let deadline = std::time::Instant::now() + window;
        while std::time::Instant::now() < deadline {
            match listener.accept() {
                Ok((stream, _)) => on_conn(stream),
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    }

    /// One-shot HTTP/1.1 host. Counts `POST /v1/pack` requests.
    struct PackHost {
        addr: SocketAddr,
        posts: Arc<AtomicUsize>,
    }

    impl PackHost {
        fn spawn(status: u16, body: Vec<u8>) -> Self {
            Self::spawn_for(status, body, Duration::from_secs(5))
        }

        fn spawn_for(status: u16, body: Vec<u8>, window: Duration) -> Self {
            let (listener, addr) = bind_local("bind pack host");
            let posts = Arc::new(AtomicUsize::new(0));
            let posts_thread = posts.clone();
            std::thread::spawn(move || {
                serve_for(listener, window, move |stream| {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                    handle_conn(stream, status, &body, &posts_thread);
                });
            });
            Self { addr, posts }
        }
    }

    fn handle_conn(mut stream: TcpStream, status: u16, body: &[u8], posts: &AtomicUsize) {
        let mut buf = Vec::new();
        let mut tmp = [0_u8; 1024];
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if request_complete(&buf) {
                        break;
                    }
                }
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&buf);
        let (code, payload): (u16, &[u8]) = if text.contains("POST /v1/pack") {
            posts.fetch_add(1, Ordering::SeqCst);
            (status, body)
        } else {
            (200, b"")
        };
        let reason = if code == 200 { "OK" } else { "ERR" };
        let header = format!(
            "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(payload);
    }

    fn request_complete(buf: &[u8]) -> bool {
        let Some(split) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            return false;
        };
        let header_end = split + 4;
        let headers = String::from_utf8_lossy(&buf[..split]);
        let length = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        });
        match length {
            Some(length) => buf.len() >= header_end + length,
            None => true,
        }
    }

    async fn pack_from(
        host: &PackHost,
        config: Arc<irys_types::Config>,
        module: &Arc<StorageModule>,
        start: u32,
        end: u32,
        partition_hash: PartitionHash,
    ) -> Result<(), String> {
        pack_range(
            host.addr,
            config,
            module,
            start,
            end,
            partition_hash,
            Some(Duration::from_secs(2)),
        )
        .await
    }

    fn assert_requested(host: &PackHost) {
        assert!(
            host.posts.load(Ordering::SeqCst) >= 1,
            "the pack request must reach the remote host"
        );
    }

    /// Remote body outcomes for one inclusive range. Each row is the case the
    /// named test used to assert on its own.
    #[test_log::test(tokio::test)]
    async fn remote_pack_body_outcomes() {
        // status, body, start, end, use the module hash, expect success,
        // offset to read, stored type, failure text.
        let cases: [(u16, Vec<u8>, u32, u32, bool, bool, u32, ChunkType, &str); 6] = [
            (
                200,
                Vec::new(),
                0,
                0,
                true,
                false,
                0,
                ChunkType::Uninitialized,
                "an empty body on inclusive 0..=0 must fail",
            ),
            (
                200,
                vec![0x11; 64],
                0,
                2,
                true,
                false,
                2,
                ChunkType::Uninitialized,
                "two chunks for inclusive 0..=2 must fail",
            ),
            (
                200,
                vec![0x22; 16],
                0,
                0,
                true,
                false,
                0,
                ChunkType::Uninitialized,
                "a short trailing buffer must fail",
            ),
            (
                500,
                vec![0x33; 32],
                0,
                0,
                true,
                false,
                0,
                ChunkType::Uninitialized,
                "HTTP 500 must fail the range",
            ),
            (
                200,
                vec![0x44; 32],
                0,
                0,
                false,
                false,
                0,
                ChunkType::Uninitialized,
                "a refused entropy write must fail the range",
            ),
            (
                200,
                vec![0x55; 32],
                0,
                0,
                true,
                true,
                0,
                ChunkType::Entropy,
                "one full chunk for inclusive 0..=0 must succeed",
            ),
        ];
        for (status, body, start, end, own_hash, expect_ok, offset, kind, message) in cases {
            let (_tmp, config, module, hash) = test_module("remote_pack_body");
            let host = PackHost::spawn(status, body);
            let hash = if own_hash {
                hash
            } else {
                PartitionHash::repeat_byte(0x11)
            };
            let result = pack_from(&host, config, &module, start, end, hash).await;
            assert_requested(&host);
            assert_eq!(result.is_ok(), expect_ok, "{message}, got {result:?}");
            assert_eq!(
                module.get_chunk_type(&PartitionChunkOffset(offset)),
                Some(kind),
                "{message}"
            );
        }
    }

    /// Accepts the TCP connection and never answers. A client with no read
    /// timeout waits here.
    fn spawn_stall_host() -> SocketAddr {
        let (listener, addr) = bind_local("bind stall host");
        std::thread::spawn(move || {
            serve_for(listener, Duration::from_secs(3), |mut stream| {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = [0_u8; 1024];
                let _ = stream.read(&mut buf);
                std::thread::sleep(Duration::from_secs(2));
            });
        });
        addr
    }

    /// Answers `/info`, then writes the pack body one byte at a time.
    fn spawn_dribble_host(gap: Duration) -> SocketAddr {
        let (listener, addr) = bind_local("bind dribble host");
        std::thread::spawn(move || {
            serve_for(listener, Duration::from_secs(5), move |stream| {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_nodelay(true);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                handle_dribble(stream, gap);
            });
        });
        addr
    }

    fn handle_dribble(mut stream: TcpStream, gap: Duration) {
        let mut buf = Vec::new();
        let mut tmp = [0_u8; 1024];
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if request_complete(&buf) {
                        break;
                    }
                }
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => return,
            }
        }
        let text = String::from_utf8_lossy(&buf);
        if !text.contains("POST /v1/pack") {
            let header = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(header.as_bytes());
            return;
        }
        let header = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
        if stream.write_all(header.as_bytes()).is_err() {
            return;
        }
        // 32 bytes is one test chunk. The gap stays under the read timeout.
        for _ in 0..32 {
            std::thread::sleep(gap);
            if stream.write_all(b"1\r\n").is_err()
                || stream.write_all(&[0xAB]).is_err()
                || stream.write_all(b"\r\n").is_err()
                || stream.flush().is_err()
            {
                return;
            }
        }
        let _ = stream.write_all(b"0\r\n\r\n");
    }

    async fn pack_with_timeout(
        addr: SocketAddr,
        config: Arc<irys_types::Config>,
        module: &Arc<StorageModule>,
        partition_hash: PartitionHash,
        timeout: Option<Duration>,
    ) -> Result<(), String> {
        pack_range(addr, config, module, 0, 0, partition_hash, timeout).await
    }

    async fn pack_range(
        addr: SocketAddr,
        config: Arc<irys_types::Config>,
        module: &Arc<StorageModule>,
        start: u32,
        end: u32,
        partition_hash: PartitionHash,
        timeout: Option<Duration>,
    ) -> Result<(), String> {
        let packing_config = crate::packing_service::config::PackingConfig {
            poll_duration: Duration::from_secs(1),
            concurrency: 1,
            #[cfg(feature = "nvidia")]
            max_chunks: 1,
            chain_id: config.consensus.chain_id,
            remotes: vec![irys_types::RemotePackingConfig {
                url: format!("http://{addr}"),
                timeout,
            }],
        };
        let strategy =
            RemotePackingStrategy::new(config.clone(), packing_config, Arc::new(Notify::new()));
        strategy
            .pack(
                module,
                PartitionChunkRange(partition_chunk_offset_ii!(start, end)),
                config.node_config.miner_address(),
                partition_hash,
                module.id,
                100,
            )
            .await
    }

    #[test_log::test(tokio::test)]
    async fn one_remote_stream_item_stays_within_the_pending_ceiling() {
        let (_tmp, config, module, hash) =
            test_module_with_ceiling("remote_pending_ceiling", Some(32));
        assert_eq!(
            module.pending_memory_ceiling_bytes(),
            32,
            "the fixture ceiling must be one chunk"
        );
        let packing_config = crate::packing_service::config::PackingConfig {
            poll_duration: Duration::from_secs(1),
            concurrency: 1,
            #[cfg(feature = "nvidia")]
            max_chunks: 1,
            chain_id: config.consensus.chain_id,
            remotes: Vec::new(),
        };
        let strategy =
            RemotePackingStrategy::new(config.clone(), packing_config, Arc::new(Notify::new()));
        // Offsets 1, 2, 3 are not multiples of 1000, so the per-chunk sync
        // does not drain the queue. One `Bytes` holds all three chunks.
        let stream = futures::stream::iter(std::iter::once(Ok::<_, reqwest::Error>(Bytes::from(
            vec![0xAB; 96],
        ))));
        let result = strategy
            .process_chunk_stream(
                stream,
                &module,
                &PartitionChunkRange(partition_chunk_offset_ii!(1, 3)),
                1,
                1000,
                module.id,
                hash,
                config.node_config.miner_address(),
                Duration::from_secs(30),
            )
            .await;
        assert_eq!(result, Ok(4), "three chunks must be accepted: {result:?}");
        let pending = module.pending_write_bytes();
        let ceiling = module.pending_memory_ceiling_bytes();
        assert!(
            pending <= ceiling,
            "one stream item passed the pending ceiling: pending {pending} ceiling {ceiling}"
        );
    }

    #[test_log::test(tokio::test)]
    async fn remote_without_timeout_returns_without_waiting() {
        let (_tmp, config, module, hash) = test_module("remote_no_timeout");
        let addr = spawn_stall_host();
        let result = tokio::time::timeout(
            Duration::from_millis(500),
            pack_with_timeout(addr, config, &module, hash, None),
        )
        .await;
        assert!(
            matches!(result, Ok(Err(_))),
            "a remote with no timeout must fail itself, got {result:?}"
        );
    }

    #[test_log::test(tokio::test)]
    async fn remote_byte_dribble_hits_the_monotonic_deadline() {
        let (_tmp, config, module, hash) = test_module("remote_dribble_deadline");
        let packing_config = crate::packing_service::config::PackingConfig {
            poll_duration: Duration::from_secs(1),
            concurrency: 1,
            #[cfg(feature = "nvidia")]
            max_chunks: 1,
            chain_id: config.consensus.chain_id,
            remotes: Vec::new(),
        };
        let strategy =
            RemotePackingStrategy::new(config.clone(), packing_config, Arc::new(Notify::new()));
        let stream = slow_byte_stream(32, Duration::from_millis(20));
        let result = tokio::time::timeout(
            Duration::from_millis(400),
            strategy.process_chunk_stream(
                stream,
                &module,
                &PartitionChunkRange(partition_chunk_offset_ii!(0, 0)),
                0,
                100,
                module.id,
                hash,
                config.node_config.miner_address(),
                Duration::from_millis(100),
            ),
        )
        .await;
        assert!(
            matches!(result, Ok(Err(ref error)) if error.contains("monotonic deadline")),
            "the stream must fail on its own deadline, got {result:?}"
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset(0)),
            Some(ChunkType::Uninitialized),
            "a deadline must not store a partial dribble as a chunk"
        );
    }

    #[test_log::test(tokio::test)]
    async fn configured_remote_dribble_hits_the_stream_deadline() {
        let (_tmp, config, module, hash) = test_module("remote_http_dribble");
        let addr = spawn_dribble_host(Duration::from_millis(20));
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_millis(800),
            pack_with_timeout(
                addr,
                config,
                &module,
                hash,
                Some(Duration::from_millis(100)),
            ),
        )
        .await;
        assert!(
            matches!(result, Ok(Err(_))),
            "a dribble must hit the stream deadline, got {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(700),
            "the deadline must fire before the dribble finishes, elapsed {:?}",
            started.elapsed()
        );
    }

    /// Two windows of 1024 chunks. The host returns exactly one window per
    /// POST. The range is packed only when the client splits the request.
    #[test_log::test(tokio::test)]
    async fn remote_pack_splits_a_long_range_into_windows() {
        const WINDOW: u32 = 1024;
        let chunks = WINDOW * 2;
        let (_tmp, config, module, hash) =
            test_module_with_chunks("remote_pack_windows", u64::from(chunks));
        let host = PackHost::spawn_for(
            200,
            vec![0xAB; 32 * WINDOW as usize],
            Duration::from_secs(30),
        );
        let result = pack_from(&host, config, &module, 0, chunks - 1, hash).await;
        assert_eq!(
            result,
            Ok(()),
            "each window is its own request, got {result:?}"
        );
        assert_eq!(
            host.posts.load(Ordering::SeqCst),
            2,
            "a 2048-chunk range must be two requests"
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset(0)),
            Some(ChunkType::Entropy)
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset(chunks - 1)),
            Some(ChunkType::Entropy)
        );
    }

    /// A dribble under the read timeout must not get one timeout per chunk.
    /// 32 chunks at 100ms would be 3.2s. The cap is a few timeouts.
    #[test_log::test(tokio::test)]
    async fn remote_dribble_budget_is_capped_for_a_long_range() {
        let (_tmp, config, module, hash) = test_module("remote_dribble_cap");
        let addr = spawn_dribble_host(Duration::from_millis(40));
        let result = tokio::time::timeout(
            Duration::from_millis(900),
            pack_range(
                addr,
                config,
                &module,
                0,
                31,
                hash,
                Some(Duration::from_millis(100)),
            ),
        )
        .await;
        assert!(
            matches!(result, Ok(Err(_))),
            "a long-range dribble must fail inside the capped budget, got {result:?}"
        );
    }

    fn slow_byte_stream(
        count: usize,
        gap: Duration,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send>> {
        Box::pin(futures::stream::unfold(0_usize, move |sent| async move {
            if sent >= count {
                return None;
            }
            tokio::time::sleep(gap).await;
            Some((
                Ok::<Bytes, reqwest::Error>(Bytes::copy_from_slice(&[0xAB])),
                sent + 1,
            ))
        }))
    }
}
