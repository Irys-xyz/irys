use eyre::eyre;
use irys_types::CHUNK_SIZE;
use nodit::{interval::ii, InclusiveInterval, Interval, NoditMap};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::{
    fs::{remove_dir_all, File},
    io::{Read, Seek, SeekFrom, Write as _},
    os::unix::fs::FileExt,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use crate::{
    interval::{IntervalState, PackingState},
    state::{StorageModule, StorageModuleConfig},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Storage provider - a wrapper struct around many storage modules
pub struct StorageProvider {
    /// map of intervals to backing storage modules
    pub sm_map: NoditMap<u32, Interval<u32>, StorageModule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Storage provider config
pub struct StorageProviderConfig {
    /// vec of intervals to storage module configurations
    pub sm_paths_offsets: Vec<(Interval<u32>, StorageModuleConfig)>,
}

impl StorageProvider {
    /// Initializes a storage provider from a config
    pub fn from_config(config: StorageProviderConfig) -> eyre::Result<Self> {
        let mut map = NoditMap::new();
        for (i, cfg) in config.sm_paths_offsets {
            map.insert_strict(i, StorageModule::new_or_load_from_disk(cfg.clone())?)
                .unwrap();
        }
        // TODO @JesseTheRobot assert there are no gaps, assert the SM's combine to provide enough storage for the partition

        return Ok(Self { sm_map: map });
    }

    /// read a range of chunks using their partition-relative offsets - **start is inclusive, end is exclusive**, so (0, 1) will read just chunk 0, and (3, 5) will read chunks 3 and 4
    pub fn read_chunks(
        &mut self,
        interval: Interval<u32>,
        expected_state: Option<PackingState>,
    ) -> eyre::Result<Vec<[u8; CHUNK_SIZE as usize]>> {
        // figure out what SMs we need
        let read_interval = interval; /* ie(start, end); */
        // intervals are interally represented as inclusive-inclusive
        let sm_iter = self.sm_map.overlapping(read_interval);
        // TODO: read in parallel, probably use streams?
        // also use a read-handle pool (removes opening delay), with a dedicated read handle for mining w/ a higher prio
        let mut result: Vec<[u8; CHUNK_SIZE as usize]> =
            Vec::with_capacity(read_interval.width() as usize);
        for (sm_interval, sm) in sm_iter {
            // storage modules use relative 0 based offsets, as that's what works well with files
            let sm_offset = sm_interval.start();
            // start reading from either the interval start (always 0) or the read interval start, depending on which is greater
            let sm_relative_start_offset =
                sm_interval.start().max(read_interval.start()) - sm_offset;

            // finish reading at the end of the SM, or the read interval end, whichever is lesser
            let sm_relative_end_offset: u32 =
                sm_interval.end().min(read_interval.end()) - sm_offset;

            let mut sm_read = sm.read_chunks(
                ii(sm_relative_start_offset, sm_relative_end_offset),
                expected_state,
            )?;
            result.append(&mut sm_read)
        }
        return Ok(result);
    }

    /// write a vec of chunks to a partition-relative interval
    pub fn write_chunks(
        &mut self,
        chunks: Vec<[u8; CHUNK_SIZE as usize]>,
        interval: Interval<u32>,
        expected_state: PackingState,
        new_state: IntervalState,
    ) -> eyre::Result<()> {
        let sm_iter = self.sm_map.overlapping_mut(interval);

        if chunks.len() < (interval.width() + 1) as usize {
            return Err(eyre!("Chunks vec is too small for interval"));
        }

        for (sm_interval, sm) in sm_iter {
            let sm_offset = sm_interval.start();
            // start writing from either the interval start (always 0) or the read interval start, depending on which is greater
            let clamped_start = sm_interval.start().max(interval.start());
            let sm_relative_start_offset = clamped_start - sm_offset;

            // finish writing at the end of the SM, or the read interval end, whichever is lesser
            let clamped_end = sm_interval.end().min(interval.end());
            let sm_relative_end_offset: u32 = clamped_end - sm_offset;

            // figure out what chunks indexes we need
            let cls = clamped_start - interval.start();
            let cle = clamped_end - interval.start();

            sm.write_chunks(
                chunks[cls as usize..=cle as usize].to_vec(),
                ii(sm_relative_start_offset, sm_relative_end_offset),
                expected_state,
                new_state.clone(),
            )?
        }

        Ok(())
    }

    // pub fn new()
}

#[test]
fn basic_storage_provider_test() -> eyre::Result<()> {
    let config = StorageProviderConfig {
        sm_paths_offsets: vec![
            (
                ii(0, 3),
                StorageModuleConfig {
                    directory_path: "/tmp/sm/sm1".into(),
                    size_bytes: 10 * CHUNK_SIZE,
                },
            ),
            (
                ii(4, 10),
                StorageModuleConfig {
                    directory_path: "/tmp/sm/sm2".into(),
                    size_bytes: 10 * CHUNK_SIZE,
                },
            ),
        ],
    };
    let _ = remove_dir_all("/tmp/sm/");
    let mut storage_provider = StorageProvider::from_config(config)?;

    let chunks = storage_provider.read_chunks(ii(0, 10), Some(PackingState::Unpacked))?;

    let chunks_to_write = vec![
        [69; CHUNK_SIZE as usize],
        [70; CHUNK_SIZE as usize],
        [71; CHUNK_SIZE as usize],
    ];

    storage_provider.write_chunks(
        chunks_to_write.clone(),
        ii(3, 5),
        PackingState::Unpacked,
        IntervalState {
            state: PackingState::Packed,
        },
    )?;

    let after_write = storage_provider.read_chunks(ii(0, 10), None)?;

    let empty_chunk = [0; CHUNK_SIZE as usize];
    assert_eq!(chunks[3], empty_chunk);
    assert_eq!(chunks[4], empty_chunk);
    assert_eq!(chunks[5], empty_chunk);

    assert_eq!(after_write[3], chunks_to_write[0]);
    assert_eq!(after_write[4], chunks_to_write[1]);
    assert_eq!(after_write[5], chunks_to_write[2]);

    Ok(())
}

/// Writes N random chunks to the specified path, filled with a specific value (count + offset) so you know if the correct chunk(s) are being read
pub fn generate_chunk_test_data(path: PathBuf, chunks: u8, offset: u8) -> eyre::Result<()> {
    let mut chunks_done = 0;
    let mut handle = File::create(path)?;
    // let mut rng = rand::thread_rng();
    // let mut chunk = [0; CHUNK_SIZE as usize];
    while chunks_done < chunks {
        // rng.fill_bytes(&mut chunk);
        // handle.write(&chunk)?;

        handle.write([offset + chunks_done as u8; CHUNK_SIZE as usize].as_ref())?;

        chunks_done = chunks_done + 1;
    }
    Ok(())
}
