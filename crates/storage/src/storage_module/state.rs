use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek as _, SeekFrom, Write},
    path::PathBuf,
    sync::{Arc, RwLock, RwLockWriteGuard},
    thread::sleep,
    time::Duration,
};

use irys_types::{CHUNK_SIZE, NUM_CHUNKS_IN_RECALL_RANGE};
use nodit::{
    interval::{ie, ii},
    InclusiveInterval, Interval, NoditMap,
};

use tracing::{error, warn, Level};
use tracing_subscriber::FmtSubscriber;

use crate::{interval::WriteLock, provider::generate_chunk_test_data};

use super::interval::{
    get_duration_since_epoch, get_now, IntervalState, IntervalStateWrapped, PackingState,
};

#[derive(Debug)]
/// Struct representing all the state of a storage module
pub struct StorageModule {
    /// Path of the SM's storage file
    pub path: PathBuf,
    /// Non-overlapping interval map representing the states of different segments of the SM
    pub interval_map: NoditMap<u32, Interval<u32>, RwLock<IntervalStateWrapped>>,
    /// capacity (in chunks) allocated to this storage module
    pub capacity: u32,
    // Chunks per interval
    pub interval_width: u32,
}

const STALE_LOCK_TIMEOUT_MS: u64 = 60_000;

impl StorageModule {
    /// Create a new SM instance at the specified path with the specified capacity (in bytes)
    /// the file will be 0-allocated if it doesn't exist
    pub fn new(path: PathBuf, capacity_bytes: u64, interval_width_chunks: u32) -> Self {
        let capacity_chunks = (capacity_bytes / CHUNK_SIZE) as u32;
        let intervals = capacity_chunks.div_ceil(interval_width_chunks);
        // TODO @JesseTheRobot - disk logic
        let mut map = NoditMap::new();
        let mut mapped_intervals = 0;
        while mapped_intervals < intervals {
            let b = mapped_intervals * interval_width_chunks;
            map.insert_strict(
                ie(b, b + interval_width_chunks),
                RwLock::new(IntervalStateWrapped::new(IntervalState {
                    state: PackingState::Unpacked,
                })),
            )
            .unwrap();
            mapped_intervals = mapped_intervals + 1;
        }
        // sparse allocate the file
        // some OS's won't 0-fill, but that's fine as we don't expect to be able to always read 0 from uninitialized space
        if !path.exists() {
            let mut file = File::create("sparse.rs").unwrap();
            file.seek(SeekFrom::Start(capacity_chunks as u64 * CHUNK_SIZE))
                .unwrap();
            file.write_all(&[0]).unwrap();
        }

        // TODO @JesseTheRobot - 0 allocate the file if it doesn't exist
        return StorageModule {
            path: path,
            interval_width: interval_width_chunks,
            interval_map: map,
            capacity: capacity_chunks,
        };
    }

    /// Acquire a File handle for the SM's path
    pub fn get_handle(&self) -> Result<File, std::io::Error> {
        File::open(&self.path)
    }

    /// read some chunks with a sm-relative offset
    pub fn read_chunks(
        &self,
        interval: Interval<u32>,
        expected_state: Option<PackingState>,
    ) -> eyre::Result<Vec<[u8; CHUNK_SIZE as usize]>> {
        let chunks_to_read = interval.width() + 1;
        let mut chunks_read: u32 = 0;
        let mut handle = self.get_handle()?;

        // move the handle to the requested start
        if interval.start() > 0 {
            handle.seek(SeekFrom::Start(interval.start() as u64 * CHUNK_SIZE))?;
        }
        let overlap_iter = self.interval_map.overlapping(interval);

        let mut result = Vec::with_capacity(chunks_to_read.try_into()?);

        for (segment_interval, segment_state) in overlap_iter {
            error!("trying to unlock {:?}...", &segment_interval);
            let lock = segment_state.read().unwrap();
            error!("unlocked {:?}!", &segment_interval);

            match lock.state {
                PackingState::WriteLocked => {
                    return Err(eyre::Report::msg("illegal range state: WriteLocked"))
                }
                _ => (),
            }
            if expected_state.is_some()
                && std::mem::discriminant(&lock.state)
                    != std::mem::discriminant(&expected_state.unwrap())
            {
                return Err(eyre::Report::msg(format!(
                    "Segment {:?} is of unexpected state {:?}",
                    segment_interval, lock.state
                )));
            }
            // while we have chunks to read and we're still in this segment
            while chunks_read < chunks_to_read
                && (interval.start() + chunks_read <= segment_interval.end())
            {
                // read 1 chunk
                let mut buf: [u8; CHUNK_SIZE as usize] = [0; CHUNK_SIZE as usize];
                error!(
                    "handle pos: {:?}, path: {:?}",
                    handle.stream_position()?,
                    &self.path
                );

                handle.read_exact(&mut buf)?;
                result.push(buf);
                chunks_read = chunks_read + 1;
            }
        }

        return Ok(result);
    }

