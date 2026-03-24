//! Benchmark: compute_solution_hash
//!
//! Regression signal: SHA256(chunk + offset + seed) throughput.
//! Called ~512x per mining attempt — the single hottest crypto call.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use irys_types::{compute_solution_hash, H256, MAX_CHUNK_SIZE};

const CHUNK_SIZE: u64 = MAX_CHUNK_SIZE as u64;

fn bench_solution_hash(c: &mut Criterion) {
    let mut group = c.benchmark_group("solution_hash");

    let seed = H256::from([0xAB; 32]);
    let offset: u32 = 42;

    let chunk_256k = vec![0xCD_u8; CHUNK_SIZE as usize];
    group.throughput(Throughput::Bytes(CHUNK_SIZE));
    group.bench_function(BenchmarkId::from_parameter("256KB_chunk"), |b| {
        b.iter(|| black_box(compute_solution_hash(&chunk_256k, offset, &seed)));
    });

    // Benchmark with empty chunk (lower bound — hash overhead only)
    group.throughput(Throughput::Elements(1));
    group.bench_function(BenchmarkId::from_parameter("empty_chunk"), |b| {
        b.iter(|| black_box(compute_solution_hash(&[], offset, &seed)));
    });

    group.finish();
}

criterion_group!(benches, bench_solution_hash);
criterion_main!(benches);
