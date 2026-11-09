// Optimised kernels for variants A and B ("fast" implementation).
//
// Differences from the production-equivalent kernel in pvb.cu:
//  * SHA-256 runs on 32-bit words held in registers, fully unrolled, specialised for the
//    three fixed input sizes (68-byte seed, 32-byte Phase 1, 64-byte Phase 2). The second
//    compression of a 64-byte message only processes the constant padding block, so its
//    message schedule is folded into precomputed K+W constants.
//  * The chunk is stored in device memory as SHA-256 state words (big-endian digest words
//    held as native uint32), so no byte shuffling happens per step. The host converts to
//    digest byte order when it copies a chunk back.
//  * Segments are moved with 16-byte vector loads/stores.
//  * Variant A loads the next segment before hashing the current one (its read address is
//    known in advance). Variant B cannot: j comes out of the previous hash.
//  * LAYOUT 2 is a no-memory diagnostic (see kernel); it does not produce entropy.
//  * Two layouts: CONTIG (chunk c owns a contiguous 256 KiB) and INTERLEAVED (segment s of
//    chunk c lives at (s * chunks + c) * 32, so a warp's same-index segments are adjacent).
#pragma once
#include <stdint.h>

// Expects `d_chunk_id_prefix` (address || partition hash || chain id, 60 bytes) to be declared.

#define FAST_SEGMENTS 8192
#define FAST_SEG_WORDS 8

namespace fast {

// FAST_FMA_ADDS: route additions through IMAD (x * one + y) instead of IADD3. The SHA rounds are
// otherwise limited by the integer ALU pipe (SHF/LOP3/IADD3) while the FMA pipe that runs IMAD
// sits idle. `one` is a kernel argument equal to 1, so the compiler cannot fold the multiply.
#ifndef FAST_FMA_ADDS
#define FAST_FMA_ADDS 0
#endif

__device__ __forceinline__ uint32_t fadd(uint32_t x, uint32_t y, uint32_t one) {
#if FAST_FMA_ADDS
    uint32_t r;
    asm("mad.lo.u32 %0, %1, %2, %3;" : "=r"(r) : "r"(x), "r"(one), "r"(y));
    return r;
#else
    (void)one;
    return x + y;
#endif
}

__device__ __forceinline__ uint32_t rotr(uint32_t x, int n) { return __funnelshift_r(x, x, n); }
__device__ __forceinline__ uint32_t bsig0(uint32_t a) { return rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22); }
__device__ __forceinline__ uint32_t bsig1(uint32_t e) { return rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25); }
__device__ __forceinline__ uint32_t ssig0(uint32_t x) { return rotr(x, 7) ^ rotr(x, 18) ^ (x >> 3); }
__device__ __forceinline__ uint32_t ssig1(uint32_t x) { return rotr(x, 17) ^ rotr(x, 19) ^ (x >> 10); }
__device__ __forceinline__ uint32_t ch(uint32_t e, uint32_t f, uint32_t g) { return (e & f) ^ (~e & g); }
__device__ __forceinline__ uint32_t maj(uint32_t a, uint32_t b, uint32_t c) { return (a & b) | (c & (a | b)); }

struct Table {
    uint32_t v[64];
};

// Host copy for compile-time table construction; device code uses K.
constexpr Table K_HOST = {{
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
}};

__device__ constexpr Table K = K_HOST;

__device__ constexpr uint32_t IV[8] = {0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
                            0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19};

constexpr uint32_t c_rotr(uint32_t x, int n) { return (x >> n) | (x << (32 - n)); }

// K[t] + W[t] for a block whose words are fixed at compile time.
constexpr Table make_kw(const uint32_t (&w16)[16]) {
    Table out{};
    uint32_t w[64] = {};
    for (int t = 0; t < 16; ++t) w[t] = w16[t];
    for (int t = 16; t < 64; ++t) {
        uint32_t s0 = c_rotr(w[t - 15], 7) ^ c_rotr(w[t - 15], 18) ^ (w[t - 15] >> 3);
        uint32_t s1 = c_rotr(w[t - 2], 17) ^ c_rotr(w[t - 2], 19) ^ (w[t - 2] >> 10);
        w[t] = w[t - 16] + s0 + w[t - 7] + s1;
    }
    for (int t = 0; t < 64; ++t) out.v[t] = K_HOST.v[t] + w[t];
    return out;
}

