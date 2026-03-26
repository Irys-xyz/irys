//! Benchmark: Efficient sampling — recall range computation
//!
//! Regression signal: next_recall_range per-step cost, reconstruct cost
//! by step gap, and recall_range_is_valid validation cost.
//! These are on the mining critical path and have zero existing benchmarks.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use irys_efficient_sampling::{Ranges, recall_range_is_valid};
use irys_types::{H256, H256List};

fn fixed_partition_hash() -> H256 {
    H256::from([0xCD; 32])
}

fn make_seeds(count: usize) -> Vec<H256> {
    (0..count)
        .map(|i| {
            let mut bytes = [0_u8; 32];
            bytes[0] = u8::try_from(i % 256).unwrap();
            bytes[1] = u8::try_from((i >> 8) % 256).unwrap();
            bytes[2] = u8::try_from((i >> 16) % 256).unwrap();
            H256::from(bytes)
        })
        .collect()
}

fn bench_next_recall_range(c: &mut Criterion) {
    let mut group = c.benchmark_group("sampling/next_recall_range");
    let partition_hash = fixed_partition_hash();
    let seeds = make_seeds(1);

    for num_ranges in [10_usize, 100, 64_840] {
        group.bench_function(
            BenchmarkId::from_parameter(format!("{num_ranges}_ranges")),
            |b| {
                b.iter_batched(
                    || Ranges::new(num_ranges),
                    |mut ranges| ranges.next_recall_range(1, &seeds[0], &partition_hash),
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

fn bench_reconstruct(c: &mut Criterion) {
    let mut group = c.benchmark_group("sampling/reconstruct");
    let partition_hash = fixed_partition_hash();

    for gap in [1_usize, 10, 100, 1000] {
        let seeds = make_seeds(gap);
        let step_seeds = H256List(seeds);

        group.bench_function(BenchmarkId::from_parameter(format!("gap_{gap}")), |b| {
            b.iter_batched(
                || Ranges::new(64_840),
                |mut ranges| ranges.reconstruct(&step_seeds, &partition_hash),
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_recall_range_is_valid(c: &mut Criterion) {
    let mut group = c.benchmark_group("sampling/recall_range_is_valid");
    let partition_hash = fixed_partition_hash();

    for num_steps in [1_usize, 10, 100] {
        let seeds = make_seeds(num_steps);
        let step_seeds = H256List(seeds);

        let mut ranges = Ranges::new(64_840);
        ranges.reconstruct(&step_seeds, &partition_hash);
        let expected_range = ranges.get_last_recall_range().unwrap();

        group.bench_function(
            BenchmarkId::from_parameter(format!("{num_steps}_steps")),
            |b| {
                b.iter(|| {
                    recall_range_is_valid(expected_range, 64_840, &step_seeds, &partition_hash)
                        .expect("valid range")
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_next_recall_range,
    bench_reconstruct,
    bench_recall_range_is_valid,
);
criterion_main!(benches);
