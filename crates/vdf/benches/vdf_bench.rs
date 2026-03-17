use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use irys_types::{H256, H256List, U256, VDFLimiterInfo, VdfConfig};
use irys_vdf::{
    apply_reset_seed, last_step_checkpoints_is_valid, step_number_to_salt_number, vdf_sha,
    vdf_sha_verification,
};
use sha2::{Digest as _, Sha256};

const NUM_CHECKPOINTS: usize = 25;

struct Tier {
    name: &'static str,
    sha_1s_difficulty: u64,
    sample_size: usize,
}

const TIERS: [Tier; 3] = [
    Tier {
        name: "testing",
        sha_1s_difficulty: 70_000,
        sample_size: 100,
    },
    Tier {
        name: "testnet",
        sha_1s_difficulty: 10_000_000,
        sample_size: 10,
    },
    Tier {
        name: "mainnet",
        sha_1s_difficulty: 13_000_000,
        sample_size: 10,
    },
];

fn fixed_seed() -> H256 {
    H256::from([0xAB; 32])
}

fn config_for_tier(tier: &Tier) -> VdfConfig {
    let mut config = irys_types::NodeConfig::testing().vdf();
    config.sha_1s_difficulty = tier.sha_1s_difficulty;
    config
}

fn bench_vdf_sha(c: &mut Criterion) {
    let mut group = c.benchmark_group("vdf_sha");

    for tier in &TIERS {
        let config = config_for_tier(tier);
        let iters = config.num_iterations_per_checkpoint();

        group.sample_size(tier.sample_size);
        group.bench_function(BenchmarkId::from_parameter(tier.name), |b| {
            let mut checkpoints = vec![H256::default(); NUM_CHECKPOINTS];
            b.iter(|| {
                let mut hasher = Sha256::new();
                let mut salt = U256::from(0);
                let mut seed = fixed_seed();
                vdf_sha(
                    &mut hasher,
                    &mut salt,
                    &mut seed,
                    NUM_CHECKPOINTS,
                    iters,
                    &mut checkpoints,
                );
                seed
            });
        });
    }

    group.finish();
}

fn bench_vdf_sha_verification(c: &mut Criterion) {
    let mut group = c.benchmark_group("vdf_sha_verification");

    for tier in &TIERS {
        let config = config_for_tier(tier);
        let iters = config.num_iterations_per_checkpoint();
        let iters_usize: usize = iters.try_into().unwrap();

        group.sample_size(tier.sample_size);
        group.bench_function(BenchmarkId::from_parameter(tier.name), |b| {
            b.iter(|| {
                vdf_sha_verification(U256::from(0), fixed_seed(), NUM_CHECKPOINTS, iters_usize)
            });
        });
    }

    group.finish();
}

fn bench_parallel_verification(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("parallel_verification");

    for tier in &TIERS {
        let config = config_for_tier(tier);
        let prev_output = fixed_seed();
        let global_step: u64 = 2;
        let mut salt = U256::from(step_number_to_salt_number(&config, global_step - 1));
        let mut seed = prev_output;
        let mut checkpoints = vec![H256::default(); NUM_CHECKPOINTS];
        let mut hasher = Sha256::new();

        vdf_sha(
            &mut hasher,
            &mut salt,
            &mut seed,
            NUM_CHECKPOINTS,
            config.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );

        let vdf_info = VDFLimiterInfo {
            output: seed,
            global_step_number: global_step,
            seed: H256::from([0x11; 32]),
            next_seed: H256::from([0x11; 32]),
            prev_output,
            last_step_checkpoints: H256List(checkpoints),
            steps: H256List(vec![seed]),
            vdf_difficulty: None,
            next_vdf_difficulty: None,
        };

        group.sample_size(tier.sample_size);
        group.bench_function(BenchmarkId::from_parameter(tier.name), |b| {
            b.iter(|| {
                rt.block_on(last_step_checkpoints_is_valid(&vdf_info, &config))
                    .unwrap()
            });
        });
    }

    group.finish();
}

fn bench_apply_reset_seed(c: &mut Criterion) {
    let seed = H256::from([0xAB; 32]);
    let reset_seed = H256::from([0xCD; 32]);
    c.bench_function("apply_reset_seed", |b| {
        b.iter(|| apply_reset_seed(seed, reset_seed))
    });
}

criterion_group!(
    benches,
    bench_vdf_sha,
    bench_vdf_sha_verification,
    bench_parallel_verification,
    bench_apply_reset_seed,
);
criterion_main!(benches);
