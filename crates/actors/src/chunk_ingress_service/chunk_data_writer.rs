use dashmap::{DashMap, DashSet};
use irys_database::cache_chunk_verified;
use irys_types::{ChunkPathHash, DataRoot, DatabaseProvider, UnpackedChunk};
use reth_db::Database as _;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, warn};

const MAX_BATCH_SIZE: usize = 64;

enum WriterCommand {
    WriteChunk(Arc<UnpackedChunk>),
    Flush(oneshot::Sender<Result<(), QueueError>>),
}

/// Async write-behind buffer for chunk MDBX writes.
///
/// Chunks are queued via [`queue_write`] and written in batches on a background
/// task, reducing per-chunk MDBX transaction overhead on the hot ingress path.
/// A [`DashSet`] tracks pending chunk hashes so callers can check for
/// in-flight duplicates, and a [`DashMap`] maintains per-data-root chunk counts
/// for ingress proof threshold checks without hitting the database.
#[derive(Debug)]
pub(crate) struct ChunkDataWriter {
    tx: mpsc::Sender<WriterCommand>,
    pending_hashes: Arc<DashSet<ChunkPathHash>>,
    pending_chunk_counts: Arc<DashMap<DataRoot, u64>>,
}

impl ChunkDataWriter {
    /// Spawn the background writer task and return the writer handle.
    pub(crate) fn spawn(db: DatabaseProvider, buffer_size: usize) -> Self {
        let (tx, rx) = mpsc::channel(buffer_size.max(1));
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
    pub(crate) async fn queue_write(&self, chunk: Arc<UnpackedChunk>) -> Result<bool, QueueError> {
        let hash = chunk.chunk_path_hash();
        if !self.pending_hashes.insert(hash) {
            return Ok(true);
        }
        if self
            .tx
            .send(WriterCommand::WriteChunk(chunk))
            .await
            .is_err()
        {
            self.pending_hashes.remove(&hash);
            return Err(QueueError::ChannelClosed);
        }
        Ok(false)
    }

    /// Returns `true` if the chunk hash is currently pending a write.
    pub(crate) fn is_pending(&self, hash: &ChunkPathHash) -> bool {
        self.pending_hashes.contains(hash)
    }

    /// Returns the number of chunks pending or already written for a data root.
    pub(crate) fn pending_chunk_count(&self, data_root: &DataRoot) -> u64 {
        self.pending_chunk_counts
            .get(data_root)
            .map(|v| *v)
            .unwrap_or(0)
    }

    /// Drain all pending writes before returning.
    ///
    /// Sends a flush sentinel through the channel and waits for the
    /// background writer to acknowledge it. Because the channel is FIFO,
    /// all chunks queued before the flush are guaranteed to be committed
    /// to MDBX by the time this returns.
    pub(crate) async fn flush(&self) -> Result<(), QueueError> {
        let (done_tx, done_rx) = oneshot::channel();
        self.tx
            .send(WriterCommand::Flush(done_tx))
            .await
            .map_err(|_| QueueError::ChannelClosed)?;
        done_rx.await.map_err(|_| QueueError::ChannelClosed)?
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueError {
    #[error("writer channel closed")]
    ChannelClosed,
    #[error("batch write failed")]
    WriteFailed,
}

struct BackgroundWriter {
    rx: mpsc::Receiver<WriterCommand>,
    db: DatabaseProvider,
    pending_hashes: Arc<DashSet<ChunkPathHash>>,
    pending_chunk_counts: Arc<DashMap<DataRoot, u64>>,
}

impl BackgroundWriter {
    async fn run(mut self) {
        let mut batch: Vec<Arc<UnpackedChunk>> = Vec::with_capacity(MAX_BATCH_SIZE);

        loop {
            // Block until at least one command arrives.
            let cmd = match self.rx.recv().await {
                Some(cmd) => cmd,
                None => {
                    debug!("ChunkDataWriter channel closed, flushing remaining");
                    break;
                }
            };

            match cmd {
                WriterCommand::WriteChunk(chunk) => batch.push(chunk),
                WriterCommand::Flush(done) => {
                    let result = if !batch.is_empty() {
                        let r = self.write_batch(&batch);
                        batch.clear();
                        r
                    } else {
                        Ok(())
                    };
                    let _ = done.send(result);
                    continue;
                }
            }

            // Drain up to MAX_BATCH_SIZE without blocking.
            while batch.len() < MAX_BATCH_SIZE {
                match self.rx.try_recv() {
                    Ok(WriterCommand::WriteChunk(chunk)) => batch.push(chunk),
                    Ok(WriterCommand::Flush(done)) => {
                        let result = self.write_batch(&batch);
                        batch.clear();
                        let _ = done.send(result);
                        break;
                    }
                    Err(_) => break,
                }
            }

            if !batch.is_empty() {
                let _ = self.write_batch(&batch);
                batch.clear();
            }
        }

        // Flush any remaining chunks.
        if !batch.is_empty() {
            let _ = self.write_batch(&batch);
        }
    }

    fn write_batch(&self, batch: &[Arc<UnpackedChunk>]) -> Result<(), QueueError> {
        if batch.is_empty() {
            return Ok(());
        }

        // Precompute SHA-256 hashes once — avoids redundant hashing inside the
        // MDBX closure (log lines) and during post-transaction cleanup.
        let hashes: Vec<ChunkPathHash> = batch.iter().map(|c| c.chunk_path_hash()).collect();

        let result = self.db.update(|tx| {
            let mut newly_written = Vec::with_capacity(batch.len());
            for (chunk, hash) in batch.iter().zip(hashes.iter()) {
                match cache_chunk_verified(tx, chunk) {
                    Ok(is_duplicate) => {
                        if is_duplicate {
                            warn!(
                                "Duplicate chunk {} of {} in write-behind batch",
                                hash, chunk.data_root
                            );
                            newly_written.push(false);
                        } else {
                            newly_written.push(true);
                        }
                    }
                    Err(e) => {
                        error!(
                            "Failed to cache chunk {} of {}: {:?}",
                            hash, chunk.data_root, e
                        );
                        newly_written.push(false);
                    }
                }
            }
            Ok::<Vec<bool>, eyre::Report>(newly_written)
        });

        match result {
            Ok(Ok(newly_written)) => {
                let written: usize = newly_written.iter().filter(|&&w| w).count();
                debug!(
                    "ChunkDataWriter committed batch of {} ({} written)",
                    batch.len(),
                    written
                );
                for ((chunk, hash), was_new) in
                    batch.iter().zip(hashes.iter()).zip(newly_written.iter())
                {
                    self.pending_hashes.remove(hash);
                    if *was_new {
                        *self
                            .pending_chunk_counts
                            .entry(chunk.data_root)
                            .or_insert(0) += 1;
                    }
                }
                Ok(())
            }
            Ok(Err(e)) => {
                error!("ChunkDataWriter batch inner error: {:?}", e);
                for hash in &hashes {
                    self.pending_hashes.remove(hash);
                }
                Err(QueueError::WriteFailed)
            }
            Err(e) => {
                error!("ChunkDataWriter MDBX transaction error: {:?}", e);
                for hash in &hashes {
                    self.pending_hashes.remove(hash);
                }
                Err(QueueError::WriteFailed)
            }
        }
    }
}
