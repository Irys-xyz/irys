//! Benchmark: Reward curve — emission calculation
//!
//! Regression signal: reward_between latency (covers decay_factor internally).
//! Called per block production for miner reward calculation.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use irys_reward_curve::HalvingCurve;
use irys_types::{U256, storage_pricing::Amount};

const SECS_PER_YEAR: u128 = 365 * 24 * 60 * 60;
const INF_SUPPLY: u128 = 100_000_000;
const HALF_LIFE_YEARS: u128 = 4;

fn test_curve() -> HalvingCurve {
    HalvingCurve {
        inflation_cap: Amount::new(U256::from(INF_SUPPLY)),
        half_life_secs: HALF_LIFE_YEARS * SECS_PER_YEAR,
    }
}

fn bench_reward_between(c: &mut Criterion) {
    let mut group = c.benchmark_group("reward/reward_between");
    let curve = test_curve();

    let cases = [
        (0_u128, 1, "1_second"),
        (0, SECS_PER_YEAR, "1_year"),
        (0, 10 * SECS_PER_YEAR, "10_years"),
        (3 * SECS_PER_YEAR, 5 * SECS_PER_YEAR, "across_half_life"),
        (240 * SECS_PER_YEAR, 241 * SECS_PER_YEAR, "large_t"),
    ];

    for (prev, new, label) in cases {
        group.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter(|| curve.reward_between(prev, new).expect("reward calc"));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_reward_between);
criterion_main!(benches);
