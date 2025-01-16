#include <stddef.h>
#include "types.h"

#pragma once

#if defined(__cplusplus)
extern "C" {
#endif

entropy_chunk_errors compute_entropy_chunks_cuda(const unsigned char *mining_addr, size_t mining_addr_size, unsigned long int chunk_offset_start, long int chunks_count, const unsigned char *partition_hash, size_t partition_hash_size, unsigned char *chunks, unsigned int packing_sha_1_5_s);

#if defined(__cplusplus)
}
#endif


