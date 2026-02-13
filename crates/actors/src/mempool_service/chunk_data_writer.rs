use dashmap::{DashMap, DashSet};
use irys_database::cache_chunk_verified;
use irys_types::{ChunkPathHash, DataRoot, DatabaseProvider, UnpackedChunk};
use reth_db::Database as _;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

const MAX_BATCH_SIZE: usize = 64;

/// Async write-behind buffer for chunk MDBX writes.
///
/// Chunks are queued via [`queue_write`] and written in batches on a background
/// task, reducing per-chunk MDBX transaction overhead on the hot ingress path.
/// A [`DashSet`] tracks pending chunk hashes so callers can check for
/// in-flight duplicates, and a [`DashMap`] maintains per-data-root chunk counts
/// for ingress proof threshold checks without hitting the database.
#[derive(Debug)]
pub struct ChunkDataWriter {
    tx: mpsc::Sender<UnpackedChunk>,
    pending_hashes: Arc<DashSet<ChunkPathHash>>,
    pending_chunk_counts: Arc<DashMap<DataRoot, u64>>,
}

impl ChunkDataWriter {
    /// Spawn the background writer task and return the writer handle.
    pub fn spawn(db: DatabaseProvider, buffer_size: usize) -> Self {
        let (tx, rx) = mpsc::channel(buffer_size);
        let pending_hashes = Arc::new(DashSet::new());
        let pending_chunk_counts = Arc::new(DashMap::new());

        let writer = BackgroundWriter {
            rx,
            db,
            pending_hashes: Arc::clone(&pending_hashes),
            pending_chunk_counts: Arc::clone(&pending_chunk_counts),
        };
        tokio::spawn(writer.run());

        Self {
            tx,
            pending_hashes,
            pending_chunk_counts,
        }
    }

    /// Queue a chunk for asynchronous write to the cache DB.
    ///
    /// Returns `true` if the chunk hash is already pending (duplicate).
    pub async fn queue_write(&self, chunk: &UnpackedChunk) -> Result<bool, QueueError> {
        let hash = chunk.chunk_path_hash();
        if !self.pending_hashes.insert(hash) {
            return Ok(true);
        }
        self.tx
            .send(chunk.clone())
            .await
            .map_err(|_| QueueError::ChannelClosed)?;
        Ok(false)
    }

    /// Returns `true` if the chunk hash is currently pending a write.
    pub fn is_pending(&self, hash: &ChunkPathHash) -> bool {
        self.pending_hashes.contains(hash)
    }

    /// Returns the number of chunks pending or already written for a data root.
    pub fn pending_chunk_count(&self, data_root: &DataRoot) -> u64 {
        self.pending_chunk_counts
            .get(data_root)
            .map(|v| *v)
            .unwrap_or(0)
    }

    /// Drain all pending writes before returning.
    ///
    /// Used before ingress proof generation to ensure all queued chunks
    /// are committed to MDBX.
    pub async fn flush(&self) -> Result<(), QueueError> {
        // Acquiring a send permit blocks until the receiver has consumed all
        // prior messages.  Drop the permit without sending.
        let _permit = self
            .tx
            .reserve()
            .await
            .map_err(|_| QueueError::ChannelClosed)?;
        drop(_permit);

        // Yield to let the writer finish its current batch.
        tokio::task::yield_now().await;

        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("writer channel closed")]
    ChannelClosed,
}

struct BackgroundWriter {
    rx: mpsc::Receiver<UnpackedChunk>,
    db: DatabaseProvider,
    pending_hashes: Arc<DashSet<ChunkPathHash>>,
    pending_chunk_counts: Arc<DashMap<DataRoot, u64>>,
}

impl BackgroundWriter {
    async fn run(mut self) {
        let mut batch: Vec<UnpackedChunk> = Vec::with_capacity(MAX_BATCH_SIZE);

        loop {
            // Block until at least one chunk arrives.
            match self.rx.recv().await {
                Some(chunk) => batch.push(chunk),
                None => {
                    debug!("ChunkDataWriter channel closed, flushing remaining");
                    break;
                }
            }

            // Drain up to MAX_BATCH_SIZE without blocking.
            while batch.len() < MAX_BATCH_SIZE {
                match self.rx.try_recv() {
                    Ok(chunk) => batch.push(chunk),
                    Err(_) => break,
                }
            }

            self.write_batch(&batch);
            batch.clear();
        }

        // Flush any remaining chunks.
        if !batch.is_empty() {
            self.write_batch(&batch);
        }
    }

    fn write_batch(&self, batch: &[UnpackedChunk]) {
        if batch.is_empty() {
            return;
        }

        let result = self.db.update(|tx| {
            let mut written = 0_u64;
            for chunk in batch {
                match cache_chunk_verified(tx, chunk) {
                    Ok(is_duplicate) => {
                        if is_duplicate {
                            warn!(
                                "Duplicate chunk {} of {} in write-behind batch",
                                chunk.chunk_path_hash(),
                                chunk.data_root
                            );
                        } else {
                            written += 1;
                        }
                    }
                    Err(e) => {
                        error!(
                            "Failed to cache chunk {} of {}: {:?}",
                            chunk.chunk_path_hash(),
                            chunk.data_root,
                            e
                        );
                    }
                }
            }
            Ok::<u64, eyre::Report>(written)
        });

        match result {
            Ok(Ok(written)) => {
                debug!(
                    "ChunkDataWriter committed batch of {} ({} written)",
                    batch.len(),
                    written
                );
            }
            Ok(Err(e)) => {
                error!("ChunkDataWriter batch inner error: {:?}", e);
            }
            Err(e) => {
                error!("ChunkDataWriter MDBX transaction error: {:?}", e);
            }
        }

        // Remove pending hashes and update chunk counts after the write.
        for chunk in batch {
            self.pending_hashes.remove(&chunk.chunk_path_hash());
            *self
                .pending_chunk_counts
                .entry(chunk.data_root)
                .or_insert(0) += 1;
        }
    }
}