// Padding block of a 64-byte message: 0x80, zeros, bit length 512.
constexpr uint32_t PAD64_W[16] = {0x80000000, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 512};
__device__ constexpr Table KW_PAD64 = make_kw(PAD64_W);

#define FAST_ROUND(a, b, c, d, e, f, g, h, kw)                                       \
    do {                                                                             \
        uint32_t t1 = fadd(fadd(fadd(h, (kw), one), bsig1(e), one), ch(e, f, g), one); \
        uint32_t t2 = fadd(bsig0(a), maj(a, b, c), one);                             \
        d = fadd(d, t1, one);                                                        \
        h = fadd(t1, t2, one);                                                       \
    } while (0)

// One compression of a variable message block `w` (overwritten as the schedule).
__device__ __forceinline__ void compress(uint32_t st[8], uint32_t w[16], uint32_t one) {
    uint32_t a = st[0], b = st[1], c = st[2], d = st[3], e = st[4], f = st[5], g = st[6], h = st[7];
#pragma unroll
    for (int t = 0; t < 64; t += 8) {
        if (t >= 16) {
#pragma unroll
            for (int k = 0; k < 8; ++k) {
                int i = t + k;
                w[i & 15] = fadd(fadd(w[i & 15], ssig0(w[(i + 1) & 15]), one),
                                 fadd(w[(i + 9) & 15], ssig1(w[(i + 14) & 15]), one), one);
            }
        }
        FAST_ROUND(a, b, c, d, e, f, g, h, fadd(w[(t + 0) & 15], K.v[t + 0], one));
        FAST_ROUND(h, a, b, c, d, e, f, g, fadd(w[(t + 1) & 15], K.v[t + 1], one));
        FAST_ROUND(g, h, a, b, c, d, e, f, fadd(w[(t + 2) & 15], K.v[t + 2], one));
        FAST_ROUND(f, g, h, a, b, c, d, e, fadd(w[(t + 3) & 15], K.v[t + 3], one));
        FAST_ROUND(e, f, g, h, a, b, c, d, fadd(w[(t + 4) & 15], K.v[t + 4], one));
        FAST_ROUND(d, e, f, g, h, a, b, c, fadd(w[(t + 5) & 15], K.v[t + 5], one));
        FAST_ROUND(c, d, e, f, g, h, a, b, fadd(w[(t + 6) & 15], K.v[t + 6], one));
        FAST_ROUND(b, c, d, e, f, g, h, a, fadd(w[(t + 7) & 15], K.v[t + 7], one));
    }
    st[0] += a; st[1] += b; st[2] += c; st[3] += d; st[4] += e; st[5] += f; st[6] += g; st[7] += h;
}

// One compression of a block whose K+W values are compile-time constants.
__device__ __forceinline__ void compress_const(uint32_t st[8], const Table &kw, uint32_t one) {
    uint32_t a = st[0], b = st[1], c = st[2], d = st[3], e = st[4], f = st[5], g = st[6], h = st[7];
#pragma unroll
    for (int t = 0; t < 64; t += 8) {
        FAST_ROUND(a, b, c, d, e, f, g, h, kw.v[t + 0]);
        FAST_ROUND(h, a, b, c, d, e, f, g, kw.v[t + 1]);
        FAST_ROUND(g, h, a, b, c, d, e, f, kw.v[t + 2]);
        FAST_ROUND(f, g, h, a, b, c, d, e, kw.v[t + 3]);
        FAST_ROUND(e, f, g, h, a, b, c, d, kw.v[t + 4]);
        FAST_ROUND(d, e, f, g, h, a, b, c, kw.v[t + 5]);
        FAST_ROUND(c, d, e, f, g, h, a, b, kw.v[t + 6]);
        FAST_ROUND(b, c, d, e, f, g, h, a, kw.v[t + 7]);
    }
    st[0] += a; st[1] += b; st[2] += c; st[3] += d; st[4] += e; st[5] += f; st[6] += g; st[7] += h;
}

// SHA-256 of a 32-byte message given as 8 digest words.
__device__ __forceinline__ void sha256_32(const uint32_t in[8], uint32_t out[8], uint32_t one) {
    uint32_t w[16];
#pragma unroll
    for (int k = 0; k < 8; ++k) w[k] = in[k];
    w[8] = 0x80000000;
#pragma unroll
    for (int k = 9; k < 15; ++k) w[k] = 0;
    w[15] = 256;
#pragma unroll
    for (int k = 0; k < 8; ++k) out[k] = IV[k];
    compress(out, w, one);
}

