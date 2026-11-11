use packing_variant_bench::{
    Chunk, ChunkId, Variant, cpu_avx512, entropy_a, entropy_b, new_chunk, to_hex,
};
use rand::{Rng as _, SeedableRng as _, rngs::StdRng};

const MAX_CHUNKS: usize = 2 * cpu_avx512::LANES;

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

fn check_groups<const G: usize>(
    variant: Variant,
    ids: &[ChunkId],
    iterations: u32,
    expected: &[Box<Chunk>],
    scratch: &mut cpu_avx512::Scratch,
) {
    let n = G * cpu_avx512::LANES;
    // pre-fill with garbage: the output must not depend on the buffers' previous contents
    let mut bufs: Vec<Box<Chunk>> = (0..n)
        .map(|_| {
            let mut b = new_chunk();
            b.fill(0xA5);
            b
        })
        .collect();
    cpu_avx512::entropy_lanes::<G>(variant, &ids[..n], iterations, &mut bufs, scratch);
    for k in 0..n {
        assert!(
            bufs[k][..] == expected[k][..],
            "variant={variant:?} G={G} chunk={k} iterations={iterations} id={:?}\n got  {}\n want {}",
            ids[k],
            to_hex(&bufs[k][..64]),
            to_hex(&expected[k][..64]),
        );
    }
}

fn check(iterations: u32, seed: u64) {
    if !cpu_avx512::available() {
        eprintln!("skipping: AVX-512F not available on this CPU");
        return;
    }
    let mut rng = StdRng::seed_from_u64(seed);
    let ids: Vec<ChunkId> = (0..MAX_CHUNKS).map(|_| random_id(&mut rng)).collect();
    // Reused across calls and variants on purpose, so stale scratch contents are exercised too.
    let mut scratch = cpu_avx512::Scratch::new(1);
    for variant in Variant::ALL {
        let expected: Vec<Box<Chunk>> = std::thread::scope(|s| {
            let handles: Vec<_> = ids
                .iter()
                .map(|id| s.spawn(move || reference(variant, id, iterations)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        check_groups::<1>(variant, &ids, iterations, &expected, &mut scratch);
        check_groups::<2>(variant, &ids, iterations, &expected, &mut scratch);
    }
}

#[test]
fn matches_reference_small_iteration_counts() {
    // Phase-1-only, a partial sweep, wrap-around sweeps, and non-multiples of 8192
    for (k, iterations) in [8_192_u32, 8_193, 12_345, 16_384, 40_000, 100_003]
        .into_iter()
        .enumerate()
    {
        check(iterations, 0xA512 + k as u64);
    }
}

#[test]
fn matches_reference_1m_iterations() {
    check(1_000_000, 0xFACE);
}

#[test]
fn below_phase1_iterations_still_writes_phase1() {
    // the reference always runs the full Phase 1, whatever the iteration count
    check(0, 0x9abc);
    check(1, 0xdef0);
}

#[test]
#[should_panic(expected = "16 * G")]
fn rejects_wrong_chunk_count() {
    let ids = vec![random_id(&mut StdRng::seed_from_u64(1)); 15];
    let mut bufs: Vec<Box<Chunk>> = (0..15).map(|_| new_chunk()).collect();
    let mut scratch = cpu_avx512::Scratch::new(1);
    cpu_avx512::entropy_lanes::<1>(Variant::A, &ids, 8_192, &mut bufs, &mut scratch);
}
