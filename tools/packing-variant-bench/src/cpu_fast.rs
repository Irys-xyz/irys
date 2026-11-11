//! Optimised CPU implementation of both variants using x86_64 SHA-NI.
//!
//! Every chunk is a serial hash chain, so one chain is bound by `sha256rnds2` latency. To keep
//! the SHA unit busy, `entropy_multi::<N>` runs N independent chunks in lockstep: each step of
//! all N chains is issued in one loop body, so the out-of-order core overlaps their rounds.
//!
//! While running, each 32-byte segment is held as eight native-endian `u32` words (the SHA-256
//! state words H0..H7) instead of big-endian bytes: a hash output can then be fed straight back
//! in as message words without byte swapping. One byte swap pass at the end converts every chunk
//! to the exact reference byte order.
//!
//! Output is byte-identical to [`crate::entropy_a`] / [`crate::entropy_b`].

use crate::{Chunk, ChunkId, Variant};

/// True when this CPU supports the SHA-NI fast path. Without it, the functions below fall back to
/// the (much slower) OpenSSL reference implementation.
pub fn available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        is_x86_feature_detected!("sha")
            && is_x86_feature_detected!("sse4.1")
            && is_x86_feature_detected!("ssse3")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// Computes N chunks, `bufs[n]` for `ids[n]`, interleaving the N hash chains on this thread.
pub fn entropy_multi<const N: usize>(
    variant: Variant,
    ids: &[ChunkId; N],
    iterations: u32,
    bufs: &mut [Box<Chunk>; N],
) {
    #[cfg(target_arch = "x86_64")]
    if available() {
        let mut ptrs = bufs.each_mut().map(|b| b.as_mut_ptr());
        // SAFETY: SHA-NI/SSE4.1/SSSE3 were detected above; the pointers come from N distinct
        // `&mut Box<Chunk>` so each refers to its own CHUNK_SIZE bytes, exclusively borrowed for
        // the duration of the call.
        unsafe { ni::entropy_multi::<N>(variant, ids, iterations, &mut ptrs) };
        return;
    }
    for (id, buf) in ids.iter().zip(bufs.iter_mut()) {
        crate::entropy(variant, id, iterations, buf);
    }
}

/// Single-chunk convenience wrapper (N = 1).
pub fn entropy(variant: Variant, id: &ChunkId, iterations: u32, buf: &mut Chunk) {
    #[cfg(target_arch = "x86_64")]
    if available() {
        let mut ptrs = [buf.as_mut_ptr()];
        // SAFETY: as in `entropy_multi`, with a single exclusively borrowed chunk.
        unsafe { ni::entropy_multi::<1>(variant, &[*id], iterations, &mut ptrs) };
        return;
    }
    crate::entropy(variant, id, iterations, buf);
}

pub(crate) const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

pub(crate) const IV: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

#[repr(align(16))]
pub(crate) struct Aligned64(pub(crate) [u32; 64]);

static K_ALIGNED: Aligned64 = Aligned64(K);

/// The second block of a 64-byte message is pure padding (0x80, zeros, bit length 512), so its
/// whole message schedule plus K is a compile-time constant and costs no msg1/msg2 work.
pub(crate) static PAD64_WK: Aligned64 = Aligned64(padding_block_wk(512));

const fn padding_block_wk(len_bits: u32) -> [u32; 64] {
    let mut w = [0_u32; 64];
    w[0] = 0x8000_0000;
    w[15] = len_bits;
    let mut t = 16;
    while t < 64 {
        let s0 = w[t - 15].rotate_right(7) ^ w[t - 15].rotate_right(18) ^ (w[t - 15] >> 3);
        let s1 = w[t - 2].rotate_right(17) ^ w[t - 2].rotate_right(19) ^ (w[t - 2] >> 10);
        w[t] = w[t - 16]
            .wrapping_add(s0)
            .wrapping_add(w[t - 7])
            .wrapping_add(s1);
        t += 1;
    }
    let mut t = 0;
    while t < 64 {
        w[t] = w[t].wrapping_add(K[t]);
        t += 1;
    }
    w
}

#[cfg(target_arch = "x86_64")]
mod ni {
    use super::{IV, K_ALIGNED, PAD64_WK};
    use crate::{ChunkId, SEGMENT_SIZE, SEGMENTS, Variant};
    use core::arch::x86_64::*;

