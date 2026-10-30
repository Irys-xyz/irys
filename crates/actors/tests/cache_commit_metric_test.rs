//! `cache_commit_duration_ms` is read through a global meter provider, which
//! records every ingest in the process. This test has a binary of its own,
//! so no other test can ingest into that provider or bind the instrument to
//! an earlier one, under nextest or `cargo test`.

use irys_actors::ChunkIngressService;
use irys_actors::chunk_ingress_service::IngressProofGenerationState;
use irys_actors::chunk_ingress_service::facade::ChunkIngressFacadeImpl;
use irys_actors::test_helpers::build_test_service_senders;
use irys_database::db::IrysDatabaseExt as _;
use irys_database::tables::IrysTables;
use irys_database::{IrysDatabaseArgs as _, open_or_create_db};
use irys_domain::{BlockTree, BlockTreeReadGuard, StorageModulesReadGuard};
use irys_testing_utils::new_mock_signed_header;
use irys_testing_utils::utils::TempDirBuilder;
use irys_types::{
    Config, DataTransactionHeader, DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata,
    DataTransactionMetadata, DatabaseProvider, H256, NodeConfig, TxChunkOffset, UnpackedChunk,
};
use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, HistogramDataPoint, MetricData, ResourceMetrics, ScopeMetrics,
};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use reth::tasks::TaskExecutor;
use reth_db::mdbx::DatabaseArguments;
use std::sync::{Arc, RwLock};

fn single_chunk(bytes: Vec<u8>) -> UnpackedChunk {
    let chunk_size = bytes.len();
    // clone: generate_leaves consumes the bytes the chunk keeps
    let leaves = irys_types::generate_leaves(std::iter::once(Ok(bytes.clone())), chunk_size)
        .expect("leaves");
    let root = irys_types::generate_data_root(leaves).expect("root");
    let data_root = H256(root.id);
    let proof = irys_types::resolve_proofs(root, None)
        .expect("proofs")
        .into_iter()
        .next()
        .expect("one proof");
    UnpackedChunk {
        data_root,
        data_size: u64::try_from(chunk_size).expect("chunk length fits u64"),
        data_path: proof.proof.into(),
        bytes: bytes.into(),
        tx_offset: TxChunkOffset::from(0),
    }
}

fn confirm_data_root(db: &DatabaseProvider, chunk: &UnpackedChunk) {
    let header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
        tx: DataTransactionHeaderV1 {
            data_root: chunk.data_root,
            data_size: chunk.data_size,
            ..Default::default()
        },
        metadata: DataTransactionMetadata::new(),
    });
    db.update_eyre(|tx| {
        irys_database::cache_data_root(tx, &header, None)?;
        irys_database::confirm_data_size_for_data_root(tx, &chunk.data_root, chunk.data_size)?;
        Ok(())
    })
    .expect("cache and confirm data root");
}

/// An ingest records its chunk-cache commit wait once, so a slow root
/// volume under the commit shows in `cache_commit_duration_ms`.
#[tokio::test]
async fn ingest_records_the_cache_commit_duration() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    opentelemetry::global::set_meter_provider(provider.clone());

    let tmp = TempDirBuilder::new().prefix("ingress-commit-timer").build();
    let mut node_config = NodeConfig::testing();
    node_config.consensus.get_mut().chunk_size = 32;
    node_config.base_directory = tmp.path().to_path_buf();
    let config = Config::new_with_random_peer_id(node_config);
    let db = DatabaseProvider(Arc::new(
        open_or_create_db(
            tmp.path().join("db"),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().expect("testing database arguments"),
        )
        .expect("open test database"),
    ));
    let (service_senders, receivers) = build_test_service_senders();
    let block_tree = BlockTreeReadGuard::new(Arc::new(RwLock::new(BlockTree::new(
        &new_mock_signed_header(),
        config.consensus.clone(),
    ))));
    let (_handle, _state) = ChunkIngressService::spawn_service(
        db.clone(),
        StorageModulesReadGuard::new(Arc::new(RwLock::new(Vec::new()))),
        &block_tree,
        receivers.chunk_ingress,
        &config,
        &service_senders,
        IngressProofGenerationState::new(),
        tokio::runtime::Handle::current(),
        TaskExecutor::test(),
    );

    let chunk = single_chunk(vec![5_u8; 32]);
    confirm_data_root(&db, &chunk);
    ChunkIngressFacadeImpl::new(service_senders.chunk_ingress.clone())
        .handle_chunk_ingress(chunk)
        .await
        .expect("chunk ingress");
    provider.force_flush().expect("flush metrics");

    let commits = exporter
        .get_finished_metrics()
        .expect("collected metrics")
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
        .find(|metric| metric.name() == "irys.mempool.chunks.cache_commit_duration_ms")
        .map(|metric| match metric.data() {
            AggregatedMetrics::F64(MetricData::Histogram(histogram)) => histogram
                .data_points()
                .map(HistogramDataPoint::count)
                .sum::<u64>(),
            _ => panic!("cache_commit_duration_ms must be an f64 histogram"),
        });
    assert_eq!(commits, Some(1), "one ingest records one commit wait");
}
