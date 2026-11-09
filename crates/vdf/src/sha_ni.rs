//! x86_64 SHA-NI implementation of the VDF hash chain.
//!
//! Every VDF hash is `SHA256(salt_le || seed)`: 64 bytes of message, so two
//! compression blocks, `[salt || seed]` followed by a padding block that never
//! changes. [`crate::compress_n_rounds`] runs both blocks through the generic
//! `sha2::compress256` for every hash. This module computes the same chain
//! with the state kept in registers and three shortcuts that do not change
//! the output:
//!
//! - the padding block's message schedule plus round constants (`W + K`) is a
//!   compile-time constant ([`PAD_WK`]);
//! - rounds 0..8 only read the IV and the salt words, so they run once per
//!   checkpoint instead of once per hash;
//! - the digest words become the next message's words 8..16 directly, with no
//!   round trip through bytes.
//!
//! The salt still increments per checkpoint with the same little-endian
//! wrapping add as [`crate::increment_le_salt`]. [`crate::compress_n_rounds`]
//! remains the fallback when the CPU lacks SHA-NI and the oracle the tests
//! compare against.

use core::arch::x86_64::{
    __m128i, _mm_add_epi32, _mm_alignr_epi8, _mm_blend_epi16, _mm_extract_epi32, _mm_set_epi32,
    _mm_sha256msg1_epu32, _mm_sha256msg2_epu32, _mm_sha256rnds2_epu32, _mm_shuffle_epi32,
};
use irys_types::H256;

use crate::{SHA256_IV, increment_le_salt};

/// SHA-256 round constants.
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// `W[i] + K[i]` for the second block of a 64-byte message: `0x80`, zeros,
/// then the 512-bit message length (`crate::SHA256_64B_PADDING`).
const PAD_WK: [u32; 64] = {
    let mut w = [0_u32; 64];
    w[0] = 0x8000_0000;
    w[15] = 512;
    let mut i = 16;
    while i < 64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
        i += 1;
    }
    let mut i = 0;
    while i < 64 {
        w[i] = w[i].wrapping_add(K[i]);
        i += 1;
    }
    w
};

/// Proof that the running CPU supports every instruction this module uses.
/// Only [`ShaNi::detect`] constructs it, so holding one is what makes the
/// `#[target_feature]` calls below sound.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ShaNi(());

impl ShaNi {
    /// Runtime detection; never relies on the compile-time target features.
    /// `is_x86_feature_detected!` caches its result, so this is cheap to call
    /// once per chain.
    #[inline]
    pub(crate) fn detect() -> Option<Self> {
        (std::arch::is_x86_feature_detected!("sha")
            && std::arch::is_x86_feature_detected!("sse2")
            && std::arch::is_x86_feature_detected!("ssse3")
            && std::arch::is_x86_feature_detected!("sse4.1"))
        .then_some(Self(()))
    }

    /// Runs `N` independent VDF chains interleaved on this thread. Chain `j`
    /// behaves exactly like `vdf_sha(salts[j], seeds[j], len, iterations,
    /// checkpoints[j])`: for each of its checkpoints it hashes `iterations`
    /// times, stores the seed and increments the salt. Interleaving fills
    /// the SHA unit's pipeline with independent work, so `N = 2` nearly
    /// doubles throughput at a small latency cost per chain. Latency-bound
    /// callers (the VDF thread) use `N = 1`.
    ///
    /// # Panics
    /// If the checkpoint slices differ in length.
    pub(crate) fn chains<const N: usize>(
        self,
        salts: [[u8; 32]; N],
        seeds: [&mut H256; N],
        iterations: u64,
        checkpoints: [&mut [H256]; N],
    ) {
        if let Some(first) = checkpoints.first() {
            let len = first.len();
            assert!(
                checkpoints.iter().all(|c| c.len() == len),
                "interleaved VDF chains must have the same checkpoint count"
            );
        }
        // SAFETY: `self` is only constructed by `detect`, which checked every
        // feature `chains_impl` enables.
        unsafe { chains_impl(salts, seeds, iterations, checkpoints) }
    }
}

/// Per-checkpoint constants: everything derived from the salt alone.
#[derive(Clone, Copy)]
struct SaltCtx {
    /// Message words 0..4 and 4..8 (the salt).
    w0: __m128i,
    w1: __m128i,
    /// State after rounds 0..8, which read only the IV and the salt words.
    mid_abef: __m128i,
    mid_cdgh: __m128i,
}

/// Initial state in the `(ABEF, CDGH)` lane layout `sha256rnds2` uses.
#[inline]
#[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
fn iv_ni() -> (__m128i, __m128i) {
    let [a, b, c, d, e, f, g, h] = SHA256_IV.map(|w| w as i32);
    (_mm_set_epi32(a, b, e, f), _mm_set_epi32(c, d, g, h))
}

