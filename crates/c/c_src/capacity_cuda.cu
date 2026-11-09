#include <cuda_runtime.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "capacity_cuda.h"
#include "entropy_cuda.cuh"

// Compile-time sanity checks to ensure chunk sizing remains consistent with SHA-256 packing.
static_assert(PACKING_HASH_SIZE == 32, "PACKING_HASH_SIZE must be 32 bytes (SHA-256)");
static_assert(DATA_CHUNK_SIZE == (HASH_ITERATIONS_PER_BLOCK * PACKING_HASH_SIZE), "DATA_CHUNK_SIZE must equal HASH_ITERATIONS_PER_BLOCK * PACKING_HASH_SIZE");
static_assert(DATA_CHUNK_SIZE == ENTROPY_CHUNK_BYTES, "entropy_cuda.cuh chunk size must match DATA_CHUNK_SIZE");
static_assert(HASH_ITERATIONS_PER_BLOCK == ENTROPY_SEGMENTS, "entropy_cuda.cuh segment count must match");
static_assert(IRYS_CUDA_SLAB_CHUNKS > 0, "slab must hold at least one chunk");

// Phase 2 hash counts per kernel launch: ~0.3 s per launch on an RTX 5090 at ~87k chunks. Long
// single launches (tens of seconds) were seen to fail silently under WDDM; overhead of the split
// is within measurement noise.
#define PHASE2_HASHES_PER_LAUNCH 50000

// Threads per block for the byte-conversion kernel.
#define CONVERT_THREADS 256

// Two output slabs, so the GPU fills one while the caller consumes the other.
#define SLAB_BUFFERS 2

struct irys_cuda_packer {
    int device;
    cudaStream_t stream;
    // most chunks one batch may hold
    uint64_t capacity;
    // segment-interleaved working buffer: capacity chunks
    uint32_t *d_chunks;
    // per slab buffer: device staging (contiguous chunks), pinned host copy, and its ready event
    unsigned char *d_slab[SLAB_BUFFERS];
    unsigned char *h_slab[SLAB_BUFFERS];
    cudaEvent_t ready[SLAB_BUFFERS];
    // current batch
    uint64_t chunks;
    uint64_t slabs;
    // next slab to queue for conversion / to hand out
    uint64_t next_queue;
    uint64_t next_out;
    // slab buffer last handed out (still being read by the caller), or -1
    int lent;
};

extern "C" int get_registers() {
    cudaFuncAttributes attr;
    if (cudaFuncGetAttributes(&attr, entropy::entropy_chunks_kernel) != cudaSuccess) {
        return -1;
    }
    return attr.numRegs;
}

extern "C" const char *irys_cuda_error_string(int err) { return cudaGetErrorString((cudaError_t)err); }

// `cudaDeviceProp` comes from the toolkit header used for this build, so its size always
// matches the linked runtime. A Rust copy of the struct does not have that property.
extern "C" int irys_cuda_device_name(int device, char *name, size_t name_len) {
    if (name == NULL || name_len == 0) {
        return cudaErrorInvalidValue;
    }
    cudaDeviceProp prop;
    cudaError_t err = cudaGetDeviceProperties(&prop, device);
    if (err != cudaSuccess) {
        return (int)err;
    }
    strncpy(name, prop.name, name_len - 1);
    name[name_len - 1] = '\0';
    return 0;
}

extern "C" int irys_cuda_mem_info(int device, size_t *free_bytes, size_t *total_bytes) {
    if (free_bytes == NULL || total_bytes == NULL) {
        return cudaErrorInvalidValue;
    }
    int previous = 0;
    cudaError_t err = cudaGetDevice(&previous);
    if (err != cudaSuccess) return (int)err;
    if ((err = cudaSetDevice(device)) != cudaSuccess) return (int)err;
    err = cudaMemGetInfo(free_bytes, total_bytes);
    cudaError_t restore = cudaSetDevice(previous);
    if (err != cudaSuccess) return (int)err;
    return (int)restore;
}

extern "C" void irys_cuda_packer_free(irys_cuda_packer *p) {
    if (p == NULL) return;
    if (cudaSetDevice(p->device) == cudaSuccess && p->stream != NULL) {
        // work queued by an abandoned batch must finish before its buffers go
        (void)cudaStreamSynchronize(p->stream);
    }
    for (int b = 0; b < SLAB_BUFFERS; ++b) {
        if (p->ready[b] != NULL) cudaEventDestroy(p->ready[b]);
        cudaFreeHost(p->h_slab[b]);
        cudaFree(p->d_slab[b]);
    }
    cudaFree(p->d_chunks);
    if (p->stream != NULL) cudaStreamDestroy(p->stream);
    (void)cudaGetLastError();
    free(p);
}

