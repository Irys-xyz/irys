use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use irys_efficient_sampling::get_recall_range;
use irys_types::{H256, H256List};

fn bench_get_recall_range(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_recall_range");
    // mainnet today is 64_840 ranges per partition; cover a representative spread.
    for &n in &[100_usize, 1_000, 10_000, 64_840] {
        let partition_hash = H256::random();
        let seeds = H256List((0..n).map(|_| H256::random()).collect());

        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter(|| get_recall_range(n, &seeds, &partition_hash).unwrap());
        });
    }
    group.finish();
}

criterion_group!(benches, bench_get_recall_range);
criterion_main!(benches);
