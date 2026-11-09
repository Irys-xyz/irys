// Entropy-packing variant benchmark kernels.
//
// `ref`: variant A follows the production CUDA kernel as it was before the optimised port
// (crates/c/c_src/capacity_cuda.cu up to commit 28bd9374b): same device SHA-256 (cuda/sha256.cuh),
// same per-thread chunk in global memory, same grid-stride loop, so its timing matches that
// kernel. Variant B differs only in Phase 2. Today's production kernel is crates/c/c_src/
// entropy_cuda.cuh, which mirrors `fast-interleaved` A; `--mode prod` measures it end to end.
#include <cuda_runtime.h>
#include <stdint.h>
#include <string.h>

#include "sha256.cuh" // the pre-port production device SHA-256, unmodified

#define PACKING_HASH_SIZE 32
#define HASH_ITERATIONS_PER_BLOCK 8192
#define DATA_CHUNK_SIZE (HASH_ITERATIONS_PER_BLOCK * PACKING_HASH_SIZE)
// mining address (20) + partition hash (32) + chain id (8) + chunk offset (8)
#define CHUNK_ID_LEN 68
#define CHUNK_ID_PREFIX_LEN (CHUNK_ID_LEN - 8)

__constant__ unsigned char d_chunk_id_prefix[CHUNK_ID_PREFIX_LEN];

// --- Shared: seed + Phase 1 (as compute_start_entropy_chunk_cuda) -------------------------

__device__ void pvb_seed_and_phase1(const unsigned char *chunk_id, unsigned char *chunk) {
    unsigned char seed_hash[PACKING_HASH_SIZE];
    SHA256_CTX sha256;
    sha256_init(&sha256);
    sha256_update(&sha256, chunk_id, CHUNK_ID_LEN);
    sha256_final(&sha256, seed_hash);

    const unsigned char *previous_segment = seed_hash;
    size_t chunk_len = 0;
    while (chunk_len < DATA_CHUNK_SIZE) {
        sha256_init(&sha256);
        sha256_update(&sha256, previous_segment, PACKING_HASH_SIZE);
        sha256_final(&sha256, chunk + chunk_len);
        previous_segment = chunk + chunk_len;
        chunk_len += PACKING_HASH_SIZE;
    }
}

// --- Variant A: Phase 2 as compute_entropy_chunk2_cuda ------------------------------------
// `segment` always points at the most recently written segment, i.e. buf[i-1] (or buf[8191]).

__device__ void pvb_phase2_a(const unsigned char *segment, unsigned char *entropy_chunk, unsigned int iterations) {
    SHA256_CTX sha256;
    for (unsigned int hash_count = HASH_ITERATIONS_PER_BLOCK; hash_count < iterations; ++hash_count) {
        size_t start_offset = (hash_count % HASH_ITERATIONS_PER_BLOCK) * PACKING_HASH_SIZE;
        sha256_init(&sha256);
        sha256_update(&sha256, segment, PACKING_HASH_SIZE);
        sha256_update(&sha256, entropy_chunk + start_offset, PACKING_HASH_SIZE);
        sha256_final(&sha256, entropy_chunk + start_offset);
        segment = entropy_chunk + start_offset;
    }
}

// --- Variant B: data-dependent read index --------------------------------------------------
// j = LE u32(prev[0..4]) & 8191; buf[i] = SHA(prev || buf[j]); prev = buf[i]

__device__ void pvb_phase2_b(const unsigned char *prev, unsigned char *entropy_chunk, unsigned int iterations) {
    SHA256_CTX sha256;
    for (unsigned int hash_count = HASH_ITERATIONS_PER_BLOCK; hash_count < iterations; ++hash_count) {
        size_t start_offset = (hash_count % HASH_ITERATIONS_PER_BLOCK) * PACKING_HASH_SIZE;
        uint32_t j = ((uint32_t)prev[0] | ((uint32_t)prev[1] << 8) | ((uint32_t)prev[2] << 16) |
                      ((uint32_t)prev[3] << 24)) & (HASH_ITERATIONS_PER_BLOCK - 1);
        sha256_init(&sha256);
        sha256_update(&sha256, prev, PACKING_HASH_SIZE);
        sha256_update(&sha256, entropy_chunk + (size_t)j * PACKING_HASH_SIZE, PACKING_HASH_SIZE);
        sha256_final(&sha256, entropy_chunk + start_offset);
        prev = entropy_chunk + start_offset;
    }
}

