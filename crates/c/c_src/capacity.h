#include <stddef.h>
#include "types.h"
#pragma once


//#define DEBUG_DUMP_ENTROPY_CHUNKS
//#define BENCHMARK_PARALLEL


// Single
entropy_chunk_errors compute_seed_hash(const unsigned char *mining_addr, size_t mining_addr_size, unsigned long int chunk_offset, unsigned long int chain_id, const unsigned char *partition_hash, size_t partition_hash_size, unsigned char *seed_hash);
entropy_chunk_errors compute_start_entropy_chunk(const unsigned char *mining_addr, size_t mining_addr_size, unsigned long int chunk_offset, unsigned long int chain_id, const unsigned char *partition_hash, size_t partition_hash_size, unsigned char *chunk);
entropy_chunk_errors compute_start_entropy_chunk2(const unsigned char *previous_segment, size_t previous_segment_len, unsigned char *chunk);
entropy_chunk_errors compute_entropy_chunk(const unsigned char *mining_addr, size_t mining_addr_size, unsigned long int chunk_offset, unsigned long int chain_id, const unsigned char *partition_hash, size_t partition_hash_size, unsigned char *entropy_chunk, unsigned int packing_sha_1_5_s);
entropy_chunk_errors compute_entropy_chunk2(const unsigned char *segment, const unsigned char *entropy_chunk, unsigned char *new_entropy_chunk, unsigned int packing_sha_1_5_s);
