use packing_variant_bench::{
    Chunk, ChunkId, Variant, cpu_fast, entropy_a, entropy_b, new_chunk, to_hex,
};
use rand::{Rng as _, SeedableRng as _, rngs::StdRng};

const MAX_N: usize = 8;

fn random_id(rng: &mut StdRng) -> ChunkId {
    ChunkId {
        address: rng.r#gen(),
        partition_hash: rng.r#gen(),
        chain_id: rng.r#gen(),
        partition_offset: rng.r#gen(),
    }
}

fn reference(variant: Variant, id: &ChunkId, iterations: u32) -> Box<Chunk> {
    let mut buf = new_chunk();
    match variant {
        Variant::A => entropy_a(id, iterations, &mut buf),
        Variant::B => entropy_b(id, iterations, &mut buf),
    }
    buf
}

fn check_n<const N: usize>(
    variant: Variant,
    ids: &[ChunkId; MAX_N],
    iterations: u32,
    expected: &[Box<Chunk>],
) {
    let batch: [ChunkId; N] = ids[..N].try_into().unwrap();
    // pre-fill with garbage: the output must not depend on the buffers' previous contents
    let mut bufs: [Box<Chunk>; N] = std::array::from_fn(|_| {
        let mut b = new_chunk();
        b.fill(0xA5);
        b
    });
    cpu_fast::entropy_multi::<N>(variant, &batch, iterations, &mut bufs);
    for n in 0..N {
        assert!(
            bufs[n][..] == expected[n][..],
            "variant={variant:?} N={N} n={n} iterations={iterations} id={:?}\n got  {}\n want {}",
            batch[n],
            to_hex(&bufs[n][..64]),
            to_hex(&expected[n][..64]),
        );
    }
}

fn check(iterations: u32, seed: u64) {
    if !cpu_fast::available() {
        eprintln!("note: SHA-NI not available on this CPU; checking the OpenSSL fallback instead");
    }
    let mut rng = StdRng::seed_from_u64(seed);
    let ids: [ChunkId; MAX_N] = std::array::from_fn(|_| random_id(&mut rng));
    for variant in Variant::ALL {
        let expected: Vec<Box<Chunk>> = std::thread::scope(|s| {
            let handles: Vec<_> = ids
                .iter()
                .map(|id| s.spawn(move || reference(variant, id, iterations)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let mut single = new_chunk();
        cpu_fast::entropy(variant, &ids[0], iterations, &mut single);
        assert!(
            single[..] == expected[0][..],
            "single variant={variant:?} iterations={iterations}"
        );
        check_n::<1>(variant, &ids, iterations, &expected);
        check_n::<2>(variant, &ids, iterations, &expected);
        check_n::<4>(variant, &ids, iterations, &expected);
        check_n::<8>(variant, &ids, iterations, &expected);
    }
}

#[test]
fn matches_reference_small_iteration_counts() {
    // Phase-1-only, a partial sweep, wrap-around sweeps, and non-multiples of 8192
    for (k, iterations) in [8_192_u32, 8_193, 12_345, 16_384, 40_000, 100_003]
        .into_iter()
        .enumerate()
    {
        check(iterations, 0xC0FFEE + k as u64);
    }
}

#[test]
fn matches_reference_1m_iterations() {
    check(1_000_000, 0xBEEF);
}

#[test]
fn below_phase1_iterations_still_writes_phase1() {
    // the reference always runs the full Phase 1, whatever the iteration count
    check(0, 0x1234);
    check(1, 0x5678);
}