/// `K[4 * i..4 * i + 4]`, lane 0 first.
#[inline]
#[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
fn k4(table: &[u32; 64], i: usize) -> __m128i {
    _mm_set_epi32(
        table[4 * i + 3] as i32,
        table[4 * i + 2] as i32,
        table[4 * i + 1] as i32,
        table[4 * i] as i32,
    )
}

/// Message words from 16 big-endian bytes, lane 0 first.
#[inline]
#[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
fn be_words(bytes: &[u8]) -> __m128i {
    let w = |i: usize| u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    _mm_set_epi32(w(12) as i32, w(8) as i32, w(4) as i32, w(0) as i32)
}

/// Writes lanes 0..4 of `v` as big-endian words (digest byte order).
#[inline]
#[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
fn store_be_words(v: __m128i, out: &mut [u8]) {
    let lanes = [
        _mm_extract_epi32::<0>(v),
        _mm_extract_epi32::<1>(v),
        _mm_extract_epi32::<2>(v),
        _mm_extract_epi32::<3>(v),
    ];
    for (chunk, lane) in out.chunks_exact_mut(4).zip(lanes) {
        chunk.copy_from_slice(&(lane as u32).to_be_bytes());
    }
}

/// Four rounds with `wk = W + K` for those rounds.
#[inline]
#[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
fn rounds4(abef: &mut __m128i, cdgh: &mut __m128i, wk: __m128i) {
    *cdgh = _mm_sha256rnds2_epu32(*cdgh, *abef, wk);
    *abef = _mm_sha256rnds2_epu32(*abef, *cdgh, _mm_shuffle_epi32::<0x0E>(wk));
}

/// Next four message-schedule words from the previous sixteen.
#[inline]
#[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
fn schedule(w0: __m128i, w1: __m128i, w2: __m128i, w3: __m128i) -> __m128i {
    let t = _mm_add_epi32(_mm_sha256msg1_epu32(w0, w1), _mm_alignr_epi8::<4>(w3, w2));
    _mm_sha256msg2_epu32(t, w3)
}

/// `(ABEF, CDGH)` state to the digest words in order, as `(A..D, E..H)` with
/// lane 0 first: exactly the next message's words 8..12 and 12..16.
#[inline]
#[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
fn digest_words(abef: __m128i, cdgh: __m128i) -> (__m128i, __m128i) {
    let feba = _mm_shuffle_epi32::<0x1B>(abef);
    let dchg = _mm_shuffle_epi32::<0xB1>(cdgh);
    (
        _mm_blend_epi16::<0xF0>(feba, dchg),
        _mm_alignr_epi8::<8>(dchg, feba),
    )
}

#[inline]
#[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
fn salt_ctx(salt_le: &[u8; 32]) -> SaltCtx {
    let w0 = be_words(&salt_le[..16]);
    let w1 = be_words(&salt_le[16..]);
    let (mut abef, mut cdgh) = iv_ni();
    rounds4(&mut abef, &mut cdgh, _mm_add_epi32(w0, k4(&K, 0)));
    rounds4(&mut abef, &mut cdgh, _mm_add_epi32(w1, k4(&K, 1)));
    SaltCtx {
        w0,
        w1,
        mid_abef: abef,
        mid_cdgh: cdgh,
    }
}

/// One VDF hash: `SHA256(salt_le || seed)`, with the seed and the result as
/// digest words (`s0` = words 0..4, `s1` = words 4..8).
#[inline]
#[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
fn vdf_hash(ctx: &SaltCtx, s0: __m128i, s1: __m128i) -> (__m128i, __m128i) {
    // Block 0: rounds 0..8 come from the salt midstate.
    let (mut abef, mut cdgh) = (ctx.mid_abef, ctx.mid_cdgh);
    let (mut w0, mut w1, mut w2, mut w3) = (ctx.w0, ctx.w1, s0, s1);
    let mut w4;
    rounds4(&mut abef, &mut cdgh, _mm_add_epi32(w2, k4(&K, 2)));
    rounds4(&mut abef, &mut cdgh, _mm_add_epi32(w3, k4(&K, 3)));
    // Rotating five registers through the 16-word schedule window.
    macro_rules! schedule_rounds4 {
        ($v0:ident, $v1:ident, $v2:ident, $v3:ident, $v4:ident, $i:expr) => {
            $v4 = schedule($v0, $v1, $v2, $v3);
            rounds4(&mut abef, &mut cdgh, _mm_add_epi32($v4, k4(&K, $i)));
        };
    }
    schedule_rounds4!(w0, w1, w2, w3, w4, 4);
    schedule_rounds4!(w1, w2, w3, w4, w0, 5);
    schedule_rounds4!(w2, w3, w4, w0, w1, 6);
    schedule_rounds4!(w3, w4, w0, w1, w2, 7);
    schedule_rounds4!(w4, w0, w1, w2, w3, 8);
    schedule_rounds4!(w0, w1, w2, w3, w4, 9);
    schedule_rounds4!(w1, w2, w3, w4, w0, 10);
    schedule_rounds4!(w2, w3, w4, w0, w1, 11);
    schedule_rounds4!(w3, w4, w0, w1, w2, 12);
    schedule_rounds4!(w4, w0, w1, w2, w3, 13);
    schedule_rounds4!(w0, w1, w2, w3, w4, 14);
    schedule_rounds4!(w1, w2, w3, w4, w0, 15);
    let _ = (w1, w2, w3, w4);

    // Feed-forward: the chaining value after block 0.
    let (iv_abef, iv_cdgh) = iv_ni();
    let mid_abef = _mm_add_epi32(abef, iv_abef);
    let mid_cdgh = _mm_add_epi32(cdgh, iv_cdgh);

    // Block 1: the constant padding block, `W + K` precomputed.
    let (mut abef, mut cdgh) = (mid_abef, mid_cdgh);
    macro_rules! pad_rounds4 {
        ($($i:expr),*) => {
            $(rounds4(&mut abef, &mut cdgh, k4(&PAD_WK, $i));)*
        };
    }
    pad_rounds4!(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);

    digest_words(_mm_add_epi32(abef, mid_abef), _mm_add_epi32(cdgh, mid_cdgh))
}

