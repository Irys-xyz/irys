#include <stddef.h>
#include "types.h"

#pragma once

#if defined(__cplusplus)
extern "C" {
#endif

entropy_chunk_errors compute_entropy_chunks_cuda(const unsigned char *mining_addr, size_t mining_addr_size, unsigned long int chunk_offset_start, unsigned long int chain_id, long int chunks_count, const unsigned char *partition_hash, size_t partition_hash_size, unsigned char *chunks, unsigned int packing_sha_1_5_s, int blocks, int thread_per_blocks);

int get_registers();

// Writes the name of CUDA device `device` into `name` as a NUL-terminated string.
// Returns 0 on success, otherwise a cudaError_t value.
int irys_cuda_device_name(int device, char *name, size_t name_len);

#if defined(__cplusplus)
}
#endif


