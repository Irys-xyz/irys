// Prototype VDE packing on the GPU: the same encoding as src/vde.rs (spec: scripts/vde_ref.py).
//
// Elements are 8 little-endian 32-bit limbs, chunk-major: element i of chunk c is at
// d_elems + (c * 8192 + i) * 32 bytes, i.e. the chunk's bytes as stored on disk.
//
//  * vde_roots_kernel: one thread per element, x = root(x + k[i] + layer). The key k[i] =
//    SHA256(seed || LE32(i) || 0^28) is recomputed per layer (~3% of a root).
//  * vde_mix_kernel: one block per chunk, one thread per lane, __syncthreads() between steps
//    (y[j] may come from any lane at an earlier step).
//
// Arithmetic mod p = 2^256 - 189 is portable C (32x32->64 multiplies); not yet hand-tuned PTX.
#pragma once
#include <stdint.h>

namespace vde {

constexpr int N = 8192;
constexpr uint32_t C = 189;  // 2^256 mod p
constexpr uint32_t P0 = 0xFFFFFF43u;

__device__ __forceinline__ uint32_t bswap(uint32_t x) { return __byte_perm(x, 0, 0x0123); }

// 512-bit product -> value < 2^256, congruent mod p.
__device__ __forceinline__ void reduce(const uint32_t t[16], uint32_t r[8]) {
    uint64_t c = 0;
#pragma unroll
    for (int k = 0; k < 8; ++k) {
        uint64_t v = (uint64_t)t[k + 8] * C + t[k] + c;
        r[k] = (uint32_t)v;
        c = v >> 32;
    }
    c *= C;  // c <= 189
#pragma unroll
    for (int k = 0; k < 8; ++k) {
        uint64_t v = (uint64_t)r[k] + c;
        r[k] = (uint32_t)v;
        c = v >> 32;
    }
    if (c) r[0] += C;  // wrapped: the value is tiny now
}

__device__ __forceinline__ void mul(const uint32_t a[8], const uint32_t b[8], uint32_t r[8]) {
    uint32_t t[16];
#pragma unroll
    for (int k = 0; k < 16; ++k) t[k] = 0;
#pragma unroll
    for (int i = 0; i < 8; ++i) {
        uint64_t c = 0;
#pragma unroll
        for (int j = 0; j < 8; ++j) {
            uint64_t v = (uint64_t)a[i] * b[j] + t[i + j] + c;
            t[i + j] = (uint32_t)v;
            c = v >> 32;
        }
        t[i + 8] = (uint32_t)c;
    }
    reduce(t, r);
}

__device__ __forceinline__ void sqr(const uint32_t a[8], uint32_t r[8]) { mul(a, a, r); }

__device__ __forceinline__ void sqr_n(uint32_t x[8], int n) {
    for (int k = 0; k < n; ++k) {
        uint32_t t[8];
        sqr(x, t);
#pragma unroll
        for (int w = 0; w < 8; ++w) x[w] = t[w];
    }
}

__device__ __forceinline__ void copy(const uint32_t a[8], uint32_t r[8]) {
#pragma unroll
    for (int w = 0; w < 8; ++w) r[w] = a[w];
}

__device__ __forceinline__ bool ge_p(const uint32_t x[8]) {
    bool top = true;
#pragma unroll
    for (int w = 1; w < 8; ++w) top &= x[w] == 0xFFFFFFFFu;
    return top && x[0] >= P0;
}

__device__ __forceinline__ void canonical(uint32_t x[8]) {
    if (ge_p(x)) {
        uint64_t c = C;
#pragma unroll
        for (int w = 0; w < 8; ++w) {
            uint64_t v = (uint64_t)x[w] + c;
            x[w] = (uint32_t)v;
            c = v >> 32;
        }
    }
}

__device__ __forceinline__ void add_wrap(const uint32_t a[8], const uint32_t b[8], uint32_t r[8]) {
    uint64_t c = 0;
#pragma unroll
    for (int w = 0; w < 8; ++w) {
        uint64_t v = (uint64_t)a[w] + b[w] + c;
        r[w] = (uint32_t)v;
        c = v >> 32;
    }
}

// (0 - x) mod p for x < p.
__device__ __forceinline__ void neg(const uint32_t x[8], uint32_t r[8]) {
    uint64_t borrow = 0;
#pragma unroll
    for (int w = 0; w < 8; ++w) {
        uint64_t v = (uint64_t)0 - x[w] - borrow;
        r[w] = (uint32_t)v;
        borrow = (v >> 32) & 1;
    }
    if (borrow) {  // + p == - 189 mod 2^256
        uint64_t b = C;
#pragma unroll
        for (int w = 0; w < 8; ++w) {
            uint64_t v = (uint64_t)r[w] - b;
            r[w] = (uint32_t)v;
            b = (v >> 32) & 1;
        }
    }
}

// x^((p+1)/4) = x^(2^254 - 47), canonical.
__device__ __noinline__ void pow_sqrt(const uint32_t x[8], uint32_t out[8]) {
    uint32_t x2[8], x4[8], x8[8], x16[8], x32[8], x64[8], acc[8], t[8];
    sqr(x, t); mul(t, x, x2);
    copy(x2, t); sqr_n(t, 2); mul(t, x2, x4);
    copy(x4, t); sqr_n(t, 4); mul(t, x4, x8);
    copy(x8, t); sqr_n(t, 8); mul(t, x8, x16);
    copy(x16, t); sqr_n(t, 16); mul(t, x16, x32);
    copy(x32, t); sqr_n(t, 32); mul(t, x32, x64);
    copy(x64, t); sqr_n(t, 64); mul(t, x64, acc);  // x128
    sqr_n(acc, 64); mul(acc, x64, t); copy(t, acc);  // x192
    sqr_n(acc, 32); mul(acc, x32, t); copy(t, acc);  // x224
    sqr_n(acc, 16); mul(acc, x16, t); copy(t, acc);  // x240
    sqr_n(acc, 8); mul(acc, x8, t); copy(t, acc);    // x248
    sqr_n(acc, 2); mul(acc, x, t); copy(t, acc);     // ..01
    sqr_n(acc, 4); mul(acc, x, out);                 // ..010001
    canonical(out);
}

__device__ __forceinline__ void root(uint32_t x[8]) {
    if (ge_p(x)) return;  // the 189 values from p up pass through
    uint32_t r[8], s[8];
    pow_sqrt(x, r);
    sqr(r, s);
    canonical(s);
    bool is_square = true;
#pragma unroll
    for (int w = 0; w < 8; ++w) is_square &= s[w] == x[w];
    bool r_even = (r[0] & 1) == 0;
    if (is_square == r_even) {
        copy(r, x);
    } else {
        neg(r, x);
    }
}

// SHA256(left_bytes || right_bytes) for two elements in limb form; result in limb form.
__device__ __forceinline__ void hash2(const uint32_t left[8], const uint32_t right[8], uint32_t out[8]) {
    uint32_t l[8], r[8], d[8];
#pragma unroll
    for (int w = 0; w < 8; ++w) {
        l[w] = bswap(left[w]);
        r[w] = bswap(right[w]);
    }
    fast::sha256_64(l, r, d, 1);
#pragma unroll
    for (int w = 0; w < 8; ++w) out[w] = bswap(d[w]);
}

__device__ __forceinline__ void load(const uint32_t *p, uint32_t v[8]) {
    const uint4 *q = reinterpret_cast<const uint4 *>(p);
    uint4 lo = q[0], hi = q[1];
    v[0] = lo.x; v[1] = lo.y; v[2] = lo.z; v[3] = lo.w;
    v[4] = hi.x; v[5] = hi.y; v[6] = hi.z; v[7] = hi.w;
}

__device__ __forceinline__ void store(uint32_t *p, const uint32_t v[8]) {
    uint4 *q = reinterpret_cast<uint4 *>(p);
    q[0] = make_uint4(v[0], v[1], v[2], v[3]);
    q[1] = make_uint4(v[4], v[5], v[6], v[7]);
}

// seeds: chunks x 8 limbs (the seed bytes, little-endian words).
__global__ void roots_kernel(const uint32_t *__restrict__ seeds, uint64_t chunks, uint32_t *__restrict__ elems,
                             uint32_t layer) {
    uint64_t g = (uint64_t)blockIdx.x * blockDim.x + threadIdx.x;
    if (g >= chunks * N) return;
    uint64_t c = g / N;
    uint32_t i = (uint32_t)(g % N);
    uint32_t seed[8], msg[8] = {i, 0, 0, 0, 0, 0, 0, 0}, k[8], x[8], t[8];
    load(seeds + c * 8, seed);
    hash2(seed, msg, k);
    load(elems + g * 8, x);
    add_wrap(x, k, t);
    uint32_t l[8] = {layer, 0, 0, 0, 0, 0, 0, 0};
    add_wrap(t, l, x);
    root(x);
    store(elems + g * 8, x);
}

// One block per chunk, blockDim.x == lanes (<= 1024), w = 8192 / lanes.
__global__ void mix_kernel(uint32_t *__restrict__ elems, uint32_t lanes, uint32_t w) {
    uint32_t *base = elems + (uint64_t)blockIdx.x * N * 8;
    uint32_t lane = threadIdx.x;
    for (uint32_t t = 1; t < w; ++t) {
        uint32_t i = lane * w + t;
        uint32_t prev[8], other[8], h[8], y[8], z[8];
        load(base + (i - 1) * 8, prev);
        uint32_t m = prev[0] % (t * lanes);
        uint32_t j = (m % lanes) * w + m / lanes;
        load(base + j * 8, other);
        hash2(prev, other, h);
        load(base + i * 8, z);
        add_wrap(z, h, y);
        store(base + i * 8, y);
        __syncthreads();
    }
}

}  // namespace vde

