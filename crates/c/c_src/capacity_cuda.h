#include <stddef.h>
#include <stdint.h>
#include "types.h"

#pragma once

#if defined(__cplusplus)
extern "C" {
#endif

// Chunks per output slab of an `irys_cuda_packer` (128 MiB at 256 KiB per chunk).
#define IRYS_CUDA_SLAB_CHUNKS 512

// `irys_cuda_packer_next` return value when the batch has been handed out completely.
#define IRYS_CUDA_PACKER_DONE (-1)

// A reusable CUDA entropy packer for one device: a stream, a device working buffer for up to
// `capacity` chunks, and two pinned host slabs of IRYS_CUDA_SLAB_CHUNKS chunks. Not thread-safe;
// use one per concurrent batch.
typedef struct irys_cuda_packer irys_cuda_packer;

// Creates a packer for batches of up to `capacity` chunks on `device`. Returns 0 on success,
// otherwise a cudaError_t value.
int irys_cuda_packer_new(int device, uint64_t capacity, irys_cuda_packer **out);

// Waits for queued work and frees the packer. `p` may be NULL.
void irys_cuda_packer_free(irys_cuda_packer *p);

// Queues the packing of chunks [chunk_offset_start, chunk_offset_start + chunks_count) and returns
// without waiting. `mining_addr` is 20 bytes, `partition_hash` 32. Returns 0 or a cudaError_t value.
int irys_cuda_packer_start(irys_cuda_packer *p, const unsigned char *mining_addr,
                           const unsigned char *partition_hash, uint64_t chain_id,
                           uint64_t chunk_offset_start, uint64_t chunks_count, uint32_t iterations,
                           int threads_per_block);

// Waits for the next slab of the batch started last and lends it to the caller: `*bytes` holds
// `*chunk_count` contiguous chunks starting at chunk `*first_chunk` of the batch, valid until the
// next call. Returns 0, IRYS_CUDA_PACKER_DONE after the last slab, or a cudaError_t value.
int irys_cuda_packer_next(irys_cuda_packer *p, const unsigned char **bytes, uint64_t *first_chunk,
                          uint64_t *chunk_count);

entropy_chunk_errors compute_entropy_chunks_cuda(const unsigned char *mining_addr, size_t mining_addr_size, unsigned long int chunk_offset_start, unsigned long int chain_id, long int chunks_count, const unsigned char *partition_hash, size_t partition_hash_size, unsigned char *chunks, unsigned int packing_sha_1_5_s, int blocks, int thread_per_blocks);

int get_registers();

// Human-readable message for a cudaError_t value (static string).
const char *irys_cuda_error_string(int err);

// Writes the name of CUDA device `device` into `name` as a NUL-terminated string.
// Returns 0 on success, otherwise a cudaError_t value.
int irys_cuda_device_name(int device, char *name, size_t name_len);

// Free and total memory of CUDA device `device`, in bytes; the calling thread's current device
// is restored afterwards. Returns 0 on success, otherwise a cudaError_t value.
int irys_cuda_mem_info(int device, size_t *free_bytes, size_t *total_bytes);

#if defined(__cplusplus)
}
#endif