    /// SHA-256 state in the register layout `sha256rnds2` operates on.
    #[derive(Clone, Copy)]
    struct State {
        abef: __m128i,
        cdgh: __m128i,
    }

    /// A 32-byte value as message words: lanes (W0..W3), (W4..W7) == state words (a..d), (e..h).
    type Words = [__m128i; 2];

    #[inline]
    #[target_feature(enable = "sha,sse4.1,ssse3")]
    fn words_to_state(w: Words) -> State {
        let dcba = _mm_shuffle_epi32::<0xB1>(w[0]);
        let hgfe = _mm_shuffle_epi32::<0x1B>(w[1]);
        State {
            abef: _mm_alignr_epi8::<8>(dcba, hgfe),
            cdgh: _mm_blend_epi16::<0xF0>(hgfe, dcba),
        }
    }

    #[inline]
    #[target_feature(enable = "sha,sse4.1,ssse3")]
    fn state_to_words(s: State) -> Words {
        let feba = _mm_shuffle_epi32::<0x1B>(s.abef);
        let dchg = _mm_shuffle_epi32::<0xB1>(s.cdgh);
        [
            _mm_blend_epi16::<0xF0>(feba, dchg),
            _mm_alignr_epi8::<8>(dchg, feba),
        ]
    }

    #[inline]
    #[target_feature(enable = "sha,sse4.1,ssse3")]
    fn add_state(s: State, t: State) -> State {
        State {
            abef: _mm_add_epi32(s.abef, t.abef),
            cdgh: _mm_add_epi32(s.cdgh, t.cdgh),
        }
    }

    #[inline]
    #[target_feature(enable = "sha,sse4.1,ssse3")]
    fn load_wk(table: &super::Aligned64, quad: usize) -> __m128i {
        let q: &[u32; 4] = table.0[quad * 4..quad * 4 + 4].try_into().unwrap();
        // SAFETY: `q` is 16 readable bytes, 16-byte aligned because the table is
        // `repr(align(16))` and `quad * 16` is a multiple of 16.
        unsafe { _mm_load_si128(q.as_ptr().cast()) }
    }

    /// Four rounds using the precomputed (W + K) quad `wk`.
    #[inline]
    #[target_feature(enable = "sha,sse4.1,ssse3")]
    fn quad_round(s: &mut State, wk: __m128i) {
        s.cdgh = _mm_sha256rnds2_epu32(s.cdgh, s.abef, wk);
        s.abef = _mm_sha256rnds2_epu32(s.abef, s.cdgh, _mm_shuffle_epi32::<0x0E>(wk));
    }

    /// Compresses one data block per chain (message words `m: [[__m128i; 4]; N]`, lanes
    /// W0..W15) into `st: [State; N]`. The N chains are issued round-quad by round-quad so
    /// their latencies overlap.
    ///
    /// A macro rather than a function: `#[inline(always)]` is not allowed with
    /// `#[target_feature]`, and as a plain function LLVM outlines it for N >= 2 (it has several
    /// callers), passing the state through memory on every hash.
    ///
    /// The quads are spelled out with a const index: a runtime `for` loop is only partially
    /// unrolled by LLVM, which then indexes `m` dynamically and keeps the message schedule on
    /// the stack (a store/reload on every quad).
    macro_rules! compress {
        ($n:tt, $st:expr, $m:expr) => {{
            let st: &mut [State; $n] = $st;
            let m: &mut [[__m128i; 4]; $n] = $m;
            let start = *st;
            sched_quad::<$n, 0>(st, m);
            sched_quad::<$n, 1>(st, m);
            sched_quad::<$n, 2>(st, m);
            sched_quad::<$n, 3>(st, m);
            sched_quad::<$n, 4>(st, m);
            sched_quad::<$n, 5>(st, m);
            sched_quad::<$n, 6>(st, m);
            sched_quad::<$n, 7>(st, m);
            sched_quad::<$n, 8>(st, m);
            sched_quad::<$n, 9>(st, m);
            sched_quad::<$n, 10>(st, m);
            sched_quad::<$n, 11>(st, m);
            sched_quad::<$n, 12>(st, m);
            sched_quad::<$n, 13>(st, m);
            sched_quad::<$n, 14>(st, m);
            sched_quad::<$n, 15>(st, m);
            for n in 0..$n {
                st[n] = add_state(st[n], start[n]);
            }
        }};
    }