    /// Scan for an "clean" any stale locks
    /// this function will reset the intervals that are under a stale lock by marking them as unpacked
    /// as there's no definitive way to recover otherwise
    // pub fn clean_stale_locks(&self) -> eyre::Result<()> {
    //     let interval_map = self.interval_map;
    //     let now = get_now();
    //     let mut stale_locks = vec![];
    //     for (interval, state) in interval_map.iter() {
    //         match &state.read().unwrap().state {
    //             PackingState::WriteLocked(wl_state) => {
    //                 // check if the write lock was refreshed > CHUNK_WL_SECS ago
    //                 if (now - wl_state.refreshed_at) > STALE_LOCK_TIMEOUT_MS {
    //                     // this lock is stale
    //                     warn!(
    //                         "Stale lock for interval {:?}, initially locked at {:?} storage module path {:?}",
    //                         &interval, &wl_state.locked_at,  &self.path
    //                     );
    //                     stale_locks.push(interval);
    //                 }
    //             }
    //             _ => (),
    //         }
    //     }
    //     // clear the stale locks by setting their intervals to "unpacked", effectively clearing them
    //     // we do a read then write pass as the most likely path is that the read finds no stale locks
    //     // so this is done to prevent unneeded write locking of the interval map
    //     // let mut interval_map_w = self.interval_map.write().unwrap();
    //     for invalid_interval in stale_locks.iter() {
    //         // let _cut = interval.insert_overwrite(
    //         //     **invalid_interval,
    //         //     IntervalStateWrapped::new(IntervalState {
    //         //         state: PackingState::Unpacked,
    //         //     }),
    //         // );
    //     }
    //     Ok(())
    // }

    /// Writes some chunks to an interval, and tags the interval with the provided new state
    pub fn write_chunks(
        &self,
        chunks: Vec<[u8; CHUNK_SIZE as usize]>,
        interval: Interval<u32>,
        expected_state: PackingState,
        new_state: IntervalState,
    ) -> eyre::Result<()> {
        // let write_start_time = get_duration_since_epoch();

        if interval.end() > self.capacity {
            return Err(eyre::Report::msg(
                "Storage module is too small for write interval",
            ));
        }

        let overlap_iter = self.interval_map.overlapping(interval);

        let chunks_to_write = interval.width() + 1;

        // TODO QUESTION - should we enforce that the chunks vec has to be the same length as the interval's width?
        if chunks.len() < chunks_to_write as usize {
            return Err(eyre::Report::msg("Chunks vec is too small for interval"));
        }

        let mut chunks_written = 0;

        let mut handle = OpenOptions::new()
            .write(true)
            .create(true)
            .open(&self.path)?; /* File::create(&self.path)?; */

        // move the handle to the requested start
        if interval.start() > 0 {
            handle.seek(SeekFrom::Start(interval.start() as u64 * CHUNK_SIZE))?;
        }

        for (segment_interval, segment_state) in overlap_iter {
            let mut lock = segment_state.write().unwrap();

            // TODO REMOVE ME
            error!("locked range {:?} for write", &segment_interval);
            sleep(Duration::from_secs(5));

            match lock.state {
                PackingState::WriteLocked => {
                    return Err(eyre::Report::msg("illegal range state: acquired lock"))
                }
                _ => (),
            }
            if std::mem::discriminant(&lock.state) != std::mem::discriminant(&expected_state) {
                return Err(eyre::Report::msg(format!(
                    "Segment {:?} is of unexpected state {:?}",
                    segment_interval, lock.state
                )));
            }
            lock.state = PackingState::WriteLocked;

            while chunks_written < chunks_to_write
                && (interval.start() + chunks_written <= segment_interval.end())
            {
                // TODO @JesseTheRobot use vectored read and write
                error!("Writing to {:?}", &handle.stream_position()?);
                sleep(Duration::from_millis(50));
                let written_bytes = handle.write(chunks.get(chunks_written as usize).unwrap())?;
                error!("written bytes {:?} to {:?}", &written_bytes, &interval);
                chunks_written = chunks_written + 1;
            }
            lock.inner = new_state.clone();

            error!("unlocked range {:?} for write", &segment_interval);
        }

        Ok(())
    }
}

