use dashmap::DashSet;
use irys_database::cache_chunk_verified;
use irys_types::{ChunkPathHash, DatabaseProvider, UnpackedChunk};
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
/// A [`DashSet`] tracks pending chunk hashes so the writer can detect
/// in-flight duplicates without hitting the database.
#[derive(Debug)]
pub(crate) struct ChunkDataWriter {
    tx: mpsc::Sender<WriterCommand>,
    pending_hashes: Arc<DashSet<ChunkPathHash>>,
}

impl ChunkDataWriter {
    /// Spawn the background writer task and return the writer handle.
    pub(crate) fn spawn(db: DatabaseProvider, buffer_size: usize) -> Self {
        let (tx, rx) = mpsc::channel(buffer_size.max(1));
        let pending_hashes = Arc::new(DashSet::new());

        let writer = BackgroundWriter {
            rx,
            db,
            pending_hashes: Arc::clone(&pending_hashes),
        };
        tokio::spawn(writer.run());

        Self { tx, pending_hashes }
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
}

impl BackgroundWriter {
    async fn run(mut self) {
        let mut batch: Vec<Arc<UnpackedChunk>> = Vec::with_capacity(MAX_BATCH_SIZE);

        loop {
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

        if !batch.is_empty() {
            let _ = self.write_batch(&batch);
        }
    }

    fn write_batch(&self, batch: &[Arc<UnpackedChunk>]) -> Result<(), QueueError> {
        if batch.is_empty() {
            return Ok(());
        }

        let hashes: Vec<ChunkPathHash> = batch.iter().map(|c| c.chunk_path_hash()).collect();

        let result = self.db.update(|tx| {
            let mut written = 0_usize;
            for (chunk, hash) in batch.iter().zip(hashes.iter()) {
                match cache_chunk_verified(tx, chunk) {
                    Ok(is_duplicate) => {
                        if is_duplicate {
                            warn!(
                                "Duplicate chunk {} of {} in write-behind batch",
                                hash, chunk.data_root
                            );
                        } else {
                            written += 1;
                        }
                    }
                    Err(e) => {
                        error!(
                            "Failed to cache chunk {} of {}: {:?}",
                            hash, chunk.data_root, e
                        );
                    }
                }
            }
            Ok::<usize, eyre::Report>(written)
        });

        for hash in &hashes {
            self.pending_hashes.remove(hash);
        }

        match result {
            Ok(Ok(written)) => {
                debug!(
                    "ChunkDataWriter committed batch of {} ({} written)",
                    batch.len(),
                    written
                );
                Ok(())
            }
            Ok(Err(e)) => {
                error!("ChunkDataWriter batch inner error: {:?}", e);
                Err(QueueError::WriteFailed)
            }
            Err(e) => {
                error!("ChunkDataWriter MDBX transaction error: {:?}", e);
                Err(QueueError::WriteFailed)
            }
        }
    }
}