// SHA-256 of the 64-byte message left || right (each 8 digest words).
__device__ __forceinline__ void sha256_64(const uint32_t left[8], const uint32_t right[8], uint32_t out[8],
                                          uint32_t one) {
    uint32_t w[16];
#pragma unroll
    for (int k = 0; k < 8; ++k) {
        w[k] = left[k];
        w[k + 8] = right[k];
    }
#pragma unroll
    for (int k = 0; k < 8; ++k) out[k] = IV[k];
    compress(out, w, one);
    compress_const(out, KW_PAD64, one);
}

__device__ __forceinline__ uint32_t be32(const unsigned char *p) {
    return ((uint32_t)p[0] << 24) | ((uint32_t)p[1] << 16) | ((uint32_t)p[2] << 8) | (uint32_t)p[3];
}

// seed = SHA(prefix(60) || offset u64 LE): 68 bytes -> two blocks.
__device__ void seed_hash(const unsigned char *prefix60, uint64_t offset, uint32_t out[8], uint32_t one) {
    unsigned char msg[128];
#pragma unroll
    for (int k = 0; k < 60; ++k) msg[k] = prefix60[k];
#pragma unroll
    for (int k = 0; k < 8; ++k) msg[60 + k] = (unsigned char)(offset >> (8 * k));
    msg[68] = 0x80;
#pragma unroll
    for (int k = 69; k < 120; ++k) msg[k] = 0;
    const uint64_t bits = 68 * 8;
#pragma unroll
    for (int k = 0; k < 8; ++k) msg[120 + k] = (unsigned char)(bits >> (56 - 8 * k));
#pragma unroll
    for (int k = 0; k < 8; ++k) out[k] = IV[k];
    uint32_t w[16];
#pragma unroll
    for (int blk = 0; blk < 2; ++blk) {
#pragma unroll
        for (int k = 0; k < 16; ++k) w[k] = be32(msg + blk * 64 + 4 * k);
        compress(out, w, one);
    }
}

// ---- memory layout --------------------------------------------------------------------

template <int LAYOUT>
__device__ __forceinline__ uint4 *seg_ptr(uint32_t *base, uint64_t chunks, uint64_t c, uint32_t s) {
    size_t seg = LAYOUT == 0 ? (size_t)c * FAST_SEGMENTS + s : (size_t)s * chunks + c;
    return reinterpret_cast<uint4 *>(base + seg * FAST_SEG_WORDS);
}

// FAST_CACHE_HINT: 0 = default caching, 1 = streaming (evict-first: __ldcs/__stcs),
// 2 = L2 only (__ldcg/__stcg). Segments are not reused before L2 would evict them anyway.
#ifndef FAST_CACHE_HINT
#define FAST_CACHE_HINT 1
#endif

__device__ __forceinline__ uint4 ld16(const uint4 *p) {
#if FAST_CACHE_HINT == 1
    return __ldcs(p);
#elif FAST_CACHE_HINT == 2
    return __ldcg(p);
#else
    return *p;
#endif
}

__device__ __forceinline__ void st16(uint4 *p, uint4 v) {
#if FAST_CACHE_HINT == 1
    __stcs(p, v);
#elif FAST_CACHE_HINT == 2
    __stcg(p, v);
#else
    *p = v;
#endif
}

__device__ __forceinline__ void load_seg(const uint4 *p, uint32_t v[8]) {
    uint4 lo = ld16(p), hi = ld16(p + 1);
    v[0] = lo.x; v[1] = lo.y; v[2] = lo.z; v[3] = lo.w;
    v[4] = hi.x; v[5] = hi.y; v[6] = hi.z; v[7] = hi.w;
}

__device__ __forceinline__ void store_seg(uint4 *p, const uint32_t v[8]) {
    st16(p, make_uint4(v[0], v[1], v[2], v[3]));
    st16(p + 1, make_uint4(v[4], v[5], v[6], v[7]));
}

