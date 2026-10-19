//! Production module open does not start an index-drain thread.
//!
//! This binary links `irys-domain` without `cfg(test)`, so it runs the open
//! path a node uses. Presence commits stay on the disk lane. Packing still
//! has to finish: chunk I/O must not wait for a drain that was never started.

use irys_domain::{StorageModule, StorageModuleInfo};
use irys_testing_utils::utils::TempDirBuilder;
use irys_types::{
    Config, ConsensusConfig, ConsensusOptions, DataLedger, NodeConfig, PartitionChunkOffset,
    partition::PartitionAssignment, partition_chunk_offset_ii,
};
use nodit::interval::ii;
use std::time::{Duration, Instant};

/// Linux `comm` keeps 15 bytes. `irys-index-drain` is longer than that.
fn index_drain_threads() -> usize {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return 0;
    };
    tasks
        .flatten()
        .filter(|entry| {
            std::fs::read_to_string(entry.path().join("comm"))
                .unwrap_or_default()
                .starts_with("irys-index-dra")
        })
        .count()
}

#[test]
fn production_open_does_not_spawn_an_index_drain_thread() -> eyre::Result<()> {
    let before = index_drain_threads();
    let tmp = TempDirBuilder::new().prefix("index_drain_no_spawn").build();
    let mut node_config = NodeConfig::testing();
    node_config.consensus = ConsensusOptions::Custom(ConsensusConfig {
        chunk_size: 32,
        num_chunks_in_partition: 4,
        entropy_packing_iterations: 1,
        ..node_config.consensus_config()
    });
    node_config.base_directory = tmp.path().to_path_buf();
    let config = Config::new_with_random_peer_id(node_config);
    let storage = StorageModule::new(
        &StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: irys_types::H256::repeat_byte(0x11),
                miner_address: config.node_config.miner_address(),
                ledger_id: Some(DataLedger::Submit.into()),
                slot_index: Some(0),
            }),
            submodules: vec![
                (partition_chunk_offset_ii!(0, 1), tmp.path().join("hdd0")),
                (partition_chunk_offset_ii!(2, 3), tmp.path().join("hdd1")),
            ],
        },
        &config,
    )?;

    let spawned = index_drain_threads().saturating_sub(before);
    assert_eq!(
        spawned, 0,
        "open started {spawned} index-drain threads for two submodules"
    );

    // Two submodules, four chunks. This reaches `hold_for_chunk_io`. A drain
    // that was registered and never reached `recv` would wait here.
    let started = Instant::now();
    storage.pack_with_zeros();
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "packing did not finish without an index-drain thread"
    );
    assert_eq!(
        index_drain_threads().saturating_sub(before),
        0,
        "packing started an index-drain thread"
    );
    drop(storage);
    assert_eq!(
        index_drain_threads().saturating_sub(before),
        0,
        "drop left an index-drain thread"
    );
    Ok(())
}
