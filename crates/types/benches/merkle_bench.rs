//! Benchmark: Merkle proof generation and validation
//!
//! Regression signal: validate_path and generate_data_root throughput.
//! These scale with chunk count and are on the block validation critical path.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use irys_types::{
    generate_data_root, generate_leaves_from_chunks, resolve_proofs, validate_path, Base64,
    ChunkBytes, MAX_CHUNK_SIZE,
};

fn make_chunks(count: usize) -> Vec<ChunkBytes> {
    (0..count)
        .map(|i| {
            let mut chunk = vec![0_u8; MAX_CHUNK_SIZE];
            chunk[0] = u8::try_from(i % 256).unwrap();
            chunk[1] = u8::try_from((i >> 8) % 256).unwrap();
            chunk
        })
        .collect()
}

/// Builds a merkle tree and returns (root_id, proofs) for use in validate_path benchmarks.
fn build_tree_and_proofs(chunks: &[ChunkBytes]) -> ([u8; 32], Vec<(Base64, u128)>) {
    let leaves =
        generate_leaves_from_chunks(chunks.iter().map(|c| Ok(c.clone()))).expect("leaf generation");
    let root = generate_data_root(leaves).expect("data root");
    let root_id = root.id;

    let proofs = resolve_proofs(root, None).expect("resolve proofs");
    let proof_pairs: Vec<(Base64, u128)> = proofs
        .into_iter()
        .map(|p| {
            let target = u128::try_from(p.last_byte_index).unwrap();
            (Base64(p.proof), target)
        })
        .collect();

    (root_id, proof_pairs)
}

fn bench_generate_data_root(c: &mut Criterion) {
    let mut group = c.benchmark_group("merkle/chunks_to_data_root");

    for count in [1, 32, 256] {
        let chunks = make_chunks(count);
        group.throughput(Throughput::Elements(u64::try_from(count).unwrap()));
        group.bench_function(
            BenchmarkId::from_parameter(format!("{count}_chunks")),
            |b| {
                b.iter(|| {
                    let leaves = generate_leaves_from_chunks(chunks.iter().map(|c| Ok(c.clone())))
                        .expect("leaf generation");
                    black_box(generate_data_root(leaves).expect("data root"))
                });
            },
        );
    }

    group.finish();
}

fn bench_validate_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("merkle/validate_path");

    for count in [1, 32, 256] {
        let chunks = make_chunks(count);
        let (root_id, proofs) = build_tree_and_proofs(&chunks);

        let (ref proof_bytes, target) = proofs[0];

        group.throughput(Throughput::Elements(1));
        group.bench_function(
            BenchmarkId::from_parameter(format!("{count}_chunks")),
            |b| {
                b.iter(|| {
                    black_box(validate_path(root_id, proof_bytes, target).expect("valid path"))
                });
            },
        );
    }

    group.finish();
}

fn bench_generate_leaves(c: &mut Criterion) {
    let mut group = c.benchmark_group("merkle/generate_leaves");

    for count in [1, 32, 256] {
        let chunks = make_chunks(count);
        group.throughput(Throughput::Elements(u64::try_from(count).unwrap()));
        group.bench_function(
            BenchmarkId::from_parameter(format!("{count}_chunks")),
            |b| {
                b.iter(|| {
                    black_box(
                        generate_leaves_from_chunks(chunks.iter().map(|c| Ok(c.clone())))
                            .expect("leaf generation"),
                    )
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_generate_data_root,
    bench_validate_path,
    bench_generate_leaves,
);
criterion_main!(benches);