extern "C" int irys_cuda_packer_new(int device, uint64_t capacity, irys_cuda_packer **out) {
    if (out == NULL || capacity == 0) return cudaErrorInvalidValue;
    *out = NULL;
    irys_cuda_packer *p = (irys_cuda_packer *)calloc(1, sizeof(irys_cuda_packer));
    if (p == NULL) return cudaErrorMemoryAllocation;
    p->device = device;
    p->capacity = capacity;
    p->lent = -1;
    const uint64_t slab_chunks = capacity < IRYS_CUDA_SLAB_CHUNKS ? capacity : IRYS_CUDA_SLAB_CHUNKS;

    cudaError_t err;
    if ((err = cudaSetDevice(device)) != cudaSuccess) goto fail;
    if ((err = cudaStreamCreateWithFlags(&p->stream, cudaStreamNonBlocking)) != cudaSuccess) goto fail;
    if ((err = cudaMalloc((void **)&p->d_chunks, capacity * DATA_CHUNK_SIZE)) != cudaSuccess) goto fail;
    for (int b = 0; b < SLAB_BUFFERS; ++b) {
        if ((err = cudaMalloc((void **)&p->d_slab[b], slab_chunks * DATA_CHUNK_SIZE)) != cudaSuccess) goto fail;
        // pinned, so the copy-back runs at full PCIe speed and asynchronously
        if ((err = cudaHostAlloc((void **)&p->h_slab[b], slab_chunks * DATA_CHUNK_SIZE, cudaHostAllocDefault)) !=
            cudaSuccess)
            goto fail;
        if ((err = cudaEventCreateWithFlags(&p->ready[b], cudaEventDisableTiming)) != cudaSuccess) goto fail;
    }
    *out = p;
    return 0;

fail:
    irys_cuda_packer_free(p);
    return (int)err;
}

// Queues conversion + copy-back of slab `slab` into slab buffer `slab % SLAB_BUFFERS`.
static cudaError_t queue_slab(irys_cuda_packer *p, uint64_t slab) {
    const int b = (int)(slab % SLAB_BUFFERS);
    const uint64_t c0 = slab * IRYS_CUDA_SLAB_CHUNKS;
    const uint64_t count = p->chunks - c0 < IRYS_CUDA_SLAB_CHUNKS ? p->chunks - c0 : IRYS_CUDA_SLAB_CHUNKS;
    const uint64_t segments = count * HASH_ITERATIONS_PER_BLOCK;
    const unsigned int blocks = (unsigned int)((segments + CONVERT_THREADS - 1) / CONVERT_THREADS);
    entropy::chunks_to_bytes_kernel<<<blocks, CONVERT_THREADS, 0, p->stream>>>(p->d_chunks, p->chunks, c0, count,
                                                                                p->d_slab[b]);
    cudaError_t err = cudaGetLastError();
    if (err != cudaSuccess) return err;
    err = cudaMemcpyAsync(p->h_slab[b], p->d_slab[b], count * DATA_CHUNK_SIZE, cudaMemcpyDeviceToHost, p->stream);
    if (err != cudaSuccess) return err;
    return cudaEventRecord(p->ready[b], p->stream);
}

extern "C" int irys_cuda_packer_start(irys_cuda_packer *p, const unsigned char *mining_addr,
                                      const unsigned char *partition_hash, uint64_t chain_id,
                                      uint64_t chunk_offset_start, uint64_t chunks_count, uint32_t iterations,
                                      int threads_per_block) {
    if (p == NULL || mining_addr == NULL || partition_hash == NULL || chunks_count == 0 ||
        chunks_count > p->capacity || threads_per_block <= 0) {
        return cudaErrorInvalidValue;
    }
    cudaError_t err;
    if ((err = cudaSetDevice(p->device)) != cudaSuccess) return (int)err;
    // an abandoned previous batch may still have work queued on the buffers
    if ((err = cudaStreamSynchronize(p->stream)) != cudaSuccess) return (int)err;

    EntropyChunkIdPrefix prefix;
    memcpy(prefix.bytes, mining_addr, 20);
    memcpy(prefix.bytes + 20, partition_hash, 32);
    memcpy(prefix.bytes + 52, &chain_id, 8);

    p->chunks = chunks_count;
    p->slabs = (chunks_count + IRYS_CUDA_SLAB_CHUNKS - 1) / IRYS_CUDA_SLAB_CHUNKS;
    p->next_queue = 0;
    p->next_out = 0;
    p->lent = -1;

    // Seed + Phase 1 in the first launch, then Phase 2 in slices; one thread per chunk.
    const unsigned int blocks = (unsigned int)((chunks_count + threads_per_block - 1) / threads_per_block);
    unsigned int h0 = HASH_ITERATIONS_PER_BLOCK;
    const unsigned int end = iterations > h0 ? iterations : h0;
    int first = 1;
    do {
        unsigned int h1 = end - h0 <= PHASE2_HASHES_PER_LAUNCH ? end : h0 + PHASE2_HASHES_PER_LAUNCH;
        entropy::entropy_chunks_kernel<<<blocks, threads_per_block, 0, p->stream>>>(
            prefix, chunk_offset_start, chunks_count, p->d_chunks, h0, h1, first);
        if ((err = cudaGetLastError()) != cudaSuccess) return (int)err;
        first = 0;
        h0 = h1;
    } while (h0 < end);

    // the first slabs start copying back as soon as the kernels finish
    for (; p->next_queue < p->slabs && p->next_queue < SLAB_BUFFERS; ++p->next_queue) {
        if ((err = queue_slab(p, p->next_queue)) != cudaSuccess) return (int)err;
    }
    return 0;
}

