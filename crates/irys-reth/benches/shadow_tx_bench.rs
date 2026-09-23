//! Benchmark: Shadow transaction encoding and decoding
//!
//! Regression signal: Borsh serialization throughput for protocol actions.
//! Encode runs 1-20x per block; decode runs per tx during EVM execution.
//! detect_and_decode fast-reject must be near-zero for non-shadow txs.

use alloy_primitives::{Address, FixedBytes, U256};
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use irys_reth::shadow_tx::{
    BlockRewardIncrement, SHADOW_TX_DESTINATION_ADDR, ShadowTransaction, TransactionPacket,
    detect_and_decode_from_parts, encode_prefixed_input, try_decode_prefixed,
};

fn make_block_reward_tx() -> ShadowTransaction {
    ShadowTransaction::V1 {
        packet: TransactionPacket::BlockReward(BlockRewardIncrement {
            amount: U256::from(1_000_000_u64),
        }),
        solution_hash: FixedBytes::from([0xAB; 32]),
    }
}

fn bench_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("shadow_tx/encode");
    let tx = make_block_reward_tx();

    group.bench_function(BenchmarkId::from_parameter("BlockReward"), |b| {
        b.iter(|| black_box(encode_prefixed_input(&tx)));
    });

    group.finish();
}

fn bench_decode_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("shadow_tx/decode");
    let tx = make_block_reward_tx();
    let encoded = encode_prefixed_input(&tx);

    group.bench_function(BenchmarkId::from_parameter("BlockReward"), |b| {
        b.iter(|| black_box(try_decode_prefixed(&encoded).expect("decode")));
    });

    group.finish();
}

fn bench_detect_and_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("shadow_tx/detect_and_decode");
    let tx = make_block_reward_tx();
    let encoded = encode_prefixed_input(&tx);
    let shadow_addr = Some(*SHADOW_TX_DESTINATION_ADDR);

    group.bench_function(BenchmarkId::from_parameter("shadow_tx"), |b| {
        b.iter(|| {
            black_box(
                detect_and_decode_from_parts(shadow_addr, &encoded)
                    .expect("decode")
                    .expect("should be Some for shadow tx"),
            )
        });
    });

    let non_shadow_addr = Some(Address::from([0x01; 20]));
    group.bench_function(BenchmarkId::from_parameter("non_shadow_reject"), |b| {
        b.iter(|| black_box(detect_and_decode_from_parts(non_shadow_addr, &encoded)));
    });

    let non_shadow_data = vec![0_u8; 100];
    group.bench_function(BenchmarkId::from_parameter("no_prefix_reject"), |b| {
        b.iter(|| black_box(detect_and_decode_from_parts(shadow_addr, &non_shadow_data)));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_encode,
    bench_decode_roundtrip,
    bench_detect_and_decode
);
criterion_main!(benches);