    /// Rounds 4Q..4Q+4 of all N chains, extending the message schedule first when Q >= 4.
    #[inline]
    #[target_feature(enable = "sha,sse4.1,ssse3")]
    fn sched_quad<const N: usize, const Q: usize>(st: &mut [State; N], m: &mut [[__m128i; 4]; N]) {
        let k = load_wk(&K_ALIGNED, Q);
        for n in 0..N {
            let m = &mut m[n];
            if Q >= 4 {
                // W[t..t+4] from W[t-16..], W[t-12], W[t-7..t-4] and W[t-4..]
                let w_7 = _mm_alignr_epi8::<4>(m[(Q + 3) % 4], m[(Q + 2) % 4]);
                let x = _mm_add_epi32(_mm_sha256msg1_epu32(m[Q % 4], m[(Q + 1) % 4]), w_7);
                m[Q % 4] = _mm_sha256msg2_epu32(x, m[(Q + 3) % 4]);
            }
            quad_round(&mut st[n], _mm_add_epi32(m[Q % 4], k));
        }
    }

    /// Compresses the constant padding block of a 64-byte message.
    #[inline]
    #[target_feature(enable = "sha,sse4.1,ssse3")]
    fn compress_pad64<const N: usize>(st: &mut [State; N]) {
        let start = *st;
        for quad in 0..16 {
            let wk = load_wk(&PAD64_WK, quad);
            for s in st.iter_mut() {
                quad_round(s, wk);
            }
        }
        for n in 0..N {
            st[n] = add_state(st[n], start[n]);
        }
    }

    #[inline]
    #[target_feature(enable = "sha,sse4.1,ssse3")]
    fn iv_state() -> State {
        // SAFETY: IV is 8 readable u32s; unaligned loads.
        let w = unsafe {
            [
                _mm_loadu_si128(IV.as_ptr().cast()),
                _mm_loadu_si128(IV.as_ptr().add(4).cast()),
            ]
        };
        words_to_state(w)
    }

    /// SHA-256 of a 64-byte message `left || right`, N chains at once.
    #[inline]
    #[target_feature(enable = "sha,sse4.1,ssse3")]
    fn sha64<const N: usize>(iv: State, left: &[Words; N], right: &[Words; N]) -> [Words; N] {
        let mut st = [iv; N];
        let mut m: [[__m128i; 4]; N] =
            core::array::from_fn(|n| [left[n][0], left[n][1], right[n][0], right[n][1]]);
        compress!(N, &mut st, &mut m);
        compress_pad64(&mut st);
        st.map(|s| state_to_words(s))
    }

    /// SHA-256 of a 32-byte message, N chains at once (one block, constant padding words).
    #[inline]
    #[target_feature(enable = "sha,sse4.1,ssse3")]
    fn sha32<const N: usize>(iv: State, msg: &[Words; N]) -> [Words; N] {
        let pad_lo = _mm_set_epi32(0, 0, 0, 0x8000_0000_u32 as i32);
        let pad_hi = _mm_set_epi32(256, 0, 0, 0);
        let mut st = [iv; N];
        let mut m: [[__m128i; 4]; N] =
            core::array::from_fn(|n| [msg[n][0], msg[n][1], pad_lo, pad_hi]);
        compress!(N, &mut st, &mut m);
        st.map(|s| state_to_words(s))
    }

    /// seed = SHA(address(20) || partition_hash(32) || chain_id LE || partition_offset LE):
    /// a 68-byte message, i.e. two blocks. Runs once per chunk, so it is not interleaved.
    #[target_feature(enable = "sha,sse4.1,ssse3")]
    fn seed_words(iv: State, id: &ChunkId) -> Words {
        let mut bytes = [0_u8; 128];
        bytes[..20].copy_from_slice(&id.address);
        bytes[20..52].copy_from_slice(&id.partition_hash);
        bytes[52..60].copy_from_slice(&id.chain_id.to_le_bytes());
        bytes[60..68].copy_from_slice(&id.partition_offset.to_le_bytes());
        bytes[68] = 0x80;
        bytes[120..128].copy_from_slice(&(68_u64 * 8).to_be_bytes());
        let quad = |q: usize| {
            let w = |k: usize| {
                let o = q * 16 + k * 4;
                u32::from_be_bytes(bytes[o..o + 4].try_into().unwrap()) as i32
            };
            _mm_set_epi32(w(3), w(2), w(1), w(0))
        };
        let mut st = [iv];
        compress!(1, &mut st, &mut [[quad(0), quad(1), quad(2), quad(3)]]);
        compress!(1, &mut st, &mut [[quad(4), quad(5), quad(6), quad(7)]]);
        state_to_words(st[0])
    }