extern "C" int irys_cuda_packer_next(irys_cuda_packer *p, const unsigned char **bytes, uint64_t *first_chunk,
                                     uint64_t *chunk_count) {
    if (p == NULL || bytes == NULL || first_chunk == NULL || chunk_count == NULL) return cudaErrorInvalidValue;
    cudaError_t err;
    if ((err = cudaSetDevice(p->device)) != cudaSuccess) return (int)err;

    // the caller is done with the slab it was lent: refill that buffer with the next slab
    if (p->lent >= 0) {
        p->lent = -1;
        if (p->next_queue < p->slabs) {
            if ((err = queue_slab(p, p->next_queue)) != cudaSuccess) return (int)err;
            ++p->next_queue;
        }
    }
    if (p->next_out >= p->slabs) return IRYS_CUDA_PACKER_DONE;

    const int b = (int)(p->next_out % SLAB_BUFFERS);
    // also reports execution errors of the packing kernels queued before this slab
    if ((err = cudaEventSynchronize(p->ready[b])) != cudaSuccess) return (int)err;

    const uint64_t c0 = p->next_out * IRYS_CUDA_SLAB_CHUNKS;
    *bytes = p->h_slab[b];
    *first_chunk = c0;
    *chunk_count = p->chunks - c0 < IRYS_CUDA_SLAB_CHUNKS ? p->chunks - c0 : IRYS_CUDA_SLAB_CHUNKS;
    p->lent = b;
    ++p->next_out;
    return 0;
}

/**
 * One-shot form of the packer: computes `chunks_count` entropy chunks for consecutive partition
 * offsets starting at `chunk_offset_start` into `chunks` (chunks_count * DATA_CHUNK_SIZE bytes).
 * Allocates and frees a packer per call; long-running callers should keep an `irys_cuda_packer`.
 * `blocks` is ignored (the launch covers every chunk); `threads_per_block` sets the block size.
 */
extern "C" entropy_chunk_errors compute_entropy_chunks_cuda(const unsigned char *mining_addr, size_t mining_addr_size, unsigned long int chunk_offset_start, unsigned long int chain_id, long int chunks_count, const unsigned char *partition_hash, size_t partition_hash_size, unsigned char *chunks, unsigned int packing_sha_1_5_s, int blocks, int threads_per_block)
{
    (void)blocks;
    if (mining_addr == NULL || partition_hash == NULL || chunks == NULL || chunks_count <= 0 ||
        mining_addr_size != 20 || partition_hash_size != 32 || threads_per_block <= 0) {
        return INVALID_ARGUMENTS;
    }
    irys_cuda_packer *p = NULL;
    int device = 0;
    if (cudaGetDevice(&device) != cudaSuccess) return CUDA_ERROR;
    int rc = irys_cuda_packer_new(device, (uint64_t)chunks_count, &p);
    if (rc != 0) {
        printf("CUDA packer allocation for %ld chunks failed: %s\n", chunks_count, cudaGetErrorString((cudaError_t)rc));
        return MEMORY_ALLOCATION_ERROR;
    }
    entropy_chunk_errors result = CUDA_ERROR;
    rc = irys_cuda_packer_start(p, mining_addr, partition_hash, (uint64_t)chain_id, (uint64_t)chunk_offset_start,
                                (uint64_t)chunks_count, packing_sha_1_5_s, threads_per_block);
    if (rc != 0) {
        printf("CUDA packing launch failed: %s\n", cudaGetErrorString((cudaError_t)rc));
        result = CUDA_KERNEL_LAUNCH_FAILED;
    } else {
        for (;;) {
            const unsigned char *bytes;
            uint64_t first, count;
            rc = irys_cuda_packer_next(p, &bytes, &first, &count);
            if (rc == IRYS_CUDA_PACKER_DONE) {
                result = NO_ERROR;
                break;
            }
            if (rc != 0) {
                printf("CUDA packing failed: %s\n", cudaGetErrorString((cudaError_t)rc));
                break;
            }
            memcpy(chunks + first * DATA_CHUNK_SIZE, bytes, count * DATA_CHUNK_SIZE);
        }
    }
    irys_cuda_packer_free(p);
    return result;
}