extern "C" int pvb_cuda_memcpy(void *dst, const void *src, size_t bytes, int to_device) {
    return (int)cudaMemcpy(dst, src, bytes, to_device ? cudaMemcpyHostToDevice : cudaMemcpyDeviceToHost);
}

extern "C" int pvb_vde_kernel_regs(int *roots_regs, size_t *roots_local, int *mix_regs) {
    cudaFuncAttributes a;
    cudaError_t e = cudaFuncGetAttributes(&a, vde::roots_kernel);
    if (e != cudaSuccess) return (int)e;
    *roots_regs = a.numRegs;
    *roots_local = a.localSizeBytes;
    e = cudaFuncGetAttributes(&a, vde::mix_kernel);
    if (e != cudaSuccess) return (int)e;
    *mix_regs = a.numRegs;
    return 0;
}

// Encodes `chunks` chunks in place: `layers` x (roots, then mix unless lanes == 8192).
// `d_seeds` holds chunks x 32 seed bytes on the device. Kernel times are summed per step.
extern "C" int pvb_vde_encode(const void *d_seeds, uint64_t chunks, void *d_elems, unsigned int layers,
                              unsigned int lanes, int threads_per_block, float *roots_ms, float *mix_ms) {
    if (lanes == 0 || (lanes & (lanes - 1)) || lanes > (unsigned)vde::N) return (int)cudaErrorInvalidValue;
    if (lanes < (unsigned)vde::N && lanes > 1024) return (int)cudaErrorInvalidValue;  // one block per chunk
    unsigned int w = vde::N / lanes;
    cudaEvent_t e0, e1, e2;
    cudaEventCreate(&e0);
    cudaEventCreate(&e1);
    cudaEventCreate(&e2);
    *roots_ms = 0;
    *mix_ms = 0;
    uint64_t threads = chunks * vde::N;
    unsigned int blocks = (unsigned int)((threads + threads_per_block - 1) / threads_per_block);
    cudaError_t e = cudaSuccess;
    for (unsigned int layer = 0; layer < layers && e == cudaSuccess; ++layer) {
        cudaEventRecord(e0);
        vde::roots_kernel<<<blocks, threads_per_block>>>((const uint32_t *)d_seeds, chunks, (uint32_t *)d_elems,
                                                         layer);
        cudaEventRecord(e1);
        if (w > 1) vde::mix_kernel<<<(unsigned int)chunks, lanes>>>((uint32_t *)d_elems, lanes, w);
        cudaEventRecord(e2);
        e = cudaEventSynchronize(e2);
        if (e == cudaSuccess) e = cudaGetLastError();
        float a = 0, b = 0;
        cudaEventElapsedTime(&a, e0, e1);
        cudaEventElapsedTime(&b, e1, e2);
        *roots_ms += a;
        *mix_ms += b;
    }
    cudaEventDestroy(e0);
    cudaEventDestroy(e1);
    cudaEventDestroy(e2);
    return (int)e;
}
