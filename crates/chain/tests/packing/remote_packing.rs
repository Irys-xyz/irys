use std::{net::SocketAddr, num::NonZero, sync::Arc, time::Duration};

use irys_actors::packing::{wait_for_packing, PackingRequest};
use irys_domain::{ChunkType, StorageModule, StorageModuleInfo};
use irys_packing::capacity_single::compute_entropy_chunk;
use irys_packing_worker::worker::start_worker;
use irys_testing_utils::{initialize_tracing, setup_tracing_and_temp_dir};
use irys_types::{
    ie,
    partition::{PartitionAssignment, PartitionHash},
    remote_packing::PackingWorkerConfig,
    Config, ConsensusConfig, NodeConfig, PartitionChunkOffset, PartitionChunkRange,
    RemotePackingConfig, StorageSyncConfig,
};
use irys_utils::listener::create_listener;
use tokio::sync::mpsc::channel;

#[actix::test]
pub async fn heavy_packing_worker_full_node_test() -> eyre::Result<()> {
    std::env::set_var("RUST_LOG", "debug");
    initialize_tracing();
    let packing_config = PackingWorkerConfig {
        bind_port: 0,
        bind_addr: "127.0.0.1".to_string(),
        cpu_packing_concurrency: 2,
        gpu_packing_batch_size: 0,
        max_pending: NonZero::new(1).unwrap(),
    };

    let addr: SocketAddr = format!(
        "{}:{}",
        &packing_config.bind_addr, &packing_config.bind_port
    )
    .parse()?;
    let listener = create_listener(addr)?;

    let local_addr = listener.local_addr()?;

    let (tx, rx) = channel(1);
    let exit_handle = tokio::spawn(start_worker(packing_config.clone(), listener, rx));

    // setup
    let partition_hash = PartitionHash::zero();
    let num_chunks = 50;
    let to_pack = 10;
    let packing_end = num_chunks - to_pack;

    let tmp_dir = setup_tracing_and_temp_dir(Some("test_packing_actor"), false);

    let base_path = tmp_dir.path().to_path_buf();
    let node_config = NodeConfig {
        consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
            entropy_packing_iterations: 1000,
            num_chunks_in_partition: num_chunks,
            chunk_size: 32,
            ..ConsensusConfig::testing()
        }),
        storage: StorageSyncConfig {
            num_writes_before_sync: 1,
        },
        packing: irys_types::PackingConfig {
            local: irys_types::LocalPackingConfig {
                cpu_packing_concurrency: 0,
                gpu_packing_batch_size: 0,
            },
            remote: vec![RemotePackingConfig {
                url: format!("http://{}:{}", &local_addr.ip(), &local_addr.port()),
                timeout: Some(Duration::from_secs(10)),
            }],
        },
        base_directory: base_path.clone(),
        ..NodeConfig::testing()
    };
    let config = Config::new(node_config);

    let infos = [StorageModuleInfo {
        id: 0,
        partition_assignment: Some(PartitionAssignment {
            partition_hash,
            miner_address: config.node_config.miner_address(),
            ledger_id: None,
            slot_index: None,
        }),
        submodules: vec![(
            irys_types::partition_chunk_offset_ie!(0, num_chunks),
            "hdd0".into(),
        )],
    }];
    // Create a StorageModule with the specified submodules and config
    let storage_module_info = &infos[0];
    let storage_module = Arc::new(StorageModule::new(storage_module_info, &config)?);

    let request = PackingRequest {
        storage_module: storage_module.clone(),
        chunk_range: PartitionChunkRange(irys_types::partition_chunk_offset_ie!(0, packing_end)),
    };
    // Create an instance of the packing service
    let packing = irys_actors::packing::PackingService::new(Arc::new(config.clone()));

    // Spawn packing controllers with runtime handle
    // Get the current Tokio runtime handle for this test context
    let runtime_handle =
        tokio::runtime::Handle::try_current().expect("Should be running in tokio runtime");
    let _packing_handles = packing.spawn_packing_controllers(runtime_handle.clone());

    let handle = packing.spawn_tokio_service(runtime_handle.clone());

    // action
    handle.send(request)?;
    wait_for_packing(handle, Some(Duration::from_secs(99999))).await?;
    storage_module.sync_pending_chunks()?;

    // assert
    // check that the chunks are marked as packed
    let intervals = storage_module.get_intervals(ChunkType::Entropy);
    assert_eq!(
        intervals,
        vec![ie(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(packing_end)
        )]
    );

    let intervals2 = storage_module.get_intervals(ChunkType::Uninitialized);
    assert_eq!(
        intervals2,
        vec![ie(
            PartitionChunkOffset::from(packing_end),
            PartitionChunkOffset::from(num_chunks)
        )]
    );

    let stored_entropy = storage_module.read_chunks(ie(
        PartitionChunkOffset::from(0),
        PartitionChunkOffset::from(packing_end),
    ))?;
    // verify the packing
    for i in 0..packing_end {
        let chunk = stored_entropy.get(&PartitionChunkOffset::from(i)).unwrap();

        let mut out = Vec::with_capacity(config.consensus.chunk_size as usize);
        compute_entropy_chunk(
            config.node_config.miner_address(),
            i,
            partition_hash.0,
            config.consensus.entropy_packing_iterations,
            config.consensus.chunk_size.try_into().unwrap(),
            &mut out,
            config.consensus.chain_id,
        );
        assert_eq!(chunk.0.first(), out.first());
    }

    // node.stop().await;
    tx.send(()).await?;
    exit_handle.await??;
    Ok(())
}