template <int VARIANT>
__global__ void pvb_kernel(uint64_t chunk_offset_start, uint64_t chunks_count, unsigned char *chunks,
                           unsigned int iterations) {
    size_t idx = (size_t)blockIdx.x * blockDim.x + threadIdx.x;
    size_t stride = (size_t)gridDim.x * blockDim.x;
    for (size_t chunk_idx = idx; chunk_idx < (size_t)chunks_count; chunk_idx += stride) {
        unsigned char *output = chunks + (chunk_idx * (size_t)DATA_CHUNK_SIZE);

        __align__(8) unsigned char chunk_id_thread[CHUNK_ID_LEN];
        memcpy(chunk_id_thread, d_chunk_id_prefix, CHUNK_ID_PREFIX_LEN);
        uint64_t full_offset = chunk_offset_start + (uint64_t)chunk_idx;
        memcpy(&chunk_id_thread[CHUNK_ID_PREFIX_LEN], &full_offset, sizeof(uint64_t));

        pvb_seed_and_phase1(chunk_id_thread, output);

        const unsigned char *last = output + (HASH_ITERATIONS_PER_BLOCK - 1) * PACKING_HASH_SIZE;
        if (VARIANT == 0) {
            // as the reference: copy the last segment into a local before Phase 2
            unsigned char last_entropy_chunk_segment[PACKING_HASH_SIZE];
            memcpy(last_entropy_chunk_segment, last, PACKING_HASH_SIZE);
            pvb_phase2_a(last_entropy_chunk_segment, output, iterations);
        } else {
            unsigned char last_entropy_chunk_segment[PACKING_HASH_SIZE];
            memcpy(last_entropy_chunk_segment, last, PACKING_HASH_SIZE);
            pvb_phase2_b(last_entropy_chunk_segment, output, iterations);
        }
    }
}

// --- Host API (all functions return a cudaError_t as int; 0 == success) -------------------

extern "C" const char *pvb_cuda_error_string(int err) { return cudaGetErrorString((cudaError_t)err); }

// Device info via cudaDeviceGetAttribute (no cudaDeviceProp crosses the FFI boundary).
extern "C" int pvb_cuda_device_info(int *sm_count, int *cc_major, int *cc_minor, int *max_threads_per_sm,
                                    int *l2_bytes) {
    cudaError_t e;
    if ((e = cudaDeviceGetAttribute(sm_count, cudaDevAttrMultiProcessorCount, 0)) != cudaSuccess) return (int)e;
    if ((e = cudaDeviceGetAttribute(cc_major, cudaDevAttrComputeCapabilityMajor, 0)) != cudaSuccess) return (int)e;
    if ((e = cudaDeviceGetAttribute(cc_minor, cudaDevAttrComputeCapabilityMinor, 0)) != cudaSuccess) return (int)e;
    if ((e = cudaDeviceGetAttribute(max_threads_per_sm, cudaDevAttrMaxThreadsPerMultiProcessor, 0)) != cudaSuccess)
        return (int)e;
    return (int)cudaDeviceGetAttribute(l2_bytes, cudaDevAttrL2CacheSize, 0);
}

extern "C" int pvb_cuda_mem_info(size_t *free_bytes, size_t *total_bytes) {
    return (int)cudaMemGetInfo(free_bytes, total_bytes);
}

extern "C" int pvb_cuda_driver_version(int *version) { return (int)cudaDriverGetVersion(version); }

