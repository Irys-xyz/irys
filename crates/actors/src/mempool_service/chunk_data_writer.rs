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
#[derive(Debug)]
pub struct ChunkDataWriter {
    tx: mpsc::Sender<WriterCommand>,
    pending_hashes: Arc<DashSet<ChunkPathHash>>,
}

impl ChunkDataWriter {
    pub fn spawn(db: DatabaseProvider, buffer_size: usize) -> Self {
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

    pub async fn queue_write(&self, chunk: Arc<UnpackedChunk>) -> Result<bool, QueueError> {
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
    pub async fn flush(&self) -> Result<(), QueueError> {
        let (done_tx, done_rx) = oneshot::channel();
        self.tx
            .send(WriterCommand::Flush(done_tx))
            .await
            .map_err(|_| QueueError::ChannelClosed)?;
        done_rx.await.map_err(|_| QueueError::ChannelClosed)?
    }
}

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
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
            let mut newly_written = Vec::with_capacity(batch.len());
            for (chunk, hash) in batch.iter().zip(hashes.iter()) {
                match cache_chunk_verified(tx, chunk) {
                    Ok(is_duplicate) => {
                        if is_duplicate {
                            warn!(
                                "Duplicate chunk {} of {} in write-behind batch",
                                hash, chunk.data_root
                            );
                        }
                        newly_written.push(!is_duplicate);
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
                for hash in &hashes {
                    self.pending_hashes.remove(hash);
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
