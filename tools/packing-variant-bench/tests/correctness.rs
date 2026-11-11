use openssl::sha::sha256;
use packing_variant_bench::{
    CHUNK_SIZE, ChunkId, Variant, entropy, entropy_a, entropy_b, fixed_id, new_chunk, reference_a,
    to_hex,
};
use rand::{Rng as _, SeedableRng as _, rngs::StdRng};

fn random_id(rng: &mut StdRng) -> ChunkId {
    ChunkId {
        address: rng.r#gen(),
        partition_hash: rng.r#gen(),
        chain_id: rng.r#gen(),
        partition_offset: rng.r#gen(),
    }
}

/// Variant A must be bit-identical to the unmodified irys-c reference implementation.
#[test]
fn variant_a_matches_reference() {
    let mut rng = StdRng::seed_from_u64(0x5eed);
    let mut buf = new_chunk();
    let mut reference = Vec::with_capacity(CHUNK_SIZE);
    // includes Phase-1-only, a partial sweep, wrap-around sweeps, and a non-multiple of 8192
    for iterations in [8_192_u32, 8_193, 12_345, 16_384, 40_000, 100_003] {
        for _ in 0..3 {
            let id = random_id(&mut rng);
            entropy_a(&id, iterations, &mut buf);
            reference_a(&id, iterations, &mut reference);
            assert_eq!(
                &buf[..],
                &reference[..],
                "iterations={iterations} id={id:?}"
            );
        }
    }
}

#[test]
fn variants_share_phase1_and_diverge_after() {
    let id = fixed_id(7);
    let (mut a, mut b) = (new_chunk(), new_chunk());
    entropy_a(&id, 8_192, &mut a);
    entropy_b(&id, 8_192, &mut b);
    assert_eq!(&a[..], &b[..], "Phase 1 must be identical");
    entropy_a(&id, 8_193, &mut a);
    entropy_b(&id, 8_193, &mut b);
    assert_ne!(&a[..32], &b[..32], "first Phase 2 step must differ");
    assert_eq!(
        &a[32..],
        &b[32..],
        "only segment 0 is written by one Phase 2 step"
    );
}

/// Variant B's first Phase 2 step, spelled out independently of the implementation.
#[test]
fn variant_b_first_step_by_hand() {
    let id = fixed_id(42);
    let mut p1 = new_chunk();
    entropy_b(&id, 8_192, &mut p1);
    let prev = &p1[CHUNK_SIZE - 32..];
    let j = (u32::from_le_bytes(prev[..4].try_into().unwrap()) & 8191) as usize;
    let mut input = prev.to_vec();
    input.extend_from_slice(&p1[j * 32..j * 32 + 32]);
    let expected = sha256(&input);

    let mut b = new_chunk();
    entropy_b(&id, 8_193, &mut b);
    assert_eq!(&b[..32], &expected);
}

/// Known-answer values: SHA-256 of the whole 256 KiB entropy chunk for `fixed_id(offset)`.
#[test]
fn known_answers() {
    #[rustfmt::skip]
    let cases: &[(Variant, u64, u32, &str)] = &[
        (Variant::A, 0, 8_192, "fac1dfc575b336951e82ba0a62454b354f4d989681c313ddf018932dbdb25072"),
        (Variant::B, 0, 8_192, "fac1dfc575b336951e82ba0a62454b354f4d989681c313ddf018932dbdb25072"),
        (Variant::A, 1, 50_000, "c7eab3cd0fee28efd2dbcd47cbc23d8888357b3541e4e1fa4fbaaee303e2f651"),
        (Variant::B, 1, 50_000, "b5343405942afce878268af4eea9d7fb9e469918b5cf848ad68a20df79d83782"),
        (Variant::A, 1_000, 1_000_000, "42e1392ff962e612a14b291987624bd2b91e50635e854f98b7aa4551295df884"),
        (Variant::B, 1_000, 1_000_000, "f40b6ae19c03dcf72a49424f67b81c4d78982b37ff4b97cf09081fb847ef29bb"),
    ];
    let mut buf = new_chunk();
    let mut failures = vec![];
    for &(v, off, it, want) in cases {
        entropy(v, &fixed_id(off), it, &mut buf);
        let got = to_hex(&sha256(&buf[..]));
        println!("{v:?} offset={off} iterations={it}: {got}");
        if got != want {
            failures.push(format!("{v:?} offset={off} iterations={it}: got {got}"));
        }
        if v == Variant::A {
            let mut reference = Vec::with_capacity(CHUNK_SIZE);
            reference_a(&fixed_id(off), it, &mut reference);
            assert_eq!(
                to_hex(&sha256(&reference)),
                want,
                "reference disagrees with A KAT"
            );
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
