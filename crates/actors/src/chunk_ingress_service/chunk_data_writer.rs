use dashmap::DashSet;
use irys_database::db::IrysDatabaseExt as _;
use irys_database::{cache_chunk_verified, store_ingress_data_hash};
use irys_types::{
    ChunkPathHash, DatabaseProvider, H256, IrysAddress, UnpackedChunk, generate_ingress_data_hash,
};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, warn};

const MAX_BATCH_SIZE: usize = 64;

struct PendingChunkWrite {
    chunk: Arc<UnpackedChunk>,
    done: oneshot::Sender<Result<(), QueueError>>,
}

/// Async write-behind buffer for chunk MDBX writes.
///
/// Chunks are queued via [`queue_write`] and written in batches on a background
/// task, reducing per-chunk MDBX transaction overhead on the hot ingress path.
/// A [`DashSet`] tracks pending chunk hashes so the writer can detect
/// in-flight duplicates without hitting the database.
#[derive(Debug)]
pub(crate) struct ChunkDataWriter {
    tx: mpsc::Sender<PendingChunkWrite>,
    pending_hashes: Arc<DashSet<ChunkPathHash>>,
}

pub(crate) struct QueuedChunkWrite {
    is_duplicate: bool,
    done: oneshot::Receiver<Result<(), QueueError>>,
}

impl QueuedChunkWrite {
    pub(crate) const fn is_duplicate(&self) -> bool {
        self.is_duplicate
    }

    pub(crate) async fn committed(self) -> Result<(), QueueError> {
        self.done.await.map_err(|_| QueueError::ChannelClosed)?
    }
}

impl ChunkDataWriter {
    /// Spawn the background writer task and return the writer handle.
    pub(crate) fn spawn(
        db: DatabaseProvider,
        address: IrysAddress,
        buffer_size: usize,
        runtime_handle: &tokio::runtime::Handle,
    ) -> Self {
        let (tx, rx) = mpsc::channel(buffer_size.max(1));
        let pending_hashes = Arc::new(DashSet::new());

        let writer = BackgroundWriter {
            rx,
            db,
            address,
            pending_hashes: Arc::clone(&pending_hashes),
        };
        runtime_handle.spawn(writer.run());

        Self { tx, pending_hashes }
    }