extern "C" int pvb_cuda_kernel_regs(int variant, int *regs, size_t *local_bytes) {
    cudaFuncAttributes attr;
    cudaError_t e = variant == 0 ? cudaFuncGetAttributes(&attr, pvb_kernel<0>)
                                 : cudaFuncGetAttributes(&attr, pvb_kernel<1>);
    if (e == cudaSuccess) {
        *regs = attr.numRegs;
        *local_bytes = attr.localSizeBytes;
    }
    return (int)e;
}

extern "C" int pvb_cuda_occupancy(int variant, int threads_per_block, int *blocks_per_sm) {
    return variant == 0
               ? (int)cudaOccupancyMaxActiveBlocksPerMultiprocessor(blocks_per_sm, pvb_kernel<0>, threads_per_block, 0)
               : (int)cudaOccupancyMaxActiveBlocksPerMultiprocessor(blocks_per_sm, pvb_kernel<1>, threads_per_block, 0);
}

extern "C" int pvb_cuda_malloc(size_t bytes, void **ptr) { return (int)cudaMalloc(ptr, bytes); }

extern "C" int pvb_cuda_free(void *ptr) { return (int)cudaFree(ptr); }

extern "C" int pvb_cuda_set_chunk_id_prefix(const unsigned char *prefix) {
    return (int)cudaMemcpyToSymbol(d_chunk_id_prefix, prefix, CHUNK_ID_PREFIX_LEN);
}

// Launches one variant over `chunks_count` chunks and returns the cudaEvent-timed kernel ms.
extern "C" int pvb_cuda_run(int variant, uint64_t chunk_offset_start, uint64_t chunks_count, void *d_chunks,
                            unsigned int iterations, int blocks, int threads_per_block, float *out_ms) {
    cudaEvent_t start, stop;
    cudaError_t e;
    if ((e = cudaEventCreate(&start)) != cudaSuccess) return (int)e;
    if ((e = cudaEventCreate(&stop)) != cudaSuccess) { cudaEventDestroy(start); return (int)e; }

    if ((e = cudaEventRecord(start)) != cudaSuccess) goto done;
    if (variant == 0)
        pvb_kernel<0><<<blocks, threads_per_block>>>(chunk_offset_start, chunks_count, (unsigned char *)d_chunks, iterations);
    else
        pvb_kernel<1><<<blocks, threads_per_block>>>(chunk_offset_start, chunks_count, (unsigned char *)d_chunks, iterations);
    if ((e = cudaGetLastError()) != cudaSuccess) goto done;
    if ((e = cudaEventRecord(stop)) != cudaSuccess) goto done;
    if ((e = cudaEventSynchronize(stop)) != cudaSuccess) goto done;
    // catches asynchronous kernel faults
    if ((e = cudaDeviceSynchronize()) != cudaSuccess) goto done;
    e = cudaEventElapsedTime(out_ms, start, stop);

done:
    cudaEventDestroy(start);
    cudaEventDestroy(stop);
    return (int)e;
}

extern "C" int pvb_cuda_copy_chunk(const void *d_chunks, uint64_t index, unsigned char *host_out) {
    return (int)cudaMemcpy(host_out, (const unsigned char *)d_chunks + index * (uint64_t)DATA_CHUNK_SIZE,
                           DATA_CHUNK_SIZE, cudaMemcpyDeviceToHost);
}

// --- Optimised implementation (see pvb_fast.cuh) ----------------------------------------
// variant: 0 = A, 1 = B; layout: 0 = contiguous chunk per thread, 1 = segment-interleaved,
// 2 = no-memory diagnostic (does not produce entropy; never copied back).

#include "pvb_fast.cuh"

typedef void (*fast_kernel_t)(uint64_t, uint64_t, uint32_t *, unsigned int, unsigned int, int, uint32_t);

static fast_kernel_t fast_kernel_for(int variant, int layout) {
    if (variant == 0) return layout == 0 ? fast::kernel<0, 0> : layout == 1 ? fast::kernel<0, 1> : fast::kernel<0, 2>;
    return layout == 0 ? fast::kernel<1, 0> : layout == 1 ? fast::kernel<1, 1> : fast::kernel<1, 2>;
}

