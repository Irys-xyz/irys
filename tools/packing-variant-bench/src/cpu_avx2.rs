//! Multi-lane AVX2 implementation of both variants, for CPUs without SHA-NI: software SHA-256
//! on 8 chunks at once, one chunk per u32 lane of a `__m256i`.
//!
//! AVX2 has no vector rotate or ternary logic, so a rotate costs shift + shift + or and every
//! boolean function is built from 2-input ops. The round below is arranged for the fewest ops:
//! `Ch = ((f ^ g) & e) ^ g` (3 ops) and `Maj = b ^ ((a ^ b) & (b ^ c))`, where `b ^ c` is the
//! previous round's `a ^ b`, so Maj costs 3 ops with one of them reused.
//!
//! `entropy_lanes::<G>` runs G groups of 8 chunks per call. Scratch layout per segment matches
//! the AVX-512 path (see [`crate::cpu_avx512`]): word planes for A, lane rows for B (so B's
//! random read of one lane touches one cache line). The scratch is converted to the exact
//! reference byte order of each chunk at the end.
//!
//! This path is optional: it is selected at runtime (`available()`), and every entry point falls
//! back to [`crate::cpu_fast`] (SHA-NI, else OpenSSL) when AVX2 is absent.
//!
//! Output is byte-identical to [`crate::entropy_a`] / [`crate::entropy_b`].

use crate::{Chunk, ChunkId, SEGMENTS, Variant};

/// Chunks per group (u32 lanes in a 256-bit vector).
pub const LANES: usize = 8;

/// One segment (32 bytes) of each of the 8 chunks of a group, as 64 native-endian state words.
/// Variant A: `0[w * 8 + l]` (word planes); variant B: `0[l * 8 + w]` (lane rows).
#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Seg([u32; 8 * LANES]);

/// Reusable working memory: 2 MiB per group of 8 chunks.
pub struct Scratch {
    segs: Vec<Seg>,
}

impl Scratch {
    pub fn new(groups: usize) -> Self {
        Self {
            segs: vec![Seg([0; 8 * LANES]); groups * SEGMENTS],
        }
    }
}

/// How variant B reads its data-dependent segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BRead {
    /// 8 `vpgatherdd`, one per word.
    Gather,
    /// 8 row loads (one 32-byte load per lane) and an 8x8 transpose.
    Transpose,
}

/// The `BRead` used by [`entropy_lanes`] (the faster one measured on Zen 5).
pub const DEFAULT_B_READ: BRead = BRead::Transpose;

/// True when this CPU supports the AVX2 path. Detected at runtime; nothing in this crate
/// requires AVX2.
pub fn available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        is_x86_feature_detected!("avx2")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// Computes `8 * G` chunks: `bufs[k]` for `ids[k]`. `scratch` is grown to G groups if needed and
/// can be reused across calls to avoid reallocating 2 MiB per group.
///
/// Panics unless `ids.len() == bufs.len() == 8 * G`.
pub fn entropy_lanes<const G: usize>(
    variant: Variant,
    ids: &[ChunkId],
    iterations: u32,
    bufs: &mut [Box<Chunk>],
    scratch: &mut Scratch,
) {
    entropy_lanes_with::<G>(variant, ids, iterations, bufs, scratch, DEFAULT_B_READ);
}

/// [`entropy_lanes`] with an explicit B read strategy (ignored for variant A).
pub fn entropy_lanes_with<const G: usize>(
    variant: Variant,
    ids: &[ChunkId],
    iterations: u32,
    bufs: &mut [Box<Chunk>],
    scratch: &mut Scratch,
    b_read: BRead,
) {
    assert_eq!(ids.len(), LANES * G, "ids must hold 8 * G chunk ids");
    assert_eq!(bufs.len(), LANES * G, "bufs must hold 8 * G chunks");
    #[cfg(target_arch = "x86_64")]
    if available() {
        if scratch.segs.len() < G * SEGMENTS {
            *scratch = Scratch::new(G);
        }
        let segs = &mut scratch.segs[..G * SEGMENTS];
        // SAFETY: AVX2 was detected above; `segs` holds exactly G * SEGMENTS segments.
        unsafe { imp::run::<G>(variant, b_read, ids, iterations, segs) };
        deinterleave(variant, segs, bufs);
        return;
    }
    let _ = b_read;
    for (id, buf) in ids.iter().zip(bufs.iter_mut()) {
        crate::cpu_fast::entropy(variant, id, iterations, buf);
    }
}

