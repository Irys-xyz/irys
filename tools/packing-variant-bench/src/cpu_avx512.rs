//! Multi-lane AVX-512 implementation of both variants: software SHA-256 on 16 chunks at once,
//! one chunk per u32 lane of a `__m512i`.
//!
//! Unlike SHA-NI, which is limited by the throughput of a single SHA unit per core, the scalar
//! SHA-256 round maps onto plain 512-bit ALU ops (`vprord`, `vpternlogd`, `vpaddd`), which Zen 5
//! issues on several pipes, so 16 independent chains per vector give more hashes per core.
//!
//! `entropy_lanes::<G>` runs G groups of 16 chunks (G = 1 or 2 is useful) per call. While
//! running, segment s of all 16 chunks of a group is one 512-byte block of a scratch buffer,
//! holding native-endian state words in a per-variant layout (see [`Seg`]). The scratch is
//! converted to the exact reference byte order of each chunk at the end.
//!
//! This path is optional: it is selected at runtime (`available()`), and every entry point falls
//! back to [`crate::cpu_fast`] (SHA-NI, else OpenSSL) when AVX-512F is absent.
//!
//! Output is byte-identical to [`crate::entropy_a`] / [`crate::entropy_b`].

use crate::{Chunk, ChunkId, SEGMENTS, Variant};

/// Chunks per group (u32 lanes in a 512-bit vector).
pub const LANES: usize = 16;

/// One segment (32 bytes) of each of the 16 chunks of a group, as 128 native-endian state words.
///
/// - Variant A, word planes: `0[w * 16 + l]` is word w of lane l. A reads and writes segment i
///   with plain aligned vector loads/stores.
/// - Variant B, lane rows: `0[l * 8 + w]` is word w of lane l. B reads one random segment per
///   lane; with lane rows each lane's 32 bytes share one cache line, so the 8 `vpgatherdd`s of a
///   step touch 16 lines instead of 128 (word planes put each word in a different line). That
///   matters once the scratch of all threads spills out of L3. The price is an 8x16 transpose
///   (24 `vpermt2d`) per store.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Seg([u32; 8 * LANES]);

/// Reusable working memory: 4 MiB per group of 16 chunks.
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