extern "C" int pvb_fast_kernel_regs(int variant, int layout, int *regs, size_t *local_bytes) {
    cudaFuncAttributes attr;
    cudaError_t e = cudaFuncGetAttributes(&attr, fast_kernel_for(variant, layout));
    if (e == cudaSuccess) {
        *regs = attr.numRegs;
        *local_bytes = attr.localSizeBytes;
    }
    return (int)e;
}

extern "C" int pvb_fast_occupancy(int variant, int layout, int threads_per_block, int *blocks_per_sm) {
    return (int)cudaOccupancyMaxActiveBlocksPerMultiprocessor(blocks_per_sm, fast_kernel_for(variant, layout),
                                                              threads_per_block, 0);
}

// `slice` > 0 splits Phase 2 into launches of at most `slice` hash counts (see pvb_fast.cuh);
// 0 runs everything in one launch. The events bracket all launches.
extern "C" int pvb_fast_run(int variant, int layout, uint64_t chunk_offset_start, uint64_t chunks_count,
                            void *d_chunks, unsigned int iterations, unsigned int slice, int blocks,
                            int threads_per_block, float *out_ms) {
    fast_kernel_t k = fast_kernel_for(variant, layout);
    cudaEvent_t start, stop;
    cudaError_t e;
    if ((e = cudaEventCreate(&start)) != cudaSuccess) return (int)e;
    if ((e = cudaEventCreate(&stop)) != cudaSuccess) { cudaEventDestroy(start); return (int)e; }

    if ((e = cudaEventRecord(start)) != cudaSuccess) goto done;
    {
        unsigned int h0 = HASH_ITERATIONS_PER_BLOCK;
        unsigned int end = iterations > h0 ? iterations : h0;
        int first = 1;
        do {
            unsigned int h1 = (slice == 0 || end - h0 <= slice) ? end : h0 + slice;
            // the last argument is the `one` multiplier used by FAST_FMA_ADDS
            k<<<blocks, threads_per_block>>>(chunk_offset_start, chunks_count, (uint32_t *)d_chunks, h0, h1,
                                             first, 1u);
            if ((e = cudaGetLastError()) != cudaSuccess) goto done;
            first = 0;
            h0 = h1;
        } while (h0 < end);
    }
    if ((e = cudaEventRecord(stop)) != cudaSuccess) goto done;
    if ((e = cudaEventSynchronize(stop)) != cudaSuccess) goto done;
    if ((e = cudaDeviceSynchronize()) != cudaSuccess) goto done;
    e = cudaEventElapsedTime(out_ms, start, stop);

done:
    cudaEventDestroy(start);
    cudaEventDestroy(stop);
    return (int)e;
}

// Copies chunk `index` back in digest byte order (the device stores native-endian state words).
extern "C" int pvb_fast_copy_chunk(int layout, const void *d_chunks, uint64_t chunks_count, uint64_t index,
                                   unsigned char *host_out) {
    const unsigned char *base = (const unsigned char *)d_chunks;
    cudaError_t e;
    if (layout == 0) {
        e = cudaMemcpy(host_out, base + index * (uint64_t)DATA_CHUNK_SIZE, DATA_CHUNK_SIZE, cudaMemcpyDeviceToHost);
    } else {
        // segment s of chunk c is at (s * chunks + c) * 32
        e = cudaMemcpy2D(host_out, PACKING_HASH_SIZE, base + index * PACKING_HASH_SIZE,
                         chunks_count * PACKING_HASH_SIZE, PACKING_HASH_SIZE, HASH_ITERATIONS_PER_BLOCK,
                         cudaMemcpyDeviceToHost);
    }
    if (e != cudaSuccess) return (int)e;
    for (size_t k = 0; k < DATA_CHUNK_SIZE; k += 4) {
        unsigned char t0 = host_out[k], t1 = host_out[k + 1];
        host_out[k] = host_out[k + 3];
        host_out[k + 1] = host_out[k + 2];
        host_out[k + 2] = t1;
        host_out[k + 3] = t0;
    }
    return 0;
}
