//! `cache_commit_duration_ms` is read through a global meter provider, which
//! records every ingest in the process. This test has a binary of its own,
//! so no other test can ingest into that provider or bind the instrument to
//! an earlier one, under nextest or `cargo test`.

use irys_actors::chunk_ingress_service::IngressProofGenerationState;
use irys_actors::chunk_ingress_service::facade::ChunkIngressFacadeImpl;
use irys_actors::test_helpers::build_test_service_senders;
use irys_actors::{ChunkIngressService, ChunkIngressSource};
use irys_database::DatabaseProvider;
use irys_database::db::{DatabaseProviderCacheExt as _, DatabaseProviderTestExt as _};
use irys_domain::{BlockTree, BlockTreeReadGuard, StorageModulesReadGuard};
use irys_testing_utils::new_mock_signed_header;
use irys_testing_utils::utils::TempDirBuilder;
use irys_types::irys::IrysSigner;
use irys_types::{Config, DataTransactionHeader, H256, NodeConfig, UnpackedChunk};
use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, HistogramDataPoint, MetricData, ResourceMetrics, ScopeMetrics,
};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use reth::tasks::TaskExecutor;
use std::sync::{Arc, RwLock};

/// A one-chunk transaction header and its chunk.
fn single_chunk_transaction(
    config: &Config,
    bytes: Vec<u8>,
) -> (DataTransactionHeader, UnpackedChunk) {
    let signer = IrysSigner::random_signer(&config.consensus);
    let transaction = signer
        .create_transaction(bytes, H256::zero())
        .expect("create transaction");
    let chunk = transaction
        .data_chunks()
        .expect("chunks")
        .into_iter()
        .next()
        .expect("one chunk");
    (transaction.header, chunk)
}

fn cache_data_root(db: &DatabaseProvider, config: &Config, header: &DataTransactionHeader) {
    db.update_cache_eyre(|tx| {
        irys_database::cache_data_root(tx, header, None, &config.data_tree_geometry())?;
        Ok(())
    })
    .expect("cache data root");
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
    let db = DatabaseProvider::for_testing(tmp.path(), &tmp.path().join("cache"))
        .expect("open test databases");
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

    let (header, chunk) = single_chunk_transaction(&config, vec![5_u8; 32]);
    cache_data_root(&db, &config, &header);
    ChunkIngressFacadeImpl::new(service_senders.chunk_ingress.clone())
        .handle_chunk_ingress(chunk, ChunkIngressSource::Sent)
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