/// True when this CPU supports the AVX-512F path. Detected at runtime; nothing in this crate
/// requires AVX-512.
pub fn available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        is_x86_feature_detected!("avx512f")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// Computes `16 * G` chunks: `bufs[k]` for `ids[k]`. `scratch` is grown to G groups if needed and
/// can be reused across calls to avoid reallocating 4 MiB per group.
///
/// Panics unless `ids.len() == bufs.len() == 16 * G`.
pub fn entropy_lanes<const G: usize>(
    variant: Variant,
    ids: &[ChunkId],
    iterations: u32,
    bufs: &mut [Box<Chunk>],
    scratch: &mut Scratch,
) {
    assert_eq!(ids.len(), LANES * G, "ids must hold 16 * G chunk ids");
    assert_eq!(bufs.len(), LANES * G, "bufs must hold 16 * G chunks");
    #[cfg(target_arch = "x86_64")]
    if available() {
        if scratch.segs.len() < G * SEGMENTS {
            *scratch = Scratch::new(G);
        }
        let segs = &mut scratch.segs[..G * SEGMENTS];
        // SAFETY: AVX-512F was detected above; `segs` holds exactly G * SEGMENTS segments.
        unsafe { imp::run::<G>(variant, ids, iterations, segs) };
        deinterleave(variant, segs, bufs);
        return;
    }
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
    use super::{LANES, Seg};
    use crate::cpu_fast::{IV, K, PAD64_WK};
    use crate::{ChunkId, SEGMENTS, Variant};
    use core::arch::x86_64::*;

    type V = __m512i;
    /// SHA-256 working state a..h, or a 32-byte value as 8 word vectors.
    type State = [V; 8];

    /// u32 offset between consecutive segments in the scratch buffer.
    const SEG_WORDS: usize = 8 * LANES;

    #[inline]
    #[target_feature(enable = "avx512f")]
    fn add(a: V, b: V) -> V {
        _mm512_add_epi32(a, b)
    }

    #[inline]
    #[target_feature(enable = "avx512f")]
    fn splat(x: u32) -> V {
        _mm512_set1_epi32(x as i32)
    }

    // vpternlogd truth tables (bit index = a << 2 | b << 1 | c): 0x96 = a ^ b ^ c,
    // 0xCA = a ? b : c (Ch), 0xE8 = majority(a, b, c) (Maj).
    #[inline]
    #[target_feature(enable = "avx512f")]
    fn xor3(a: V, b: V, c: V) -> V {
        _mm512_ternarylogic_epi32::<0x96>(a, b, c)
    }

    /// One SHA-256 round with `kw` = K[t] + W[t].
    #[inline]
    #[target_feature(enable = "avx512f")]
    fn round(s: &mut State, kw: V) {
        let [a, b, c, d, e, f, g, h] = *s;
        let s1 = xor3(
            _mm512_ror_epi32::<6>(e),
            _mm512_ror_epi32::<11>(e),
            _mm512_ror_epi32::<25>(e),
        );
        let ch = _mm512_ternarylogic_epi32::<0xCA>(e, f, g);
        let t1 = add(add(h, s1), add(ch, kw));
        let s0 = xor3(
            _mm512_ror_epi32::<2>(a),
            _mm512_ror_epi32::<13>(a),
            _mm512_ror_epi32::<22>(a),
        );
        let maj = _mm512_ternarylogic_epi32::<0xE8>(a, b, c);
        *s = [add(t1, add(s0, maj)), a, b, c, add(d, t1), e, f, g];
    }

    /// Round T of a data block for all G groups; extends the schedule ring `w` when T >= 16.
    /// T is a const so every ring index is a constant and `w` stays in registers.
    #[inline]
    #[target_feature(enable = "avx512f")]
    fn data_round<const G: usize, const T: usize>(s: &mut [State; G], w: &mut [[V; 16]; G]) {
        for g in 0..G {
            let w = &mut w[g];
            if T >= 16 {
                let w15 = w[(T + 1) % 16];
                let w2 = w[(T + 14) % 16];
                let sigma0 = xor3(
                    _mm512_ror_epi32::<7>(w15),
                    _mm512_ror_epi32::<18>(w15),
                    _mm512_srli_epi32::<3>(w15),
                );
                let sigma1 = xor3(
                    _mm512_ror_epi32::<17>(w2),
                    _mm512_ror_epi32::<19>(w2),
                    _mm512_srli_epi32::<10>(w2),
                );
                w[T % 16] = add(add(w[T % 16], sigma0), add(w[(T + 9) % 16], sigma1));
            }
            round(&mut s[g], add(w[T % 16], splat(K[T])));
        }
    }

    /// Round T of the constant padding block of a 64-byte message (W + K is a constant).
    #[inline]
    #[target_feature(enable = "avx512f")]
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
    #[target_feature(enable = "avx512f")]
    fn compress<const G: usize>(s: &mut [State; G], w: &mut [[V; 16]; G]) {
        let start = *s;
        rounds64!(data_round, G, s, w);
        for g in 0..G {
            for k in 0..8 {
                s[g][k] = add(s[g][k], start[g][k]);
            }
        }
    }

    #[inline]
    #[target_feature(enable = "avx512f")]
    fn iv() -> State {
        IV.map(|x| splat(x))
    }

    /// SHA-256 of the 64-byte messages `left || right`.
    #[inline]
    #[target_feature(enable = "avx512f")]
    fn sha64<const G: usize>(left: &[State; G], right: &[State; G]) -> [State; G] {
        let mut s = [iv(); G];
        let mut w: [[V; 16]; G] = core::array::from_fn(|g| {
            core::array::from_fn(|k| if k < 8 { left[g][k] } else { right[g][k - 8] })
        });
        compress(&mut s, &mut w);
        let mid = s;
        rounds64!(pad_round, G, &mut s);
        for g in 0..G {
            for k in 0..8 {
                s[g][k] = add(s[g][k], mid[g][k]);
            }
        }
        s
    }

    /// SHA-256 of the 32-byte messages `msg` (one block; words 8..15 are constant padding).
    #[inline]
    #[target_feature(enable = "avx512f")]
    fn sha32<const G: usize>(msg: &[State; G]) -> [State; G] {
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
    #[target_feature(enable = "avx512f")]
    fn seeds(ids: &[ChunkId]) -> State {
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
        // SAFETY: each `words[k]` is 16 readable u32s; unaligned load.
        let vec = |k: usize| unsafe { _mm512_loadu_si512(words[k].as_ptr().cast()) };
        let mut s = [iv()];
        compress(&mut s, &mut [core::array::from_fn(vec)]);
        compress(&mut s, &mut [core::array::from_fn(|k| vec(16 + k))]);
        s[0]
    }

    /// Variant A layout (word planes): reads segment i.
    ///
    /// # Safety
    /// `base` must point to the start of a group's SEGMENTS segments; `i < SEGMENTS`.
    #[inline]
    #[target_feature(enable = "avx512f")]
    unsafe fn load_planes(base: *const u32, i: usize) -> State {
        // SAFETY: segment i lies within the group (caller contract); each plane is 64 bytes at
        // a 64-byte aligned offset of the `align(64)` `Seg` array.
        core::array::from_fn(|w| unsafe {
            _mm512_load_si512(base.add(i * SEG_WORDS + w * LANES).cast())
        })
    }

    /// Variant A layout (word planes): writes segment i.
    ///
    /// # Safety
    /// As for `load_planes`, and the memory must be writable and not otherwise borrowed.
    #[inline]
    #[target_feature(enable = "avx512f")]
    unsafe fn store_planes(base: *mut u32, i: usize, s: &State) {
        for (w, &v) in s.iter().enumerate() {
            // SAFETY: as in `load_planes`.
            unsafe { _mm512_store_si512(base.add(i * SEG_WORDS + w * LANES).cast(), v) };
        }
    }

    /// `vpermt2d` index tables for the word-plane -> lane-row transpose in `store_rows`.
    /// Bit 4 of an index selects the second source, bits 0..3 the element.
    const fn transpose_idx(stage: usize, sel: usize) -> [i32; 16] {
        let mut idx = [0_i32; 16];
        let mut i = 0;
        while i < 16 {
            let v = match stage {
                // interleave word pairs: lanes 8 sel..8 sel + 8, 2 words each
                1 => (i % 2) * 16 + 8 * sel + i / 2,
                // pairs -> quads: lanes 4 sel..4 sel + 4 of the 8 in each source
                2 => (i % 4 / 2) * 16 + 2 * (4 * sel + i / 4) + i % 2,
                // quads -> rows of 8 words: lanes 2 sel, 2 sel + 1 of the 4 in each source
                _ => (i % 8 / 4) * 16 + 4 * (2 * sel + i / 8) + i % 4,
            };
            idx[i] = v as i32;
            i += 1;
        }
        idx
    }

    static TRANSPOSE_IDX: [[i32; 16]; 6] = [
        transpose_idx(1, 0),
        transpose_idx(1, 1),
        transpose_idx(2, 0),
        transpose_idx(2, 1),
        transpose_idx(3, 0),
        transpose_idx(3, 1),
    ];

    /// Variant B layout (lane rows): writes segment i, transposing the 8 word planes into 16
    /// rows of 8 words (two rows per vector) in three rounds of 8 two-source permutes.
    ///
    /// # Safety
    /// As for `store_planes`.
    #[inline]
    #[target_feature(enable = "avx512f")]
    unsafe fn store_rows(base: *mut u32, i: usize, p: &State) {
        // SAFETY: TRANSPOSE_IDX rows are 16 readable i32s; unaligned loads.
        let idx = |k: usize| unsafe { _mm512_loadu_si512(TRANSPOSE_IDX[k].as_ptr().cast()) };
        let perm = |a, k, b| _mm512_permutex2var_epi32(a, idx(k), b);
        // q[h][a]: words 2a, 2a + 1 of lanes 8h..8h + 8
        let q: [[V; 4]; 2] =
            core::array::from_fn(|h| core::array::from_fn(|a| perm(p[2 * a], h, p[2 * a + 1])));
        for (h, q) in q.iter().enumerate() {
            for qq in 0..2 {
                // words 0..4 and 4..8 of lanes 8h + 4qq..8h + 4qq + 4
                let lo = perm(q[0], 2 + qq, q[1]);
                let hi = perm(q[2], 2 + qq, q[3]);
                for r in 0..2 {
                    // all 8 words of lanes 8h + 4qq + 2r and the next one
                    let row = perm(lo, 4 + r, hi);
                    let lane = 8 * h + 4 * qq + 2 * r;
                    // SAFETY: segment i lies within the group (caller contract); rows of lane
                    // pairs are 64-byte aligned within the `align(64)` `Seg`.
                    unsafe { _mm512_store_si512(base.add(i * SEG_WORDS + lane * 8).cast(), row) };
                }
            }
        }
    }

    /// Variant B's read (lane rows): lane l loads segment j_l of its own chunk, where j_l is
    /// the LE u32 of the first 4 bytes of lane l's previous output, & 8191. Those bytes are H0
    /// big-endian, so j = bswap(H0) & 8191 = (H0 >> 24) | ((H0 >> 8) & 0x1f00).
    ///
    /// # Safety
    /// `base` must point to the start of a group's SEGMENTS segments.
    #[inline]
    #[target_feature(enable = "avx512f")]
    unsafe fn gather_rows(base: *const u32, prev: &State) -> State {
        let h0 = prev[0];
        let j = _mm512_or_si512(
            _mm512_srli_epi32::<24>(h0),
            _mm512_and_si512(_mm512_srli_epi32::<8>(h0), splat(0x1f00)),
        );
        let lane8 = _mm512_set_epi32(
            120, 112, 104, 96, 88, 80, 72, 64, 56, 48, 40, 32, 24, 16, 8, 0,
        );
        // u32 index of (segment j, lane l, word 0); word w adds w via the base pointer
        let idx = add(_mm512_slli_epi32::<7>(j), lane8);
        core::array::from_fn(|w| {
            // SAFETY: j < SEGMENTS, l < 16 and w < 8, so every index stays within the group's
            // SEGMENTS * 128 u32s (caller contract).
            unsafe { _mm512_i32gather_epi32::<4>(idx, base.add(w).cast()) }
        })
    }

    /// # Safety
    /// The CPU must support AVX-512F. `segs` must hold exactly G * SEGMENTS segments and
    /// `ids` 16 * G ids.
    #[target_feature(enable = "avx512f")]
    pub(super) unsafe fn run<const G: usize>(
        variant: Variant,
        ids: &[ChunkId],
        iterations: u32,
        segs: &mut [Seg],
    ) {
        assert_eq!(segs.len(), G * SEGMENTS);
        assert_eq!(ids.len(), G * LANES);
        // SAFETY: forwarded caller contract.
        unsafe {
            match variant {
                Variant::A => run_variant::<G, false>(ids, iterations, segs),
                Variant::B => run_variant::<G, true>(ids, iterations, segs),
            }
        }
    }

    /// `B == false`: variant A with the word-plane layout; `B == true`: variant B with the
    /// lane-row layout.
    ///
    /// # Safety
    /// As for `run`.
    #[target_feature(enable = "avx512f")]
    unsafe fn run_variant<const G: usize, const B: bool>(
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
        let store = |g: usize, i: usize, p: &State| {
            // SAFETY: i < SEGMENTS (callers below) and group(g) is this group's exclusively
            // borrowed memory.
            unsafe {
                if B {
                    store_rows(group(g), i, p)
                } else {
                    store_planes(group(g), i, p)
                }
            }
        };

        // Phase 1: prev = seed; buf[i] = prev = SHA(prev)
        let mut prev: [State; G] = core::array::from_fn(|g| seeds(&ids[g * LANES..][..LANES]));
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
            let right: [State; G] = core::array::from_fn(|g| {
                // SAFETY: reads this group's memory at segments < SEGMENTS.
                unsafe {
                    if B {
                        gather_rows(group(g), &prev[g])
                    } else {
                        load_planes(group(g), i)
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
