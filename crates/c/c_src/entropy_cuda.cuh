// Device code for CUDA entropy packing (see capacity_cuda.cu for the host side).
//
// Design, measured on an RTX 5090 at 1M iterations (tools/packing-variant-bench has the numbers
// and the tooling):
//  * SHA-256 runs on 32-bit words in registers, fully unrolled and specialised for the three
//    message sizes used: the 68-byte seed, the 32-byte Phase 1 message and the 64-byte Phase 2
//    message. The second block of a 64-byte message is constant padding, so its message schedule
//    plus K is a compile-time table. ~2 300 integer instructions per Phase 2 step, against ~6 300
//    plus local-memory traffic for a byte-at-a-time SHA-256.
//  * The working buffer is segment-interleaved: segment s of chunk c lives at (s * chunks + c),
//    so the 32 threads of a warp touch adjacent segments. With one contiguous 256 KiB chunk per
//    thread, a warp's accesses are 256 KiB apart and pile onto the same DRAM channels.
//  * Segments hold SHA-256 state words (big-endian digest words as native uint32), so no byte
//    swapping happens per step. `chunks_to_bytes` converts to digest byte order and to one
//    contiguous chunk per 256 KiB at the end.
//  * Phase 2 runs in launches covering a range of hash counts. The whole chain state is in the
//    buffer (the left input is buf[(h - 1) mod 8192]), so a launch resumes exactly where the
//    previous one stopped; short launches keep the GPU responsive and avoid long preemption
//    windows under WDDM (Windows / WSL2), where long single launches were seen to fail silently.
#pragma once
#include <stdint.h>

#define ENTROPY_SEGMENTS 8192
#define ENTROPY_SEG_WORDS 8
#define ENTROPY_CHUNK_BYTES (ENTROPY_SEGMENTS * ENTROPY_SEG_WORDS * 4)

// mining address (20) || partition hash (32) || chain id u64 LE (8); the kernel appends the
// partition offset u64 LE. Passed by value so concurrent batches never share state.
struct EntropyChunkIdPrefix {
    unsigned char bytes[60];
};

