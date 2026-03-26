//! Benchmark: P2P rate limiting and gossip serialisation
//!
//! Regression signal: check_request per-request cost (DashMap + sliding window),
//! pre_serialize_for_broadcast per-payload JSON serialisation cost.
//! check_request runs per P2P data request; pre_serialize amortises across peers.

use std::sync::Arc;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use irys_p2p::{DataRequestTracker, GossipClient};
use irys_types::v2::GossipDataV2;
use irys_types::{
    Base64, BlockBody, H256, IrysAddress, IrysBlockHeader, IrysPeerId, TxChunkOffset, UnpackedChunk,
};

fn bench_check_request(c: &mut Criterion) {
    let mut group = c.benchmark_group("p2p/check_request");
    let dedup_window_ms: u128 = 10;
    let peer = IrysPeerId::from(IrysAddress::from([0x01; 20]));

    group.bench_function(BenchmarkId::from_parameter("first_request"), |b| {
        b.iter_batched(
            || {
                let tracker = DataRequestTracker::new();
                let mut addr_bytes = [0_u8; 20];
                addr_bytes.copy_from_slice(&H256::random().0[..20]);
                let fresh_peer = IrysPeerId::from(IrysAddress::from(addr_bytes));
                (tracker, fresh_peer)
            },
            |(tracker, fresh_peer)| black_box(tracker.check_request(&fresh_peer, dedup_window_ms)),
            criterion::BatchSize::SmallInput,
        );
    });

    group.bench_function(BenchmarkId::from_parameter("repeat_request"), |b| {
        b.iter_batched(
            || {
                let tracker = DataRequestTracker::new();
                tracker.check_request(&peer, dedup_window_ms);
                tracker
            },
            |tracker| black_box(tracker.check_request(&peer, dedup_window_ms)),
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_pre_serialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("p2p/pre_serialize");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let handle = rt.handle().clone(); // clone: Handle is cheap (Arc internally)

    let mining_address = IrysAddress::from([0xAB; 20]);
    let peer_id = IrysPeerId::from(mining_address);
    let client = GossipClient::new(Duration::from_secs(5), mining_address, peer_id, handle);

    let chunk = GossipDataV2::Chunk(Arc::new(UnpackedChunk {
        data_root: H256::from([0xCD; 32]),
        data_size: 256 * 1024,
        data_path: Base64(vec![0_u8; 64]),
        bytes: Base64(vec![0xAB_u8; 256 * 1024]),
        tx_offset: TxChunkOffset(0),
    }));

    let block_header = GossipDataV2::BlockHeader(Arc::new(IrysBlockHeader::new_mock_header()));

    let block_body = GossipDataV2::BlockBody(Arc::new(BlockBody::default()));

    group.bench_function(BenchmarkId::from_parameter("chunk_256KB"), |b| {
        b.iter(|| {
            black_box(client.pre_serialize_for_broadcast(&chunk));
        });
    });

    group.bench_function(BenchmarkId::from_parameter("block_header"), |b| {
        b.iter(|| {
            black_box(client.pre_serialize_for_broadcast(&block_header));
        });
    });

    group.bench_function(BenchmarkId::from_parameter("block_body"), |b| {
        b.iter(|| {
            black_box(client.pre_serialize_for_broadcast(&block_body));
        });
    });

    group.finish();
}

criterion_group!(benches, bench_check_request, bench_pre_serialize);
criterion_main!(benches);
