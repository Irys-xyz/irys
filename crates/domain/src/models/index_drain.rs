//! Batched data-path index commits for one submodule MDBX environment.

use super::WriteDataChunkError;
use irys_database::{
    db::IrysDatabaseExt as _,
    submodule::{add_data_path_hash_to_offset_index, add_full_data_path},
};
use irys_types::{ChunkDataPath, ChunkPathHash, PartitionChunkOffset, app_state::DatabaseProvider};
use std::{
    collections::HashSet,
    sync::{
        Arc, Condvar, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};
use tracing::{debug, error};

const MAX_BATCH_SIZE: usize = 64;

pub(super) struct IndexOp {
    pub(super) path_hash: ChunkPathHash,
    pub(super) data_path: ChunkDataPath,
    pub(super) offset: PartitionChunkOffset,
    pub(super) generation: u64,
    pub(super) done: mpsc::Sender<Result<(), WriteDataChunkError>>,
}

#[derive(Debug)]
struct InFlight {
    offsets: Mutex<HashSet<PartitionChunkOffset>>,
    cv: Condvar,
}

impl Default for InFlight {
    fn default() -> Self {
        Self {
            offsets: Mutex::new(HashSet::new()),
            cv: Condvar::new(),
        }
    }
}

#[derive(Debug)]
pub(super) struct IndexDrain {
    tx: Mutex<Option<mpsc::Sender<IndexOp>>>,
    join: Mutex<Option<JoinHandle<()>>>,
    in_flight: Arc<InFlight>,
}

#[cfg(test)]
pub(super) struct DrainTestHooks {
    pub(super) commit_count: Arc<AtomicU64>,
    pub(super) fail_next: Arc<AtomicBool>,
    pub(super) current_generation: Arc<AtomicU64>,
}

pub(super) struct DrainRunner {
    rx: mpsc::Receiver<IndexOp>,
    db: DatabaseProvider,
    current_generation: Arc<AtomicU64>,
    in_flight: Arc<InFlight>,
    fail_next: Arc<AtomicBool>,
    #[cfg(test)]
    hooks: Option<DrainTestHooks>,
}

impl IndexDrain {
    pub(super) fn spawn(
        db: DatabaseProvider,
        generation: Arc<AtomicU64>,
        fail_next: Arc<AtomicBool>,
    ) -> eyre::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let in_flight = Arc::new(InFlight::default());
        let runner = DrainRunner {
            rx,
            db,
            current_generation: Arc::clone(&generation),
            in_flight: Arc::clone(&in_flight),
            fail_next,
            #[cfg(test)]
            hooks: None,
        };
        let join = thread::Builder::new()
            .name("irys-index-drain".into())
            .spawn(move || runner.run())
            .map_err(|error| eyre::eyre!("failed to spawn index drain: {error}"))?;
        Ok(Self {
            tx: Mutex::new(Some(tx)),
            join: Mutex::new(Some(join)),
            in_flight,
        })
    }

    #[cfg(test)]
    pub(super) fn unstarted(db: DatabaseProvider, hooks: DrainTestHooks) -> (Self, DrainRunner) {
        let (tx, rx) = mpsc::channel();
        let in_flight = Arc::new(InFlight::default());
        (
            Self {
                tx: Mutex::new(Some(tx)),
                join: Mutex::new(None),
                in_flight: Arc::clone(&in_flight),
            },
            DrainRunner {
                rx,
                db,
                current_generation: Arc::clone(&hooks.current_generation),
                in_flight,
                fail_next: Arc::clone(&hooks.fail_next),
                hooks: Some(hooks),
            },
        )
    }

    pub(super) fn submit(&self, op: IndexOp) {
        let tx = self.tx.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(tx) = tx.as_ref() else {
            send_ack(
                op.done,
                Err(WriteDataChunkError::Other(eyre::eyre!(
                    "index drain closed"
                ))),
            );
            return;
        };
        if let Err(mpsc::SendError(op)) = tx.send(op) {
            send_ack(
                op.done,
                Err(WriteDataChunkError::Other(eyre::eyre!(
                    "index drain closed"
                ))),
            );
        }
    }

    pub(super) fn shutdown(&self) {
        drop(
            self.tx
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        let join = self
            .join
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(join) = join
            && join.join().is_err()
        {
            error!("index drain thread panicked");
        }
    }

    pub(super) fn wait_idle(&self) {
        self.wait_idle_in_range(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(u32::MAX),
        );
    }

    pub(super) fn wait_idle_in_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) {
        let mut offsets = self
            .in_flight
            .offsets
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while offsets
            .iter()
            .any(|offset| *offset >= start && *offset <= end)
        {
            offsets = self
                .in_flight
                .cv
                .wait(offsets)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl Drop for IndexDrain {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl DrainRunner {
    pub(super) fn run(self) {
        loop {
            let first = match self.rx.recv() {
                Ok(op) => op,
                Err(_) => break,
            };
            let mut batch = Vec::with_capacity(MAX_BATCH_SIZE);
            batch.push(first);
            while batch.len() < MAX_BATCH_SIZE {
                match self.rx.try_recv() {
                    Ok(op) => batch.push(op),
                    Err(_) => break,
                }
            }
            self.commit_batch(&mut batch);
        }
    }

    fn commit_batch(&self, batch: &mut Vec<IndexOp>) {
        if batch.is_empty() {
            return;
        }
        let current_generation = self.current_generation.load(Ordering::SeqCst);
        let mut apply = Vec::with_capacity(batch.len());
        for op in batch.drain(..) {
            if op.generation == current_generation {
                apply.push(op);
                continue;
            }
            send_ack(op.done, Err(WriteDataChunkError::WritesPaused));
        }
        if apply.is_empty() {
            return;
        }
        self.mark_in_flight(&apply);
        if self.current_generation.load(Ordering::SeqCst) != current_generation {
            self.clear_in_flight(&apply);
            for op in apply {
                send_ack(op.done, Err(WriteDataChunkError::WritesPaused));
            }
            return;
        }
        let result = self.write_batch(&apply);
        self.clear_in_flight(&apply);
        for op in apply {
            let ack = match &result {
                Ok(()) => Ok(()),
                Err(error) => Err(WriteDataChunkError::Other(eyre::eyre!("{error}"))),
            };
            send_ack(op.done, ack);
        }
    }

    fn mark_in_flight(&self, batch: &[IndexOp]) {
        let mut offsets = self
            .in_flight
            .offsets
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        offsets.extend(batch.iter().map(|op| op.offset));
    }

    fn clear_in_flight(&self, batch: &[IndexOp]) {
        let mut offsets = self
            .in_flight
            .offsets
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for op in batch {
            offsets.remove(&op.offset);
        }
        self.in_flight.cv.notify_all();
    }

    fn write_batch(&self, batch: &[IndexOp]) -> eyre::Result<()> {
        #[cfg(test)]
        if let Some(hooks) = &self.hooks {
            hooks.commit_count.fetch_add(1, Ordering::SeqCst);
        }
        if self.fail_next.swap(false, Ordering::SeqCst) {
            eyre::bail!("injected index drain commit failure");
        }
        self.db.update_eyre(|tx| {
            for op in batch {
                // clone: put consumes the path bytes; the IndexOp is kept for the waiter ACK
                add_full_data_path(tx, op.path_hash, op.data_path.clone())?;
                add_data_path_hash_to_offset_index(tx, op.offset, Some(op.path_hash))?;
            }
            Ok(())
        })
    }
}

fn send_ack(
    done: mpsc::Sender<Result<(), WriteDataChunkError>>,
    ack: Result<(), WriteDataChunkError>,
) {
    if done.send(ack).is_err() {
        debug!("index drain waiter dropped");
    }
}

#[cfg(test)]
mod tests {
    use super::{DrainTestHooks, IndexDrain, IndexOp};
    use crate::WriteDataChunkError;
    use irys_database::{
        IrysDatabaseArgs as _,
        db::IrysDatabaseExt as _,
        submodule::{create_or_open_submodule_db, get_path_hashes_by_offset},
    };
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{PartitionChunkOffset, UnpackedChunk, app_state::DatabaseProvider};
    use reth_db::mdbx::DatabaseArguments;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    };

    fn open_submodule_db(path: &std::path::Path) -> eyre::Result<DatabaseProvider> {
        let env = create_or_open_submodule_db(path, DatabaseArguments::irys_testing()?)?;
        Ok(DatabaseProvider(Arc::new(env)))
    }

    fn submit_op(
        drain: &IndexDrain,
        offset: u32,
        data_path: Vec<u8>,
        generation: u64,
    ) -> mpsc::Receiver<Result<(), WriteDataChunkError>> {
        let (done_tx, done_rx) = mpsc::channel();
        drain.submit(IndexOp {
            path_hash: UnpackedChunk::hash_data_path(&data_path),
            data_path,
            offset: PartitionChunkOffset::from(offset),
            generation,
            done: done_tx,
        });
        done_rx
    }

    fn path_hash_at(
        db: &DatabaseProvider,
        offset: u32,
    ) -> eyre::Result<Option<irys_types::ChunkPathHash>> {
        db.view_eyre(|tx| {
            Ok(
                get_path_hashes_by_offset(tx, PartitionChunkOffset::from(offset))?
                    .and_then(|hashes| hashes.data_path_hash),
            )
        })
    }

    #[test]
    fn two_ops_queued_before_drain_share_one_commit() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new()
            .prefix("index_drain_share_commit")
            .with_tracing()
            .build();
        let db = open_submodule_db(&tmp.path().join("db"))?;
        let commit_count = Arc::new(AtomicU64::new(0));
        let (drain, runner) = IndexDrain::unstarted(
            db.clone(),
            DrainTestHooks {
                commit_count: Arc::clone(&commit_count),
                fail_next: Arc::new(AtomicBool::new(false)),
                current_generation: Arc::new(AtomicU64::new(0)),
            },
        );

        let first_path = vec![1_u8, 2, 3];
        let second_path = vec![4_u8, 5, 6];
        let first_hash = UnpackedChunk::hash_data_path(&first_path);
        let second_hash = UnpackedChunk::hash_data_path(&second_path);
        let first_done = submit_op(&drain, 0, first_path, 0);
        let second_done = submit_op(&drain, 1, second_path, 0);
        drop(drain);

        runner.run();

        assert_eq!(commit_count.load(Ordering::SeqCst), 1);
        first_done.recv()?.map_err(|err| eyre::eyre!("{err}"))?;
        second_done.recv()?.map_err(|err| eyre::eyre!("{err}"))?;
        assert_eq!(path_hash_at(&db, 0)?, Some(first_hash));
        assert_eq!(path_hash_at(&db, 1)?, Some(second_hash));
        Ok(())
    }

    #[test]
    fn failed_commit_fails_every_waiter() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new()
            .prefix("index_drain_shared_failure")
            .with_tracing()
            .build();
        let db = open_submodule_db(&tmp.path().join("db"))?;
        let commit_count = Arc::new(AtomicU64::new(0));
        let fail_next = Arc::new(AtomicBool::new(true));
        let (drain, runner) = IndexDrain::unstarted(
            db.clone(),
            DrainTestHooks {
                commit_count: Arc::clone(&commit_count),
                fail_next: Arc::clone(&fail_next),
                current_generation: Arc::new(AtomicU64::new(0)),
            },
        );

        let first_done = submit_op(&drain, 0, vec![1_u8, 2, 3], 0);
        let second_done = submit_op(&drain, 1, vec![4_u8, 5, 6], 0);
        drop(drain);

        runner.run();

        assert!(first_done.recv()?.is_err());
        assert!(second_done.recv()?.is_err());
        assert_eq!(path_hash_at(&db, 0)?, None);
        assert_eq!(path_hash_at(&db, 1)?, None);
        Ok(())
    }

    #[test]
    fn queued_write_is_dropped_across_pause_clear_resume() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new()
            .prefix("index_drain_pause_generation")
            .with_tracing()
            .build();
        let db = open_submodule_db(&tmp.path().join("db"))?;
        let current_generation = Arc::new(AtomicU64::new(1));
        let path = vec![7_u8, 8, 9];
        let (drain, runner) = IndexDrain::unstarted(
            db.clone(),
            DrainTestHooks {
                commit_count: Arc::new(AtomicU64::new(0)),
                fail_next: Arc::new(AtomicBool::new(false)),
                current_generation: Arc::clone(&current_generation),
            },
        );
        let done = submit_op(&drain, 0, path, 1);
        current_generation.store(2, Ordering::SeqCst);
        drop(drain);
        runner.run();

        assert!(matches!(
            done.recv()?,
            Err(WriteDataChunkError::WritesPaused)
        ));
        assert_eq!(path_hash_at(&db, 0)?, None);

        let (drain, runner) = IndexDrain::unstarted(
            db.clone(),
            DrainTestHooks {
                commit_count: Arc::new(AtomicU64::new(0)),
                fail_next: Arc::new(AtomicBool::new(false)),
                current_generation: Arc::clone(&current_generation),
            },
        );
        let resume_path = vec![9_u8, 8, 7];
        let resume_hash = UnpackedChunk::hash_data_path(&resume_path);
        let resumed = submit_op(&drain, 0, resume_path, 2);
        drop(drain);
        runner.run();
        resumed.recv()?.map_err(|err| eyre::eyre!("{err}"))?;
        assert_eq!(path_hash_at(&db, 0)?, Some(resume_hash));
        Ok(())
    }
}
