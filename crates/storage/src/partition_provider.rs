use eyre::eyre;
use irys_types::{
    ChunkState, IntervalState, PartitionStorageProviderConfig, StorageModuleConfig, CHUNK_SIZE,
};
use nodit::{interval::ii, InclusiveInterval, Interval, NoditMap};
use serde::{Deserialize, Serialize};
use std::{
    fs::{remove_dir_all, File},
    io::Write as _,
    path::PathBuf,
    sync::Arc,
};
use tracing::error;
use tracing_subscriber::util::SubscriberInitExt;

use crate::state::StorageModule;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
/// Storage provider - a wrapper struct around many storage modules
pub struct PartitionStorageProvider {
    /// map of intervals to backing storage modules
    pub sm_map: NoditMap<u32, Interval<u32>, Arc<StorageModule>>,
}

/// A storage provider provides a high level read/write interface over a backing set of storage modules, routing read and write requests to the correct SM
impl PartitionStorageProvider {
    /// Initializes a storage provider from a config
    pub fn from_config(config: PartitionStorageProviderConfig) -> eyre::Result<Self> {
        let mut map = NoditMap::new();
        for (i, cfg) in config.sm_paths_offsets {
            map.insert_strict(
                i,
                Arc::new(StorageModule::new_or_load_from_disk(cfg.clone())?),
            )
            .unwrap();
        }
        // TODO @JesseTheRobot assert there are no gaps, assert the SM's combine to provide enough storage for the partition

        return Ok(Self { sm_map: map });
    }

    /// read a range of chunks using their partition-relative offsets
    pub fn read_chunks(
        &self,
        read_interval: Interval<u32>,
        expected_state: Option<ChunkState>,
    ) -> eyre::Result<Vec<[u8; CHUNK_SIZE as usize]>> {
        // figure out what SMs we need
        let sm_iter = self.sm_map.overlapping(read_interval);
        // TODO: read in parallel, probably use streams?
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
        &self,
        chunks: Vec<[u8; CHUNK_SIZE as usize]>,
        write_interval: Interval<u32>,
        expected_state: ChunkState,
        new_state: IntervalState,
    ) -> eyre::Result<()> {
        let sm_iter = self.sm_map.overlapping(write_interval);

        if chunks.len() < (write_interval.width() + 1) as usize {
            return Err(eyre!("Chunks vec is too small for interval"));
        }

        for (sm_interval, sm) in sm_iter {
            let sm_offset = sm_interval.start();
            // start writing from either the sm_interval start (always 0) or the write interval start, depending on which is greater
            let clamped_start = sm_interval.start().max(write_interval.start());
            let sm_relative_start_offset = clamped_start - sm_offset;

            // finish writing at the end of the SM, or the write interval end, whichever is lesser
            let clamped_end = sm_interval.end().min(write_interval.end());
            let sm_relative_end_offset: u32 = clamped_end - sm_offset;

            // figure out what chunks indexes we need
            let cls = clamped_start - write_interval.start();
            let cle = clamped_end - write_interval.start();

            sm.write_chunks(
                chunks[cls as usize..=cle as usize].to_vec(),
                ii(sm_relative_start_offset, sm_relative_end_offset),
                expected_state,
                new_state.clone(),
            )?
        }

        Ok(())
    }
}

#[test]
fn basic_storage_provider_test() -> eyre::Result<()> {
    tracing_subscriber::FmtSubscriber::new().init();

    let config = PartitionStorageProviderConfig {
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
    let mut storage_provider = PartitionStorageProvider::from_config(config)?;

    let chunks = storage_provider.read_chunks(ii(0, 10), Some(ChunkState::Unpacked))?;

    let chunks_to_write = vec![
        [69; CHUNK_SIZE as usize],
        [70; CHUNK_SIZE as usize],
        [71; CHUNK_SIZE as usize],
    ];

    storage_provider.write_chunks(
        chunks_to_write.clone(),
        ii(3, 5),
        ChunkState::Unpacked,
        IntervalState {
            chunk_state: ChunkState::Packed,
        },
    )?;

    error!("after write!");
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