/// # Safety
/// The CPU must support SHA-NI, SSE2, SSSE3 and SSE4.1.
#[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
unsafe fn chains_impl<const N: usize>(
    mut salts: [[u8; 32]; N],
    seeds: [&mut H256; N],
    iterations: u64,
    checkpoints: [&mut [H256]; N],
) {
    let num_checkpoints = checkpoints.first().map_or(0, |c| c.len());
    let mut state: [(__m128i, __m128i); N] = core::array::from_fn(|j| {
        let seed = seeds[j].as_bytes();
        (be_words(&seed[..16]), be_words(&seed[16..]))
    });

    for checkpoint_idx in 0..num_checkpoints {
        if checkpoint_idx > 0 {
            for salt in &mut salts {
                increment_le_salt(salt);
            }
        }
        let ctx: [SaltCtx; N] = core::array::from_fn(|j| salt_ctx(&salts[j]));
        for _ in 0..iterations {
            for j in 0..N {
                state[j] = vdf_hash(&ctx[j], state[j].0, state[j].1);
            }
        }
        for j in 0..N {
            let slot = checkpoints[j][checkpoint_idx].as_mut();
            store_be_words(state[j].0, &mut slot[..16]);
            store_be_words(state[j].1, &mut slot[16..]);
            *seeds[j] = checkpoints[j][checkpoint_idx];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vdf_sha_compress256;
    use irys_types::U256;
    use proptest::prelude::*;

    /// Salts anywhere in the range, plus salts a few increments below
    /// `U256::MAX` so the per-checkpoint increment wraps around to zero.
    fn salt_strategy() -> impl Strategy<Value = U256> {
        prop_oneof![
            any::<[u64; 4]>().prop_map(U256),
            (0_u64..8).prop_map(|below| U256::MAX - U256::from(below)),
            (0_u64..8).prop_map(U256::from),
        ]
    }

    fn salt_le(salt: U256) -> [u8; 32] {
        let mut bytes = [0_u8; 32];
        salt.to_little_endian(&mut bytes);
        bytes
    }

    /// `(final seed, checkpoints)` from the `sha2::compress256` oracle.
    fn oracle(
        salt: U256,
        seed: H256,
        num_checkpoints: usize,
        iterations: u64,
    ) -> (H256, Vec<H256>) {
        let mut seed = seed;
        let mut checkpoints = vec![H256::zero(); num_checkpoints];
        vdf_sha_compress256(
            salt,
            &mut seed,
            num_checkpoints,
            iterations,
            &mut checkpoints,
        );
        (seed, checkpoints)
    }

    fn detect_or_skip() -> Option<ShaNi> {
        let sha_ni = ShaNi::detect();
        if sha_ni.is_none() {
            eprintln!("CPU lacks SHA-NI; skipping SHA-NI equivalence test");
        }
        sha_ni
    }

    #[test]
    fn pad_wk_matches_padding_block() {
        let pad = crate::SHA256_64B_PADDING;
        for i in 0..16 {
            let w = u32::from_be_bytes(pad[4 * i..4 * i + 4].try_into().unwrap());
            assert_eq!(PAD_WK[i], w.wrapping_add(K[i]), "word {i}");
        }
    }

    proptest! {
        #[test]
        fn single_chain_matches_compress256(
            salt in salt_strategy(),
            seed in any::<[u8; 32]>(),
            num_checkpoints in 0..=5_usize,
            iterations in 0..=8_u64,
        ) {
            let Some(sha_ni) = detect_or_skip() else { return Ok(()) };
            let (expected_seed, expected_checkpoints) =
                oracle(salt, H256(seed), num_checkpoints, iterations);

            let mut seed = H256(seed);
            let mut checkpoints = vec![H256::zero(); num_checkpoints];
            sha_ni.chains([salt_le(salt)], [&mut seed], iterations, [&mut checkpoints]);

            prop_assert_eq!(seed, expected_seed);
            prop_assert_eq!(checkpoints, expected_checkpoints);
        }

        #[test]
        fn interleaved_chains_match_compress256(
            salts in [salt_strategy(), salt_strategy()],
            seeds in any::<[[u8; 32]; 2]>(),
            num_checkpoints in 0..=5_usize,
            iterations in 0..=8_u64,
        ) {
            let Some(sha_ni) = detect_or_skip() else { return Ok(()) };
            let expected =
                [0, 1].map(|j| oracle(salts[j], H256(seeds[j]), num_checkpoints, iterations));

            let [mut seed_a, mut seed_b] = seeds.map(H256);
            let mut checkpoints_a = vec![H256::zero(); num_checkpoints];
            let mut checkpoints_b = vec![H256::zero(); num_checkpoints];
            sha_ni.chains(
                salts.map(salt_le),
                [&mut seed_a, &mut seed_b],
                iterations,
                [&mut checkpoints_a, &mut checkpoints_b],
            );

            prop_assert_eq!(&(seed_a, checkpoints_a), &expected[0]);
            prop_assert_eq!(&(seed_b, checkpoints_b), &expected[1]);
        }

        /// The public entry point, whichever path it dispatches to.
        #[test]
        fn vdf_sha_matches_compress256(
            salt in salt_strategy(),
            seed in any::<[u8; 32]>(),
            num_checkpoints in 0..=5_usize,
            iterations in 0..=8_u64,
        ) {
            let (expected_seed, expected_checkpoints) =
                oracle(salt, H256(seed), num_checkpoints, iterations);
            let mut seed = H256(seed);
            // One spare slot: `vdf_sha` must fill only the first `num_checkpoints`.
            let mut checkpoints = vec![H256::repeat_byte(0xEE); num_checkpoints + 1];
            crate::vdf_sha(salt, &mut seed, num_checkpoints, iterations, &mut checkpoints);

            prop_assert_eq!(seed, expected_seed);
            prop_assert_eq!(&checkpoints[..num_checkpoints], &expected_checkpoints[..]);
            prop_assert_eq!(checkpoints[num_checkpoints], H256::repeat_byte(0xEE));
        }
    }

    /// Long chains in the 25-checkpoint step shape, far beyond what the
    /// proptests reach: every checkpoint of the single-chain and the
    /// interleaved paths must equal the `compress256` oracle. The near-MAX
    /// salts wrap to zero mid-step (after checkpoint 11 and 4 respectively).
    #[rstest::rstest]
    #[case::low_salts(U256::from(1), U256::from(26))]
    #[case::wrapping_salts(U256::MAX - U256::from(10), U256::MAX - U256::from(3))]
    fn long_chains_match_compress256(#[case] salt_a: U256, #[case] salt_b: U256) {
        const CHECKPOINTS: usize = 25;
        const ITERATIONS: u64 = 100_000;
        let Some(sha_ni) = detect_or_skip() else {
            return;
        };
        let (seed_a, seed_b) = (H256::repeat_byte(0xAB), H256::repeat_byte(0x5C));
        let expected_a = oracle(salt_a, seed_a, CHECKPOINTS, ITERATIONS);
        let expected_b = oracle(salt_b, seed_b, CHECKPOINTS, ITERATIONS);

        let mut seed = seed_a;
        let mut checkpoints = vec![H256::zero(); CHECKPOINTS];
        sha_ni.chains(
            [salt_le(salt_a)],
            [&mut seed],
            ITERATIONS,
            [&mut checkpoints],
        );
        assert_eq!((seed, checkpoints), expected_a, "single chain");

        let (mut pair_seed_a, mut pair_seed_b) = (seed_a, seed_b);
        let mut checkpoints_a = vec![H256::zero(); CHECKPOINTS];
        let mut checkpoints_b = vec![H256::zero(); CHECKPOINTS];
        sha_ni.chains(
            [salt_le(salt_a), salt_le(salt_b)],
            [&mut pair_seed_a, &mut pair_seed_b],
            ITERATIONS,
            [&mut checkpoints_a, &mut checkpoints_b],
        );
        assert_eq!(
            (pair_seed_a, checkpoints_a),
            expected_a,
            "interleaved chain a"
        );
        assert_eq!(
            (pair_seed_b, checkpoints_b),
            expected_b,
            "interleaved chain b"
        );
    }
}
