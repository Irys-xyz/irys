use std::{fs::remove_dir_all, path::PathBuf};

use irys_storage::ii;
use irys_storage::{ie, partition_provider::PartitionStorageProvider, state::StorageModule};
use irys_types::{
    block_production::Partition, Interval, PartitionStorageProviderConfig, StorageModuleConfig,
    CHUNK_SIZE,
};
use irys_types::{ChunkState, IntervalState};
use rand::Rng;

#[derive(Debug, Clone)]
pub struct PartitionTestConfig {
    chunk_capacity: u32,
    /// whether to prewrite chunks in an "offset pattern", i.e write the offset value of the chunk to the entire chunk
    prewrite_offset_pattern: bool,
    storage_modules: u32,
    test_path: PathBuf,
}

impl Default for PartitionTestConfig {
    fn default() -> Self {
        let mut rng = rand::thread_rng();
        let random_id = rng.gen_range(1..=1000);
        Self {
            chunk_capacity: 100,
            prewrite_offset_pattern: true,
            storage_modules: 2,
            test_path: PathBuf::from("/tmp/").join(random_id.to_string()),
        }
    }
}

pub fn generate_partition_and_storage_provider(
    config: PartitionTestConfig,
) -> eyre::Result<(CleanupGuard, (Partition, PartitionStorageProvider))> {
    let partition = Partition::default();
    let PartitionTestConfig {
        chunk_capacity,
        prewrite_offset_pattern,
        storage_modules,
        test_path,
    } = config;
    let mut sm_paths_offsets: Vec<(Interval<u32>, StorageModuleConfig)> =
        Vec::with_capacity(storage_modules as usize);
    let chunks_per_sm = chunk_capacity / storage_modules;
    for i in 0..storage_modules {
        let sm_interval = ie(i * chunks_per_sm, (i * chunks_per_sm) + chunks_per_sm);
        let sm_config = StorageModuleConfig {
            directory_path: test_path.join("/sm/").join(i.to_string()),
            size_bytes: CHUNK_SIZE * chunks_per_sm as u64,
        };

        if prewrite_offset_pattern {
            let mut sm = StorageModule::new_or_load_from_disk(sm_config.clone())?;
            let mut chunks_done = 0;

            while chunks_done < chunks_per_sm {
                let o = sm_interval.start() + chunks_done;
                // TODO @JesseTheRobot detect when we overflow u8
                sm.write_chunks(
                    vec![[o as u8; CHUNK_SIZE as usize]],
                    ii(o, o),
                    ChunkState::Unpacked,
                    IntervalState {
                        chunk_state: ChunkState::Data,
                    },
                )?;
                chunks_done = chunks_done + 1;
            }
        }
        sm_paths_offsets.push((sm_interval, sm_config));
    }
    let storage_provider =
        PartitionStorageProvider::from_config(PartitionStorageProviderConfig { sm_paths_offsets })?;

    return Ok((
        CleanupGuard { dir: test_path },
        (partition, storage_provider),
    ));
}

// rust is cool :sunglasses:
pub struct CleanupGuard {
    dir: PathBuf,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        dbg!("CleanupGuard: removing dir {}", &self.dir);
        remove_dir_all(&self.dir).unwrap()
    }
}
