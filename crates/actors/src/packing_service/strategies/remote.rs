use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt as _;
use irys_domain::{ChunkType, StorageModule};
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

        let mut process_chunk = |chunk_bytes: Bytes| {
            storage_module.write_chunk(
                PartitionChunkOffset(range_start),
                chunk_bytes.to_vec(),
                ChunkType::Entropy,
            );
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
        };

        // Process streaming chunks
        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| format!("Error getting chunk: {:?}", e))?;
            buffer.extend_from_slice(&chunk);

            // Process complete chunks from buffer
            while buffer.len() >= chunk_size {
                let chunk_to_process = buffer.split_to(chunk_size).freeze();
                process_chunk(chunk_to_process);
            }
        }

        // Process any remaining partial chunk
        if !buffer.is_empty() {
            process_chunk(buffer.freeze());
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
        if let Err(e) = client.get(format!("{}/info", &v1_url)).send().await {
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
            .post(format!("{}/pack", &v1_url))
            .json(&request)
            .send()
            .await
            .map_err(|e| format!("Error sending packing request to {} - {}", &v1_url, e))?;

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
                    current_start = new_start;
                    if current_start >= range_end {
                        return Ok(());
                    }
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