/// Scratch layout of `variant` (native-endian words) -> per-chunk reference (big-endian) bytes.
fn deinterleave(variant: Variant, segs: &[Seg], bufs: &mut [Box<Chunk>]) {
    let (lane_stride, word_stride) = match variant {
        Variant::A => (1, LANES),
        Variant::B => (8, 1),
    };
    for (g, group) in bufs.chunks_mut(LANES).enumerate() {
        let group_segs = &segs[g * SEGMENTS..(g + 1) * SEGMENTS];
        for (l, buf) in group.iter_mut().enumerate() {
            for (seg, out) in group_segs.iter().zip(buf.chunks_exact_mut(32)) {
                for (w, bytes) in out.chunks_exact_mut(4).enumerate() {
                    let word = seg.0[l * lane_stride + w * word_stride];
                    bytes.copy_from_slice(&word.to_be_bytes());
                }
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod imp {
    use super::{BRead, LANES, Seg};
    use crate::cpu_fast::{IV, K, PAD64_WK};
    use crate::{ChunkId, SEGMENTS, Variant};
    use core::arch::x86_64::*;

    type V = __m256i;
    /// A 32-byte value as 8 word vectors (also the a..h hash state).
    type Words = [V; 8];

    /// Working state: a..h plus `bc` = b ^ c, carried between rounds for Maj.
    #[derive(Clone, Copy)]
    struct State {
        s: Words,
        bc: V,
    }

    /// u32 offset between consecutive segments in the scratch buffer.
    const SEG_WORDS: usize = 8 * LANES;

    #[inline]
    #[target_feature(enable = "avx2")]
    fn add(a: V, b: V) -> V {
        _mm256_add_epi32(a, b)
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    fn xor(a: V, b: V) -> V {
        _mm256_xor_si256(a, b)
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    fn splat(x: u32) -> V {
        _mm256_set1_epi32(x as i32)
    }

    /// Rotate right by R; L must be 32 - R (const generic arithmetic is not stable).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn ror<const R: i32, const L: i32>(x: V) -> V {
        const { assert!(R + L == 32) };
        _mm256_or_si256(_mm256_srli_epi32::<R>(x), _mm256_slli_epi32::<L>(x))
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    fn state(s: Words) -> State {
        State {
            s,
            bc: xor(s[1], s[2]),
        }
    }

    /// One SHA-256 round with `kw` = K[t] + W[t].
    #[inline]
    #[target_feature(enable = "avx2")]
    fn round(st: &mut State, kw: V) {
        let [a, b, c, d, e, f, g, h] = st.s;
        let s1 = xor(xor(ror::<6, 26>(e), ror::<11, 21>(e)), ror::<25, 7>(e));
        let ch = xor(_mm256_and_si256(xor(f, g), e), g);
        let t1 = add(add(h, s1), add(ch, kw));
        let s0 = xor(xor(ror::<2, 30>(a), ror::<13, 19>(a)), ror::<22, 10>(a));
        let ab = xor(a, b);
        let maj = xor(b, _mm256_and_si256(ab, st.bc));
        st.s = [add(t1, add(s0, maj)), a, b, c, add(d, t1), e, f, g];
        st.bc = ab;
    }

    /// Round T of a data block for all G groups; extends the schedule ring `w` when T >= 16.
    /// T is a const so every ring index is a constant and `w` stays in registers.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn data_round<const G: usize, const T: usize>(s: &mut [State; G], w: &mut [[V; 16]; G]) {
        for g in 0..G {
            let w = &mut w[g];
            if T >= 16 {
                let w15 = w[(T + 1) % 16];
                let w2 = w[(T + 14) % 16];
                let sigma0 = xor(
                    xor(ror::<7, 25>(w15), ror::<18, 14>(w15)),
                    _mm256_srli_epi32::<3>(w15),
                );
                let sigma1 = xor(
                    xor(ror::<17, 15>(w2), ror::<19, 13>(w2)),
                    _mm256_srli_epi32::<10>(w2),
                );
                w[T % 16] = add(add(w[T % 16], sigma0), add(w[(T + 9) % 16], sigma1));
            }
            round(&mut s[g], add(w[T % 16], splat(K[T])));
        }
    }

    /// Round T of the constant padding block of a 64-byte message (W + K is a constant).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn pad_round<const G: usize, const T: usize>(s: &mut [State; G]) {
        let kw = splat(PAD64_WK.0[T]);
        for s in s.iter_mut() {
            round(s, kw);
        }
    }

    /// Expands `$f::<G, T>(args)` for T in 0..64, so every round has a const index.
    macro_rules! rounds64 {
        ($f:ident, $g:tt, $($arg:expr),*) => {
            rounds64!(@ $f, $g, ($($arg),*),
                0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15
                16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31
                32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47
                48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63)
        };
        (@ $f:ident, $g:tt, $args:tt, $($t:literal)*) => {
            $( rounds64!(@call $f, $g, $t, $args); )*
        };
        (@call $f:ident, $g:tt, $t:literal, ($($arg:expr),*)) => {
            $f::<$g, $t>($($arg),*)
        };
    }

    /// Compresses one data block (message words `w`) into `s` for all G groups.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn compress<const G: usize>(s: &mut [Words; G], w: &mut [[V; 16]; G]) {
        let mut st = s.map(|s| state(s));
        rounds64!(data_round, G, &mut st, w);
        feed_forward(s, &st);
    }

    /// Davies-Meyer feed-forward: chaining value += compressed state.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn feed_forward<const G: usize>(s: &mut [Words; G], st: &[State; G]) {
        for (s, st) in s.iter_mut().zip(st) {
            for (x, &y) in s.iter_mut().zip(&st.s) {
                *x = add(*x, y);
            }
        }
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    fn iv() -> Words {
        IV.map(|x| splat(x))
    }

    /// SHA-256 of the 64-byte messages `left || right`.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn sha64<const G: usize>(left: &[Words; G], right: &[Words; G]) -> [Words; G] {
        let mut s = [iv(); G];
        let mut w: [[V; 16]; G] = core::array::from_fn(|g| {
            core::array::from_fn(|k| if k < 8 { left[g][k] } else { right[g][k - 8] })
        });
        compress(&mut s, &mut w);
        let mut st = s.map(|s| state(s));
        rounds64!(pad_round, G, &mut st);
        feed_forward(&mut s, &st);
        s
    }

    /// SHA-256 of the 32-byte messages `msg` (one block; words 8..15 are constant padding).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn sha32<const G: usize>(msg: &[Words; G]) -> [Words; G] {
        let mut s = [iv(); G];
        let mut w: [[V; 16]; G] = core::array::from_fn(|g| {
            core::array::from_fn(|k| match k {
                0..8 => msg[g][k],
                8 => splat(0x8000_0000),
                15 => splat(256),
                _ => splat(0),
            })
        });
        compress(&mut s, &mut w);
        s
    }

    /// Seeds of one group: SHA-256 of the 68-byte (two-block) seed message of each lane.
    #[target_feature(enable = "avx2")]
    fn seeds(ids: &[ChunkId]) -> Words {
        // words[k][l] = big-endian word k of lane l's padded 128-byte message
        let mut words = [[0_u32; LANES]; 32];
        for (l, id) in ids.iter().enumerate() {
            let mut bytes = [0_u8; 128];
            bytes[..20].copy_from_slice(&id.address);
            bytes[20..52].copy_from_slice(&id.partition_hash);
            bytes[52..60].copy_from_slice(&id.chain_id.to_le_bytes());
            bytes[60..68].copy_from_slice(&id.partition_offset.to_le_bytes());
            bytes[68] = 0x80;
            bytes[120..128].copy_from_slice(&(68_u64 * 8).to_be_bytes());
            for (k, word) in words.iter_mut().enumerate() {
                word[l] = u32::from_be_bytes(bytes[k * 4..k * 4 + 4].try_into().unwrap());
            }
        }
        // SAFETY: each `words[k]` is 8 readable u32s; unaligned load.
        let vec = |k: usize| unsafe { _mm256_loadu_si256(words[k].as_ptr().cast()) };
        let mut s = [iv()];
        compress(&mut s, &mut [core::array::from_fn(vec)]);
        compress(&mut s, &mut [core::array::from_fn(|k| vec(16 + k))]);
        s[0]
    }

    /// 8x8 u32 transpose (its own inverse): rows <-> word planes. 24 shuffles.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn transpose(r: &Words) -> Words {
        let t: [V; 8] = core::array::from_fn(|k| {
            let (x, y) = (r[k / 2 * 2], r[k / 2 * 2 + 1]);
            if k % 2 == 0 {
                _mm256_unpacklo_epi32(x, y)
            } else {
                _mm256_unpackhi_epi32(x, y)
            }
        });
        // u[4 * half + e], e = 0..4: element e (and e + 4 in the high 128 bits) of rows
        // 4 * half..4 * half + 4
        let u: [V; 8] = core::array::from_fn(|k| {
            let base = (k / 4) * 4;
            let e = k % 4;
            let (x, y) = (t[base + e / 2], t[base + e / 2 + 2]);
            if e % 2 == 0 {
                _mm256_unpacklo_epi64(x, y)
            } else {
                _mm256_unpackhi_epi64(x, y)
            }
        });
        core::array::from_fn(|e| {
            if e < 4 {
                _mm256_permute2x128_si256::<0x20>(u[e], u[4 + e])
            } else {
                _mm256_permute2x128_si256::<0x31>(u[e - 4], u[e])
            }
        })
    }

    /// Variant A layout (word planes): reads segment i.
    ///
    /// # Safety
    /// `base` must point to the start of a group's SEGMENTS segments; `i < SEGMENTS`.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn load_planes(base: *const u32, i: usize) -> Words {
        // SAFETY: segment i lies within the group (caller contract); each plane is 32 bytes at
        // a 32-byte aligned offset of the `align(64)` `Seg` array.
        core::array::from_fn(|w| unsafe {
            _mm256_load_si256(base.add(i * SEG_WORDS + w * LANES).cast())
        })
    }

    /// Writes 8 vectors to segment i: word planes for A, lane rows (already transposed) for B.
    ///
    /// # Safety
    /// As for `load_planes`, and the memory must be writable and not otherwise borrowed.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn store_seg(base: *mut u32, i: usize, v: &Words) {
        for (k, &x) in v.iter().enumerate() {
            // SAFETY: as in `load_planes`.
            unsafe { _mm256_store_si256(base.add(i * SEG_WORDS + k * LANES).cast(), x) };
        }
    }

    /// Variant B's segment index per lane: the LE u32 of the first 4 bytes of the previous
    /// output, & 8191. Those bytes are H0 big-endian, so j = bswap(H0) & 8191
    /// = (H0 >> 24) | ((H0 >> 8) & 0x1f00).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn b_index(prev: &Words) -> V {
        let h0 = prev[0];
        _mm256_or_si256(
            _mm256_srli_epi32::<24>(h0),
            _mm256_and_si256(_mm256_srli_epi32::<8>(h0), splat(0x1f00)),
        )
    }

    /// Variant B's read (lane rows) with 8 gathers, one per word.
    ///
    /// # Safety
    /// `base` must point to the start of a group's SEGMENTS segments.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn gather_rows(base: *const u32, prev: &Words) -> Words {
        let lane8 = _mm256_set_epi32(56, 48, 40, 32, 24, 16, 8, 0);
        // u32 index of (segment j, lane l, word 0); word w adds w via the base pointer
        let idx = add(_mm256_slli_epi32::<6>(b_index(prev)), lane8);
        core::array::from_fn(|w| {
            // SAFETY: j < SEGMENTS, l < 8 and w < 8, so every index stays within the group's
            // SEGMENTS * 64 u32s (caller contract).
            unsafe { _mm256_i32gather_epi32::<4>(base.add(w).cast(), idx) }
        })
    }

    /// Variant B's read (lane rows) with one 32-byte load per lane and a transpose.
    ///
    /// # Safety
    /// As for `gather_rows`.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn load_rows(base: *const u32, prev: &Words) -> Words {
        let mut j = [0_u32; LANES];
        // SAFETY: `j` is 8 writable u32s; unaligned store.
        unsafe { _mm256_storeu_si256(j.as_mut_ptr().cast(), b_index(prev)) };
        let rows: Words = core::array::from_fn(|l| {
            // SAFETY: j[l] < SEGMENTS (masked to 13 bits), l < 8: within the group; rows are
            // 32-byte aligned.
            unsafe { _mm256_load_si256(base.add(j[l] as usize * SEG_WORDS + l * 8).cast()) }
        });
        transpose(&rows)
    }

    /// # Safety
    /// The CPU must support AVX2. `segs` must hold exactly G * SEGMENTS segments and `ids`
    /// 8 * G ids.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn run<const G: usize>(
        variant: Variant,
        b_read: BRead,
        ids: &[ChunkId],
        iterations: u32,
        segs: &mut [Seg],
    ) {
        assert_eq!(segs.len(), G * SEGMENTS);
        assert_eq!(ids.len(), G * LANES);
        // SAFETY: forwarded caller contract.
        unsafe {
            match (variant, b_read) {
                (Variant::A, _) => run_variant::<G, false, false>(ids, iterations, segs),
                (Variant::B, BRead::Gather) => run_variant::<G, true, true>(ids, iterations, segs),
                (Variant::B, BRead::Transpose) => {
                    run_variant::<G, true, false>(ids, iterations, segs)
                }
            }
        }
    }

    /// `B == false`: variant A with the word-plane layout; `B == true`: variant B with the
    /// lane-row layout, reading with gathers when `GATHER`.
    ///
    /// # Safety
    /// As for `run`.
    #[target_feature(enable = "avx2")]
    unsafe fn run_variant<const G: usize, const B: bool, const GATHER: bool>(
        ids: &[ChunkId],
        iterations: u32,
        segs: &mut [Seg],
    ) {
        // All accesses below go through this one pointer; group g starts at g * SEGMENTS
        // segments.
        let base = segs.as_mut_ptr().cast::<u32>();
        let group = |g: usize| {
            // SAFETY: g < G, so the offset stays within `segs`.
            unsafe { base.add(g * SEGMENTS * SEG_WORDS) }
        };
        let store = |g: usize, i: usize, p: &Words| {
            let v = if B { transpose(p) } else { *p };
            // SAFETY: i < SEGMENTS (callers below) and group(g) is this group's exclusively
            // borrowed memory.
            unsafe { store_seg(group(g), i, &v) }
        };

        // Phase 1: prev = seed; buf[i] = prev = SHA(prev)
        let mut prev: [Words; G] = core::array::from_fn(|g| seeds(&ids[g * LANES..][..LANES]));
        for i in 0..SEGMENTS {
            prev = sha32(&prev);
            for (g, p) in prev.iter().enumerate() {
                store(g, i, p);
            }
        }

        // Phase 2. `prev` is the previous step's output; for A it is also buf[i - 1].
        let iterations = iterations as usize;
        let mut hash_count = SEGMENTS;
        while hash_count < iterations {
            let i = hash_count % SEGMENTS;
            let right: [Words; G] = core::array::from_fn(|g| {
                // SAFETY: reads this group's memory at segments < SEGMENTS.
                unsafe {
                    match (B, GATHER) {
                        (false, _) => load_planes(group(g), i),
                        (true, true) => gather_rows(group(g), &prev[g]),
                        (true, false) => load_rows(group(g), &prev[g]),
                    }
                }
            });
            prev = sha64(&prev, &right);
            for (g, p) in prev.iter().enumerate() {
                store(g, i, p);
            }
            hash_count += 1;
        }
    }
}