    /// Reads segment `i` (in native-endian word form) of the chunk at `p`.
    ///
    /// # Safety
    /// `p` must point to CHUNK_SIZE readable bytes.
    #[inline]
    #[target_feature(enable = "sha,sse4.1,ssse3")]
    unsafe fn load_seg(p: *const u8, i: usize) -> Words {
        debug_assert!(i < SEGMENTS);
        // SAFETY: i < SEGMENTS, so bytes [32 i, 32 i + 32) lie within the chunk.
        unsafe {
            let q = p.add(i * SEGMENT_SIZE);
            [_mm_loadu_si128(q.cast()), _mm_loadu_si128(q.add(16).cast())]
        }
    }

    /// # Safety
    /// `p` must point to CHUNK_SIZE writable bytes.
    #[inline]
    #[target_feature(enable = "sha,sse4.1,ssse3")]
    unsafe fn store_seg(p: *mut u8, i: usize, w: Words) {
        debug_assert!(i < SEGMENTS);
        // SAFETY: i < SEGMENTS, so bytes [32 i, 32 i + 32) lie within the chunk.
        unsafe {
            let q = p.add(i * SEGMENT_SIZE);
            _mm_storeu_si128(q.cast(), w[0]);
            _mm_storeu_si128(q.add(16).cast(), w[1]);
        }
    }

    /// # Safety
    /// Each `bufs[n]` must point to CHUNK_SIZE writable bytes, not aliased by any other pointer
    /// in `bufs` nor accessed elsewhere during the call. The CPU must support SHA, SSE4.1, SSSE3.
    #[target_feature(enable = "sha,sse4.1,ssse3")]
    pub(super) unsafe fn entropy_multi<const N: usize>(
        variant: Variant,
        ids: &[ChunkId; N],
        iterations: u32,
        bufs: &mut [*mut u8; N],
    ) {
        let iv = iv_state();

        // Phase 1: prev = seed; buf[i] = prev = SHA(prev)
        let mut prev: [Words; N] = core::array::from_fn(|n| seed_words(iv, &ids[n]));
        for i in 0..SEGMENTS {
            prev = sha32(iv, &prev);
            for n in 0..N {
                // SAFETY: bufs[n] is a valid exclusive chunk (caller contract).
                unsafe { store_seg(bufs[n], i, prev[n]) };
            }
        }

        // Phase 2. In both variants `prev` (the left half of the message) is the previous
        // step's output, carried in registers. For A, buf[i - 1] (or buf[8191] at i == 0) is
        // exactly that output, so only buf[i] is read from memory.
        let mut hash_count = SEGMENTS;
        let iterations = iterations as usize;
        while hash_count < iterations {
            let i = hash_count % SEGMENTS;
            let right: [Words; N] = core::array::from_fn(|n| {
                let j = match variant {
                    Variant::A => i,
                    // j = LE u32 of the first 4 output bytes. Those bytes are H0 big-endian,
                    // so the LE read is H0 byte swapped.
                    Variant::B => {
                        (_mm_cvtsi128_si32(prev[n][0]) as u32).swap_bytes() as usize
                            & (SEGMENTS - 1)
                    }
                };
                // SAFETY: j < SEGMENTS; bufs[n] is a valid chunk (caller contract).
                unsafe { load_seg(bufs[n], j) }
            });
            prev = sha64(iv, &prev, &right);
            for n in 0..N {
                // SAFETY: i < SEGMENTS; bufs[n] is a valid exclusive chunk (caller contract).
                unsafe { store_seg(bufs[n], i, prev[n]) };
            }
            hash_count += 1;
        }

        // Convert native-endian state words to the reference big-endian byte order.
        let bswap = _mm_set_epi8(12, 13, 14, 15, 8, 9, 10, 11, 4, 5, 6, 7, 0, 1, 2, 3);
        for &p in bufs.iter() {
            for off in (0..crate::CHUNK_SIZE).step_by(16) {
                // SAFETY: off + 16 <= CHUNK_SIZE; p is a valid exclusive chunk.
                unsafe {
                    let q = p.add(off).cast::<__m128i>();
                    _mm_storeu_si128(q, _mm_shuffle_epi8(_mm_loadu_si128(q), bswap));
                }
            }
        }
    }
}