namespace entropy {

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

#define ENTROPY_ROUND(a, b, c, d, e, f, g, h, kw)              \
    do {                                                       \
        uint32_t t1 = h + bsig1(e) + ch(e, f, g) + (kw);       \
        uint32_t t2 = bsig0(a) + maj(a, b, c);                 \
        d += t1;                                               \
        h = t1 + t2;                                           \
    } while (0)

// One compression of a variable message block `w` (overwritten as the schedule).
__device__ __forceinline__ void compress(uint32_t st[8], uint32_t w[16]) {
    uint32_t a = st[0], b = st[1], c = st[2], d = st[3], e = st[4], f = st[5], g = st[6], h = st[7];
#pragma unroll
    for (int t = 0; t < 64; t += 8) {
        if (t >= 16) {
#pragma unroll
            for (int k = 0; k < 8; ++k) {
                int i = t + k;
                w[i & 15] += ssig0(w[(i + 1) & 15]) + w[(i + 9) & 15] + ssig1(w[(i + 14) & 15]);
            }
        }
        ENTROPY_ROUND(a, b, c, d, e, f, g, h, K.v[t + 0] + w[(t + 0) & 15]);
        ENTROPY_ROUND(h, a, b, c, d, e, f, g, K.v[t + 1] + w[(t + 1) & 15]);
        ENTROPY_ROUND(g, h, a, b, c, d, e, f, K.v[t + 2] + w[(t + 2) & 15]);
        ENTROPY_ROUND(f, g, h, a, b, c, d, e, K.v[t + 3] + w[(t + 3) & 15]);
        ENTROPY_ROUND(e, f, g, h, a, b, c, d, K.v[t + 4] + w[(t + 4) & 15]);
        ENTROPY_ROUND(d, e, f, g, h, a, b, c, K.v[t + 5] + w[(t + 5) & 15]);
        ENTROPY_ROUND(c, d, e, f, g, h, a, b, K.v[t + 6] + w[(t + 6) & 15]);
        ENTROPY_ROUND(b, c, d, e, f, g, h, a, K.v[t + 7] + w[(t + 7) & 15]);
    }
    st[0] += a; st[1] += b; st[2] += c; st[3] += d; st[4] += e; st[5] += f; st[6] += g; st[7] += h;
}

// One compression of a block whose K+W values are compile-time constants.
__device__ __forceinline__ void compress_const(uint32_t st[8], const Table &kw) {
    uint32_t a = st[0], b = st[1], c = st[2], d = st[3], e = st[4], f = st[5], g = st[6], h = st[7];
#pragma unroll
    for (int t = 0; t < 64; t += 8) {
        ENTROPY_ROUND(a, b, c, d, e, f, g, h, kw.v[t + 0]);
        ENTROPY_ROUND(h, a, b, c, d, e, f, g, kw.v[t + 1]);
        ENTROPY_ROUND(g, h, a, b, c, d, e, f, kw.v[t + 2]);
        ENTROPY_ROUND(f, g, h, a, b, c, d, e, kw.v[t + 3]);
        ENTROPY_ROUND(e, f, g, h, a, b, c, d, kw.v[t + 4]);
        ENTROPY_ROUND(d, e, f, g, h, a, b, c, kw.v[t + 5]);
        ENTROPY_ROUND(c, d, e, f, g, h, a, b, kw.v[t + 6]);
        ENTROPY_ROUND(b, c, d, e, f, g, h, a, kw.v[t + 7]);
    }
    st[0] += a; st[1] += b; st[2] += c; st[3] += d; st[4] += e; st[5] += f; st[6] += g; st[7] += h;
}

// SHA-256 of a 32-byte message given as 8 digest words.
__device__ __forceinline__ void sha256_32(const uint32_t in[8], uint32_t out[8]) {
    uint32_t w[16];
#pragma unroll
    for (int k = 0; k < 8; ++k) w[k] = in[k];
    w[8] = 0x80000000;
#pragma unroll
    for (int k = 9; k < 15; ++k) w[k] = 0;
    w[15] = 256;
#pragma unroll
    for (int k = 0; k < 8; ++k) out[k] = IV[k];
    compress(out, w);
}

// SHA-256 of the 64-byte message left || right (each 8 digest words).
__device__ __forceinline__ void sha256_64(const uint32_t left[8], const uint32_t right[8], uint32_t out[8]) {
    uint32_t w[16];
#pragma unroll
    for (int k = 0; k < 8; ++k) {
        w[k] = left[k];
        w[k + 8] = right[k];
    }
#pragma unroll
    for (int k = 0; k < 8; ++k) out[k] = IV[k];
    compress(out, w);
    compress_const(out, KW_PAD64);
}

// seed = SHA(prefix(60) || offset u64 LE): 68 bytes -> two blocks.
__device__ void seed_hash(const EntropyChunkIdPrefix &prefix, uint64_t offset, uint32_t out[8]) {
    unsigned char msg[128];
#pragma unroll
    for (int k = 0; k < 60; ++k) msg[k] = prefix.bytes[k];
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
        for (int k = 0; k < 16; ++k) {
            const unsigned char *p = msg + blk * 64 + 4 * k;
            w[k] = ((uint32_t)p[0] << 24) | ((uint32_t)p[1] << 16) | ((uint32_t)p[2] << 8) | (uint32_t)p[3];
        }
        compress(out, w);
    }
}

// ---- interleaved buffer ------------------------------------------------------------------

__device__ __forceinline__ uint4 *seg_ptr(uint32_t *base, uint64_t chunks, uint64_t c, uint32_t s) {
    return reinterpret_cast<uint4 *>(base + ((size_t)s * chunks + c) * ENTROPY_SEG_WORDS);
}

// Streaming (evict-first) hints: a segment is not touched again before L2 would evict it.
__device__ __forceinline__ void load_seg(const uint4 *p, uint32_t v[8]) {
    uint4 lo = __ldcs(p), hi = __ldcs(p + 1);
    v[0] = lo.x; v[1] = lo.y; v[2] = lo.z; v[3] = lo.w;
    v[4] = hi.x; v[5] = hi.y; v[6] = hi.z; v[7] = hi.w;
}