// j = u32::from_le_bytes(prev_bytes[0..4]) & 8191. Digest byte 0 is the top byte of word 0,
// so the little-endian u32 of the first four bytes is the byte-swapped word 0.
__device__ __forceinline__ uint32_t read_index(uint32_t w0) {
    return __byte_perm(w0, 0, 0x0123) & (FAST_SEGMENTS - 1);
}

// One launch covers Phase 2 hash counts [h0, h1). The whole chain state lives in the buffer
// (A's left input and B's prev are both buf[(h - 1) mod 8192]), so a later launch resumes exactly
// where the previous one stopped. Splitting a long run into short launches keeps each kernel
// brief, which matters under WDDM (Windows / WSL2), where the GPU is pre-empted for the desktop.
// `phase1` runs the seed and Phase 1 first; it is set only on the first launch, with h0 == 8192.
template <int VARIANT, int LAYOUT>
__global__ void kernel(uint64_t offset_start, uint64_t chunks, uint32_t *__restrict__ base, unsigned int h0,
                       unsigned int h1, int phase1, uint32_t one) {
    size_t stride = (size_t)gridDim.x * blockDim.x;
    for (size_t c = (size_t)blockIdx.x * blockDim.x + threadIdx.x; c < chunks; c += stride) {
        uint32_t prev[8];
        if (phase1) {
            seed_hash(d_chunk_id_prefix, offset_start + c, prev, one);
            for (uint32_t s = 0; s < FAST_SEGMENTS; ++s) {
                uint32_t out[8];
                sha256_32(prev, out, one);
                store_seg(seg_ptr<LAYOUT>(base, chunks, c, s), out);
#pragma unroll
                for (int k = 0; k < 8; ++k) prev[k] = out[k];
            }
        } else if (LAYOUT == 2) {
            load_seg(seg_ptr<LAYOUT>(base, chunks, c, 0), prev);
        } else {
            load_seg(seg_ptr<LAYOUT>(base, chunks, c, (h0 - 1) & (FAST_SEGMENTS - 1)), prev);
        }
        if (h0 >= h1) continue;

        // Phase 2; prev == buf[(h0 - 1) mod 8192]
        if (LAYOUT == 2) {
            // Diagnostic only, output is NOT the entropy chunk: the same Phase 2 hashing with no
            // loads or stores, to measure the compute ceiling. The result is stored so the work
            // is not optimised away.
            uint32_t cur[8];
#pragma unroll
            for (int k = 0; k < 8; ++k) cur[k] = prev[k];
            for (unsigned int hc = h0; hc < h1; ++hc) {
                uint32_t out[8];
                sha256_64(prev, cur, out, one);
                if (VARIANT == 1) out[7] ^= read_index(out[0]);
#pragma unroll
                for (int k = 0; k < 8; ++k) {
                    cur[k] = prev[k];
                    prev[k] = out[k];
                }
            }
            store_seg(seg_ptr<LAYOUT>(base, chunks, c, 0), prev);
        } else if (VARIANT == 0) {
            uint32_t cur[8];
            load_seg(seg_ptr<LAYOUT>(base, chunks, c, h0 & (FAST_SEGMENTS - 1)), cur);
            for (unsigned int hc = h0; hc < h1; ++hc) {
                uint32_t i = hc & (FAST_SEGMENTS - 1);
                // buf[i + 1] is not written by this step, so it can be fetched before hashing.
                uint32_t next[8];
                load_seg(seg_ptr<LAYOUT>(base, chunks, c, (i + 1) & (FAST_SEGMENTS - 1)), next);
                uint32_t out[8];
                sha256_64(prev, cur, out, one);
                store_seg(seg_ptr<LAYOUT>(base, chunks, c, i), out);
#pragma unroll
                for (int k = 0; k < 8; ++k) {
                    prev[k] = out[k];
                    cur[k] = next[k];
                }
            }
        } else {
            for (unsigned int hc = h0; hc < h1; ++hc) {
                uint32_t i = hc & (FAST_SEGMENTS - 1);
                uint32_t rd[8];
                load_seg(seg_ptr<LAYOUT>(base, chunks, c, read_index(prev[0])), rd);
                uint32_t out[8];
                sha256_64(prev, rd, out, one);
                store_seg(seg_ptr<LAYOUT>(base, chunks, c, i), out);
#pragma unroll
                for (int k = 0; k < 8; ++k) prev[k] = out[k];
            }
        }
    }
}

} // namespace fast
