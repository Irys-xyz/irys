use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use irys_types::{ConsensusConfig, H256, H256List, NodeConfig, U256, VDFLimiterInfo, VdfConfig};
use irys_vdf::{
    apply_reset_seed, last_step_checkpoints_is_valid, step_number_to_salt_number, vdf_sha,
    vdf_sha_verification,
};

const DEFAULT_PARALLEL_VERIFICATION_THREAD_LIMIT: usize = 4;

struct Tier {
    name: &'static str,
    config: VdfConfig,
    sample_size: usize,
    measurement_time: Duration,
}

fn build_tiers() -> [Tier; 3] {
    let testing_config = NodeConfig::testing().vdf();

    let testnet_consensus = ConsensusConfig::testnet().vdf;
    let testnet_config = VdfConfig {
        reset_frequency: testnet_consensus.reset_frequency,
        parallel_verification_thread_limit: DEFAULT_PARALLEL_VERIFICATION_THREAD_LIMIT,
        num_checkpoints_in_vdf_step: testnet_consensus.num_checkpoints_in_vdf_step,
        max_allowed_vdf_fork_steps: testnet_consensus.max_allowed_vdf_fork_steps,
        sha_1s_difficulty: testnet_consensus.sha_1s_difficulty,
    };

    let mainnet_consensus = ConsensusConfig::mainnet().vdf;
    let mainnet_config = VdfConfig {
        reset_frequency: mainnet_consensus.reset_frequency,
        parallel_verification_thread_limit: DEFAULT_PARALLEL_VERIFICATION_THREAD_LIMIT,
        num_checkpoints_in_vdf_step: mainnet_consensus.num_checkpoints_in_vdf_step,
        max_allowed_vdf_fork_steps: mainnet_consensus.max_allowed_vdf_fork_steps,
        sha_1s_difficulty: mainnet_consensus.sha_1s_difficulty,
    };

    [
        Tier {
            name: "testing",
            config: testing_config,
            sample_size: 100,
            measurement_time: Duration::from_secs(5),
        },
        Tier {
            name: "testnet",
            config: testnet_config,
            sample_size: 10,
            measurement_time: Duration::from_secs(75),
        },
        Tier {
            name: "mainnet",
            config: mainnet_config,
            sample_size: 10,
            measurement_time: Duration::from_secs(100),
        },
    ]
}

fn fixed_seed() -> H256 {
    H256::from([0xAB; 32])
}

fn bench_vdf_sha(c: &mut Criterion) {
    let tiers = build_tiers();
    let mut group = c.benchmark_group("vdf_sha");

    for tier in &tiers {
        let num_checkpoints = tier.config.num_checkpoints_in_vdf_step;
        let iters = tier.config.num_iterations_per_checkpoint();

        group.sample_size(tier.sample_size);
        group.measurement_time(tier.measurement_time);
        group.bench_function(BenchmarkId::from_parameter(tier.name), |b| {
            let mut checkpoints = vec![H256::default(); num_checkpoints];
            b.iter(|| {
                let salt = U256::from(0);
                let mut seed = fixed_seed();
                vdf_sha(salt, &mut seed, num_checkpoints, iters, &mut checkpoints);
                std::hint::black_box(&checkpoints);
                seed
            });
        });
    }

    group.finish();
}

fn bench_vdf_sha_verification(c: &mut Criterion) {
    let tiers = build_tiers();
    let mut group = c.benchmark_group("vdf_sha_verification");

    for tier in &tiers {
        let num_checkpoints = tier.config.num_checkpoints_in_vdf_step;
        let iters = tier.config.num_iterations_per_checkpoint();
        group.sample_size(tier.sample_size);
        group.measurement_time(tier.measurement_time);
        group.bench_function(BenchmarkId::from_parameter(tier.name), |b| {
            b.iter(|| vdf_sha_verification(U256::from(0), fixed_seed(), num_checkpoints, iters));
        });
    }

    group.finish();
}

fn bench_parallel_verification(c: &mut Criterion) {
    let tiers = build_tiers();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("parallel_verification");

    for tier in &tiers {
        let config = &tier.config;
        let num_checkpoints = config.num_checkpoints_in_vdf_step;
        let prev_output = fixed_seed();
        let global_step: u64 = 2;
        let salt = U256::from(step_number_to_salt_number(config, global_step - 1));
        let mut seed = prev_output;
        let mut checkpoints = vec![H256::default(); num_checkpoints];

        vdf_sha(
            salt,
            &mut seed,
            num_checkpoints,
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
        group.measurement_time(tier.measurement_time);
        group.bench_function(BenchmarkId::from_parameter(tier.name), |b| {
            b.iter(|| {
                rt.block_on(last_step_checkpoints_is_valid(&vdf_info, config))
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
