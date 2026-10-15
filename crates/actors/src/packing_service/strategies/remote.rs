use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt as _;
use irys_domain::StorageModule;
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
    ) -> Result<u32, String> {
        let chunk_size = self.config.consensus.chunk_size as usize;
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
            let wrote = storage_module.write_entropy_chunk(
                PartitionChunkOffset(range_start),
                chunk_bytes.to_vec(),
                partition_hash,
            );
            if !wrote {
                return Err(format!(
                    "entropy write refused at offset {range_start} for partition {partition_hash}"
                ));
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

        // Process streaming chunks
        while let Some(chunk_result) = stream.next().await {
            // Wait for the disk before taking more entropy off the stream.
            if !storage_module.has_pending_room() {
                crate::packing_service::wait_for_pending_room(
                    storage_module,
                    "remote packing (streaming)",
                )
                .await?;
            }
            let chunk = chunk_result.map_err(|e| format!("Error getting chunk: {:?}", e))?;
            buffer.extend_from_slice(&chunk);

            // Process complete chunks from buffer
            while buffer.len() >= chunk_size {
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

        let RemotePackingConfig { url, timeout } = remote;
        let v1_url = format!("{}/v1", url);

        let client = self
            .client_pool
            .get_or_create_client(&v1_url, timeout.as_ref());

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
        )
        .await
    }
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
        let mut current_start = range_start;

        // Try each remote in order
        for remote in &self.packing_config.remotes {
            match self
                .try_remote(
                    remote,
                    storage_module,
                    current_start,
                    range_end,
                    mining_address,
                    partition_hash,
                    storage_module_id,
                    short_writes_before_sync,
                )
                .await
            {
                Ok(new_start) => {
                    // Inclusive range `range_start..=range_end` is done only when
                    // the next offset is one past the end. `>= range_end` treats
                    // a stream that stopped on the last offset as success.
                    let Some(done) = range_end.checked_add(1) else {
                        return Err(format!(
                            "remote packing range end {range_end} has no following chunk"
                        ));
                    };
                    if new_start == done {
                        return Ok(());
                    }
                    if new_start > done {
                        return Err(format!(
                            "remote packing advanced to {new_start}, past inclusive end {range_end}"
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

    /// One-shot HTTP/1.1 host. Counts `POST /v1/pack` requests.
    struct PackHost {
        addr: SocketAddr,
        posts: Arc<AtomicUsize>,
    }

    impl PackHost {
        fn spawn(status: u16, body: Vec<u8>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind pack host");
            listener
                .set_nonblocking(true)
                .expect("pack host accepts without blocking the test thread");
            let addr = listener.local_addr().expect("pack host addr");
            let posts = Arc::new(AtomicUsize::new(0));
            let posts_thread = posts.clone();
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while std::time::Instant::now() < deadline {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                            handle_conn(stream, status, &body, &posts_thread);
                        }
                        Err(err)
                            if err.kind() == std::io::ErrorKind::WouldBlock
                                || err.kind() == std::io::ErrorKind::TimedOut =>
                        {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
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
        let packing_config = crate::packing_service::config::PackingConfig {
            poll_duration: Duration::from_secs(1),
            concurrency: 1,
            #[cfg(feature = "nvidia")]
            max_chunks: 1,
            chain_id: config.consensus.chain_id,
            remotes: vec![irys_types::RemotePackingConfig {
                url: format!("http://{}", host.addr),
                timeout: Some(Duration::from_secs(2)),
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

    fn assert_requested(host: &PackHost) {
        assert!(
            host.posts.load(Ordering::SeqCst) >= 1,
            "the pack request must reach the remote host"
        );
    }

    #[test_log::test(tokio::test)]
    async fn empty_remote_body_is_not_a_successful_inclusive_range() {
        let (_tmp, config, module, hash) = test_module("remote_empty_body");
        let host = PackHost::spawn(200, Vec::new());
        let result = pack_from(&host, config, &module, 0, 0, hash).await;
        assert_requested(&host);
        assert!(
            result.is_err(),
            "an empty body on inclusive 0..=0 must fail, got {result:?}"
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset(0)),
            Some(ChunkType::Uninitialized)
        );
    }

    #[test_log::test(tokio::test)]
    async fn short_remote_body_does_not_finish_the_inclusive_range() {
        let (_tmp, config, module, hash) = test_module("remote_short_body");
        let host = PackHost::spawn(200, vec![0x11; 64]);
        let result = pack_from(&host, config, &module, 0, 2, hash).await;
        assert_requested(&host);
        assert!(
            result.is_err(),
            "two chunks for inclusive 0..=2 must fail, got {result:?}"
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset(2)),
            Some(ChunkType::Uninitialized)
        );
    }

    #[test_log::test(tokio::test)]
    async fn short_trailing_remote_buffer_is_rejected() {
        let (_tmp, config, module, hash) = test_module("remote_trailing_buffer");
        let host = PackHost::spawn(200, vec![0x22; 16]);
        let result = pack_from(&host, config, &module, 0, 0, hash).await;
        assert_requested(&host);
        assert!(
            result.is_err(),
            "a short trailing buffer must fail, got {result:?}"
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset(0)),
            Some(ChunkType::Uninitialized),
            "a short buffer must not be stored as a chunk"
        );
    }

    #[test_log::test(tokio::test)]
    async fn remote_http_error_does_not_pack_the_body() {
        let (_tmp, config, module, hash) = test_module("remote_http_error");
        let host = PackHost::spawn(500, vec![0x33; 32]);
        let result = pack_from(&host, config, &module, 0, 0, hash).await;
        assert_requested(&host);
        assert!(
            result.is_err(),
            "HTTP 500 must fail the range, got {result:?}"
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset(0)),
            Some(ChunkType::Uninitialized),
            "an error body must not be stored as entropy"
        );
    }

    #[test_log::test(tokio::test)]
    async fn remote_refused_entropy_write_fails_the_range() {
        let (_tmp, config, module, _hash) = test_module("remote_refused_write");
        let host = PackHost::spawn(200, vec![0x44; 32]);
        let other_hash = PartitionHash::repeat_byte(0x11);
        let result = pack_from(&host, config, &module, 0, 0, other_hash).await;
        assert_requested(&host);
        assert!(
            result.is_err(),
            "a refused entropy write must fail the range, got {result:?}"
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset(0)),
            Some(ChunkType::Uninitialized)
        );
    }

    #[test_log::test(tokio::test)]
    async fn complete_remote_body_packs_the_inclusive_range() {
        let (_tmp, config, module, hash) = test_module("remote_complete_body");
        let host = PackHost::spawn(200, vec![0x55; 32]);
        let result = pack_from(&host, config, &module, 0, 0, hash).await;
        assert_requested(&host);
        assert!(
            result.is_ok(),
            "one full chunk for inclusive 0..=0 must succeed, got {result:?}"
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset(0)),
            Some(ChunkType::Entropy)
        );
    }
}