/// Resets an interval to `PackingState::Unpacked`
pub fn reset_range(
    mut map: RwLockWriteGuard<'_, NoditMap<u32, Interval<u32>, IntervalStateWrapped>>,
    interval: Interval<u32>,
) {
    let _ = map.insert_overwrite(
        interval,
        IntervalStateWrapped::new(IntervalState {
            state: PackingState::Unpacked,
        }),
    );
}

#[test]
fn test_sm_rw() -> eyre::Result<()> {
    FmtSubscriber::builder().compact().init();

    let chunks: u8 = 20;
    generate_chunk_test_data("/tmp/sample".into(), chunks, 0)?;
    // let sm1_interval_map = NoditMap::from_slice_strict([(
    //     ie(0, chunks as u32),
    //     IntervalStateWrapped::new(IntervalState {
    //         state: PackingState::Data,
    //     }),
    // )])
    // .unwrap();

    let sm1 = Arc::new(StorageModule::new(
        "/tmp/sample".into(),
        100 * CHUNK_SIZE,
        1,
    ));

    let read1 = sm1.read_chunks(ii(0, 5), Some(PackingState::Unpacked))?;

    let sm2 = sm1.clone();

    std::thread::spawn(move || {
        sm2.write_chunks(
            vec![[69; CHUNK_SIZE as usize], [70; CHUNK_SIZE as usize]],
            ii(4, 5),
            PackingState::Unpacked,
            IntervalState {
                state: PackingState::Packed,
            },
        )
        .unwrap();
    });

    sleep(Duration::from_millis(1000));

    let read2 = sm1.read_chunks(ii(0, 5), None)?;
    dbg!(&read2, &read1);
    // let mut sp = StorageProvider {
    //     sm_map: NoditMap::from_slice_strict([(
    //         ie(0, chunks as u32),
    //         StorageModule {
    //             path: "/tmp/sample".into(),
    //             interval_map: RwLock::new(sm1_interval_map),
    //             capacity: chunks as u32,
    //         },
    //     )])
    //     .unwrap(),
    // };

    // let sm = StorageModule {
    //     path: "/tmp/sample".into(),
    //     interval_map: RwLock::new(sm1_interval_map),
    //     capacity: chunks as u32,
    // };

    // let interval = ii(1, 1);

    // let chunks = sm.read_chunks(interval)?;

    // sm.write_chunks(
    //     vec![[69; CHUNK_SIZE as usize]],
    //     interval,
    //     IntervalStateWrapped::new(IntervalState {
    //         state: PackingState::Packed,
    //     }),
    // )?;

    // let chunks2 = sm.read_chunks(interval)?;

    Ok(())
}

// #[test]
// fn vec_test() {
//     let chunks_per_recall_range = NUM_CHUNKS_IN_RECALL_RANGE;
//     let chunks  = 100;
//     let intervals =
//     let mut intervals = vec![];
// }
