//! Benchmark: Storage pricing functions
//!
//! Regression signal: calculate_term_fee and exp_neg_fp18 latency.
//! Called per data tx during validation and per block for reward computation.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use irys_types::{
    storage_pricing::{
        calculate_perm_fee_from_config, calculate_term_fee, exp_neg_fp18,
        phantoms::{IrysPrice, Usd},
        Amount,
    },
    ConsensusConfig, U256,
};
use rust_decimal_macros::dec;

fn make_config() -> ConsensusConfig {
    ConsensusConfig::testing()
}

fn bench_calculate_term_fee(c: &mut Criterion) {
    let mut group = c.benchmark_group("pricing/calculate_term_fee");
    let config = make_config();
    let irys_price: Amount<(IrysPrice, Usd)> = Amount::token(dec!(1.0)).expect("valid price");
    let bytes = config.chunk_size;

    for (epochs, label) in [(1_u64, "1_epoch"), (10, "10_epochs"), (100, "100_epochs")] {
        group.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter(|| {
                black_box(
                    calculate_term_fee(bytes, epochs, &config, 10, irys_price).expect("fee calc"),
                )
            });
        });
    }
    group.finish();
}

fn bench_calculate_perm_fee(c: &mut Criterion) {
    let mut group = c.benchmark_group("pricing/calculate_perm_fee");
    let config = make_config();
    let irys_price: Amount<(IrysPrice, Usd)> = Amount::token(dec!(1.0)).expect("valid price");
    let bytes = config.chunk_size;
    let term_fee = calculate_term_fee(
        bytes,
        config.epoch.submit_ledger_epoch_length,
        &config,
        10,
        irys_price,
    )
    .expect("term fee");

    for (proofs, label) in [(1_u64, "1_proof"), (10, "10_proofs"), (100, "100_proofs")] {
        group.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter(|| {
                black_box(
                    calculate_perm_fee_from_config(bytes, &config, proofs, irys_price, term_fee)
                        .expect("perm fee calc"),
                )
            });
        });
    }
    group.finish();
}

fn bench_exp_neg_fp18(c: &mut Criterion) {
    let mut group = c.benchmark_group("pricing/exp_neg_fp18");

    for (x_val, label) in [
        (U256::from(1_000_000_000_000_000_u64), "small"),
        (U256::from(500_000_000_000_000_000_u64), "medium"),
        (U256::from(693_147_180_559_945_309_u64), "ln2"),
    ] {
        group.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter(|| black_box(exp_neg_fp18(x_val).expect("exp_neg")));
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_calculate_term_fee,
    bench_calculate_perm_fee,
    bench_exp_neg_fp18,
);
criterion_main!(benches);