    /// Queue a chunk and return its exact batch-commit acknowledgement.
    ///
    /// Concurrent writes are still committed in shared MDBX batches, but each
    /// caller receives an acknowledgement for the batch containing its own
    /// chunk. The returned handle also records whether the chunk hash was
    /// already pending (duplicate).
    pub(crate) async fn queue_write(
        &self,
        chunk: Arc<UnpackedChunk>,
    ) -> Result<QueuedChunkWrite, QueueError> {
        let hash = chunk.chunk_path_hash();
        let is_duplicate = !self.pending_hashes.insert(hash);
        let (done_tx, done_rx) = oneshot::channel();
        if self
            .tx
            .send(PendingChunkWrite {
                chunk,
                done: done_tx,
            })
            .await
            .is_err()
        {
            self.pending_hashes.remove(&hash);
            return Err(QueueError::ChannelClosed);
        }
        Ok(QueuedChunkWrite {
            is_duplicate,
            done: done_rx,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum QueueError {
    #[error("writer channel closed")]
    ChannelClosed,
    #[error("batch write failed")]
    WriteFailed,
}

struct BackgroundWriter {
    rx: mpsc::Receiver<PendingChunkWrite>,
    db: DatabaseProvider,
    address: IrysAddress,
    pending_hashes: Arc<DashSet<ChunkPathHash>>,
}

impl BackgroundWriter {
    async fn run(mut self) {
        let mut batch: Vec<PendingChunkWrite> = Vec::with_capacity(MAX_BATCH_SIZE);

        loop {
            let write = match self.rx.recv().await {
                Some(write) => write,
                None => {
                    debug!("ChunkDataWriter channel closed, committing remaining writes");
                    break;
                }
            };

            batch.push(write);

            while batch.len() < MAX_BATCH_SIZE {
                match self.rx.try_recv() {
                    Ok(write) => batch.push(write),
                    Err(_) => break,
                }
            }

            if !batch.is_empty()
                && let Err(e) = self.commit_batch(&mut batch)
            {
                error!("ChunkDataWriter auto-drain write failed: {:?}", e);
            }
        }

        if let Err(e) = self.commit_batch(&mut batch) {
            error!("ChunkDataWriter shutdown-drain write failed: {:?}", e);
        }
    }

    /// Commits one batch and reports the same transaction outcome to every
    /// write it contained, so an auto-drain failure reaches each exact caller.
    fn commit_batch(&self, batch: &mut Vec<PendingChunkWrite>) -> Result<(), QueueError> {
        if batch.is_empty() {
            return Ok(());
        }
        let result = self.write_batch(batch);
        for pending in batch.drain(..) {
            let _ = pending.done.send(result);
        }
        result
    }

    #[tracing::instrument(level = "trace", skip_all, fields(batch_size = batch.len()))]
    fn write_batch(&self, batch: &[PendingChunkWrite]) -> Result<(), QueueError> {
        if batch.is_empty() {
            return Ok(());
        }

        let hashes: Vec<(ChunkPathHash, H256)> = batch
            .iter()
            .map(|pending| {
                (
                    pending.chunk.chunk_path_hash(),
                    generate_ingress_data_hash(&pending.chunk.bytes.0, self.address),
                )
            })
            .collect();

        let result = self.db.update_eyre(|tx| {
            let mut written = 0_usize;
            for (pending, (chunk_path_hash, ingress_data_hash)) in batch.iter().zip(&hashes) {
                match cache_chunk_verified(tx, &pending.chunk) {
                    Ok(is_duplicate) => {
                        if is_duplicate {
                            warn!(
                                "Duplicate chunk {} of {} in write-behind batch",
                                chunk_path_hash, pending.chunk.data_root
                            );
                        } else {
                            written += 1;
                        }
                        store_ingress_data_hash(
                            tx,
                            pending.chunk.data_root,
                            self.address,
                            pending.chunk.tx_offset,
                            *ingress_data_hash,
                        )
                        .map_err(eyre::Report::from)?;
                    }
                    Err(e) => {
                        error!(
                            "Failed to cache chunk {} of {}: {:?}",
                            chunk_path_hash, pending.chunk.data_root, e
                        );
                        return Err(e);
                    }
                }
            }
            Ok(written)
        });

        for (chunk_path_hash, _) in &hashes {
            self.pending_hashes.remove(chunk_path_hash);
        }

        match result {
            Ok(written) => {
                debug!(
                    "ChunkDataWriter committed batch of {} ({} written)",
                    batch.len(),
                    written
                );
                Ok(())
            }
            Err(e) => {
                error!("ChunkDataWriter batch transaction failed: {:?}", e);
                Err(QueueError::WriteFailed)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_database::{
        cached_chunk_by_chunk_offset, cached_ingress_leaf, complete_ingress_leaves,
        database::{IrysDatabaseArgs as _, open_or_create_db},
        db_cache::CachedDataRoot,
        tables::{CachedDataRoots, IrysTables},
    };
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{Base64, H256, TxChunkOffset};
    use reth_db::{mdbx::DatabaseArguments, transaction::DbTxMut as _};

    #[test_log::test(tokio::test)]
    async fn validated_leaf_is_persisted_without_a_storage_assignment() -> eyre::Result<()> {
        let dir = TempDirBuilder::new().with_tracing().build();
        let env = open_or_create_db(
            dir.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(env));
        let data_root = H256::random();
        let address = IrysAddress::random();
        let tx_offset = TxChunkOffset::from(0_u32);
        let chunk = Arc::new(UnpackedChunk {
            data_root,
            data_size: 8,
            data_path: Base64::default(),
            bytes: Base64(vec![7_u8; 8]),
            tx_offset,
        });
        let ingress_data_hash = generate_ingress_data_hash(&chunk.bytes.0, address);
        let leaf = irys_types::generate_ingress_leaf_from_data_hash(ingress_data_hash, 8)?;
        db.update_eyre(|tx| {
            tx.put::<CachedDataRoots>(
                data_root,
                CachedDataRoot {
                    data_size: 8,
                    data_size_confirmed: true,
                    block_set: vec![H256::random()],
                    ..Default::default()
                },
            )?;
            Ok(())
        })?;

        let writer =
            ChunkDataWriter::spawn(db.clone(), address, 8, &tokio::runtime::Handle::current());
        let queued = writer.queue_write(chunk).await?;
        assert!(!queued.is_duplicate());
        queued.committed().await?;

        db.view_eyre(|tx| {
            assert!(cached_chunk_by_chunk_offset(tx, data_root, tx_offset)?.is_some());
            let cached_leaf = cached_ingress_leaf(tx, data_root, address, tx_offset)?
                .expect("validated leaf must be stored without any storage assignment");
            assert_eq!(cached_leaf.ingress_data_hash, ingress_data_hash);
            assert_eq!(
                complete_ingress_leaves(tx, data_root, address, 8, 8)?.unwrap(),
                vec![leaf],
                "a non-assignee's validated leaves must satisfy proof readiness"
            );
            Ok(())
        })
    }

    #[test_log::test(tokio::test)]
    async fn every_write_in_a_failed_auto_drain_batch_receives_the_error() -> eyre::Result<()> {
        let dir = TempDirBuilder::new().with_tracing().build();
        let env = open_or_create_db(
            dir.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(env));
        let data_root = H256::random();
        let address = IrysAddress::random();
        let tx_offset = TxChunkOffset::from(0_u32);
        let chunk = Arc::new(UnpackedChunk {
            data_root,
            data_size: 8,
            data_path: Base64::default(),
            bytes: Base64(vec![7_u8; 8]),
            tx_offset,
        });
        db.update_eyre(|tx| {
            tx.put::<CachedDataRoots>(
                data_root,
                CachedDataRoot {
                    data_size: 8,
                    data_size_confirmed: true,
                    block_set: vec![H256::random()],
                    ..Default::default()
                },
            )?;
            store_ingress_data_hash(tx, data_root, address, tx_offset, H256::repeat_byte(9))?;
            Ok(())
        })?;

        let (command_tx, command_rx) = mpsc::channel(4);
        let pending_hashes = Arc::new(DashSet::new());
        let background = BackgroundWriter {
            rx: command_rx,
            db: db.clone(),
            address,
            pending_hashes,
        };
        let (first_done_tx, first_done_rx) = oneshot::channel();
        let (second_done_tx, second_done_rx) = oneshot::channel();
        for done in [first_done_tx, second_done_tx] {
            command_tx
                .send(PendingChunkWrite {
                    chunk: Arc::clone(&chunk),
                    done,
                })
                .await?;
        }
        drop(command_tx);

        background.run().await;
        assert_eq!(first_done_rx.await?, Err(QueueError::WriteFailed));
        assert_eq!(second_done_rx.await?, Err(QueueError::WriteFailed));
        db.view_eyre(|tx| {
            assert!(cached_chunk_by_chunk_offset(tx, data_root, tx_offset)?.is_none());
            assert_eq!(
                cached_ingress_leaf(tx, data_root, address, tx_offset)?
                    .expect("pre-existing leaf remains after rollback")
                    .ingress_data_hash,
                H256::repeat_byte(9)
            );
            Ok(())
        })
    }
}