__device__ __forceinline__ void store_seg(uint4 *p, const uint32_t v[8]) {
    __stcs(p, make_uint4(v[0], v[1], v[2], v[3]));
    __stcs(p + 1, make_uint4(v[4], v[5], v[6], v[7]));
}

// Phase 2 hash counts [h0, h1) for every chunk; `phase1` additionally runs the seed and Phase 1
// first (first launch only, with h0 == 8192). One thread per chunk, grid-stride for any launch shape.
__global__ void entropy_chunks_kernel(EntropyChunkIdPrefix prefix, uint64_t offset_start, uint64_t chunks,
                                      uint32_t *__restrict__ base, unsigned int h0, unsigned int h1,
                                      int phase1) {
    size_t stride = (size_t)gridDim.x * blockDim.x;
    for (size_t c = (size_t)blockIdx.x * blockDim.x + threadIdx.x; c < chunks; c += stride) {
        uint32_t prev[8];
        if (phase1) {
            seed_hash(prefix, offset_start + c, prev);
            for (uint32_t s = 0; s < ENTROPY_SEGMENTS; ++s) {
                uint32_t out[8];
                sha256_32(prev, out);
                store_seg(seg_ptr(base, chunks, c, s), out);
#pragma unroll
                for (int k = 0; k < 8; ++k) prev[k] = out[k];
            }
        } else {
            load_seg(seg_ptr(base, chunks, c, (h0 - 1) & (ENTROPY_SEGMENTS - 1)), prev);
        }
        if (h0 >= h1) continue;

        // Phase 2: buf[i] = SHA(buf[i - 1] || buf[i]); prev == buf[(h0 - 1) mod 8192]
        uint32_t cur[8];
        load_seg(seg_ptr(base, chunks, c, h0 & (ENTROPY_SEGMENTS - 1)), cur);
        for (unsigned int hc = h0; hc < h1; ++hc) {
            uint32_t i = hc & (ENTROPY_SEGMENTS - 1);
            // buf[i + 1] is not written by this step, so it can be fetched before hashing.
            uint32_t next[8];
            load_seg(seg_ptr(base, chunks, c, (i + 1) & (ENTROPY_SEGMENTS - 1)), next);
            uint32_t out[8];
            sha256_64(prev, cur, out);
            store_seg(seg_ptr(base, chunks, c, i), out);
#pragma unroll
            for (int k = 0; k < 8; ++k) {
                prev[k] = out[k];
                cur[k] = next[k];
            }
        }
    }
}

// Writes chunks [c0, c0 + count) of the interleaved buffer to `out` as contiguous chunks in digest
// byte order. One thread per segment; consecutive threads read consecutive chunks' segment s.
__global__ void chunks_to_bytes_kernel(const uint32_t *__restrict__ base, uint64_t chunks, uint64_t c0,
                                       uint64_t count, unsigned char *__restrict__ out) {
    uint64_t total = count * ENTROPY_SEGMENTS;
    for (uint64_t t = (uint64_t)blockIdx.x * blockDim.x + threadIdx.x; t < total;
         t += (uint64_t)gridDim.x * blockDim.x) {
        uint64_t c = t % count;
        uint32_t s = (uint32_t)(t / count);
        const uint4 *src = reinterpret_cast<const uint4 *>(base + ((size_t)s * chunks + c0 + c) * ENTROPY_SEG_WORDS);
        uint4 lo = src[0], hi = src[1];
        uint4 *dst = reinterpret_cast<uint4 *>(out + c * (uint64_t)ENTROPY_CHUNK_BYTES + (uint64_t)s * 32);
        // state words are big-endian digest words; bytes go out in digest order
        dst[0] = make_uint4(__byte_perm(lo.x, 0, 0x0123), __byte_perm(lo.y, 0, 0x0123),
                            __byte_perm(lo.z, 0, 0x0123), __byte_perm(lo.w, 0, 0x0123));
        dst[1] = make_uint4(__byte_perm(hi.x, 0, 0x0123), __byte_perm(hi.y, 0, 0x0123),
                            __byte_perm(hi.z, 0, 0x0123), __byte_perm(hi.w, 0, 0x0123));
    }
}

} // namespace entropy
