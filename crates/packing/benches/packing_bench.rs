//! Benchmark: Packing operations — XOR, entropy generation, Rust vs C
//!
//! Regression signal: XOR throughput (SIMD vectorization), entropy generation
//! cost per chunk, and Rust vs C implementation comparison.
//! Packing runs per-chunk during storage initialization and mining.

use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use irys_packing::{
    capacity_pack_range_c, capacity_pack_range_with_data, capacity_pack_range_with_data_c,
    capacity_single, packing_xor_vec_u8, xor_vec_u8_arrays_in_place,
};
use irys_types::{ConsensusConfig, H256, IrysAddress};

const CHUNK_SIZE_U64: u64 = 256 * 1024;
const CHUNK_SIZE: usize = CHUNK_SIZE_U64 as usize;

struct PackingTier {
    name: &'static str,
    iterations: u32,
    chain_id: u64,
    sample_size: usize,
    measurement_time: Duration,
}

fn build_packing_tiers() -> [PackingTier; 2] {
    let testnet = ConsensusConfig::testnet();
    let mainnet = ConsensusConfig::mainnet();

    [
        PackingTier {
            name: "testnet",
            iterations: testnet.entropy_packing_iterations,
            chain_id: testnet.chain_id,
            sample_size: 10,
            measurement_time: Duration::from_secs(30),
        },
        PackingTier {
            name: "mainnet",
            iterations: mainnet.entropy_packing_iterations,
            chain_id: mainnet.chain_id,
            sample_size: 10,
            measurement_time: Duration::from_secs(60),
        },
    ]
}

fn bench_xor_in_place(c: &mut Criterion) {
    let mut group = c.benchmark_group("packing/xor_in_place");
    let mut a = vec![0xAA_u8; CHUNK_SIZE];
    let b = vec![0x55_u8; CHUNK_SIZE];

    group.throughput(Throughput::Bytes(CHUNK_SIZE_U64));
    group.bench_function(BenchmarkId::from_parameter("256KB"), |bench| {
        bench.iter(|| {
            xor_vec_u8_arrays_in_place(&mut a, &b);
        });
    });
    group.finish();
}

fn bench_packing_xor_owned(c: &mut Criterion) {
    let mut group = c.benchmark_group("packing/xor_owned");
    let data = vec![0x55_u8; CHUNK_SIZE];

    group.throughput(Throughput::Bytes(CHUNK_SIZE_U64));
    group.bench_function(BenchmarkId::from_parameter("256KB"), |bench| {
        bench.iter(|| {
            let entropy = vec![0xAA_u8; CHUNK_SIZE];
            black_box(packing_xor_vec_u8(entropy, &data))
        });
    });
    group.finish();
}

fn bench_entropy_rust_vs_c(c: &mut Criterion) {
    let tiers = build_packing_tiers();
    let mining_address = IrysAddress::from([0xAB; 20]);
    let partition_hash_bytes = H256::from([0xCD; 32]);
    let partition_hash = partition_hash_bytes;
    let chunk_offset: u64 = 0;

    // Rust implementation
    {
        let mut group = c.benchmark_group("packing/entropy_rust");
        for tier in &tiers {
            let mut entropy = Vec::with_capacity(CHUNK_SIZE);
            group.sample_size(tier.sample_size);
            group.measurement_time(tier.measurement_time);
            group.throughput(Throughput::Bytes(CHUNK_SIZE_U64));
            group.bench_function(BenchmarkId::from_parameter(tier.name), |b| {
                b.iter(|| {
                    capacity_single::compute_entropy_chunk(
                        mining_address,
                        chunk_offset,
                        partition_hash_bytes.0,
                        tier.iterations,
                        CHUNK_SIZE,
                        &mut entropy,
                        tier.chain_id,
                    );
                });
            });
        }
        group.finish();
    }

    // C implementation
    {
        let mut group = c.benchmark_group("packing/entropy_c");
        for tier in &tiers {
            let mut entropy = Vec::with_capacity(CHUNK_SIZE);
            group.sample_size(tier.sample_size);
            group.measurement_time(tier.measurement_time);
            group.throughput(Throughput::Bytes(CHUNK_SIZE_U64));
            group.bench_function(BenchmarkId::from_parameter(tier.name), |b| {
                b.iter(|| {
                    capacity_pack_range_c(
                        mining_address,
                        chunk_offset,
                        partition_hash,
                        &mut entropy,
                        tier.iterations,
                        tier.chain_id,
                    );
                });
            });
        }
        group.finish();
    }
}

fn bench_batch_pack(c: &mut Criterion) {
    let tiers = build_packing_tiers();
    let mining_address = IrysAddress::from([0xAB; 20]);
    let partition_hash_bytes = H256::from([0xCD; 32]);
    let partition_hash = partition_hash_bytes;

    for batch_size in [1_usize, 32] {
        // Rust batch
        {
            let mut group = c.benchmark_group(format!("packing/batch_rust/{batch_size}_chunks"));
            for tier in &tiers {
                group.sample_size(tier.sample_size);
                group.measurement_time(tier.measurement_time);
                group.throughput(Throughput::Bytes(
                    u64::try_from(batch_size).unwrap() * CHUNK_SIZE_U64,
                ));
                group.bench_function(BenchmarkId::from_parameter(tier.name), |b| {
                    b.iter_batched(
                        || {
                            (0..batch_size)
                                .map(|_| vec![0_u8; CHUNK_SIZE])
                                .collect::<Vec<Vec<u8>>>()
                        },
                        |mut data| {
                            capacity_pack_range_with_data(
                                &mut data,
                                mining_address,
                                0,
                                partition_hash,
                                CHUNK_SIZE,
                                tier.iterations,
                                tier.chain_id,
                            );
                        },
                        criterion::BatchSize::LargeInput,
                    );
                });
            }
            group.finish();
        }

        // C batch
        {
            let mut group = c.benchmark_group(format!("packing/batch_c/{batch_size}_chunks"));
            for tier in &tiers {
                group.sample_size(tier.sample_size);
                group.measurement_time(tier.measurement_time);
                group.throughput(Throughput::Bytes(
                    u64::try_from(batch_size).unwrap() * CHUNK_SIZE_U64,
                ));
                group.bench_function(BenchmarkId::from_parameter(tier.name), |b| {
                    b.iter_batched(
                        || {
                            (0..batch_size)
                                .map(|_| vec![0_u8; CHUNK_SIZE])
                                .collect::<Vec<Vec<u8>>>()
                        },
                        |mut data| {
                            capacity_pack_range_with_data_c(
                                &mut data,
                                mining_address,
                                0,
                                partition_hash,
                                tier.iterations,
                                tier.chain_id,
                                CHUNK_SIZE,
                            );
                        },
                        criterion::BatchSize::LargeInput,
                    );
                });
            }
            group.finish();
        }
    }
}

criterion_group!(
    benches,
    bench_xor_in_place,
    bench_packing_xor_owned,
    bench_entropy_rust_vs_c,
    bench_batch_pack,
);
criterion_main!(benches);
