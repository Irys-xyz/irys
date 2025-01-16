#pragma once

#define PACKING_HASH_ALG EVP_sha256()
#define HASH_ITERATIONS_PER_BLOCK 8192
#define PACKING_HASH_SIZE 32
#define DATA_CHUNK_SIZE (HASH_ITERATIONS_PER_BLOCK * PACKING_HASH_SIZE)

// Length of the chunk ID - mining address + partition ID + chunk offset (8 bytes)
#define CHUNK_ID_LEN 60

// Define entropy_chunk_errors as an enumeration
typedef enum {
    NO_ERROR,
    PARTITION_HASH_ERROR,
    SEED_HASH_ERROR,
    MEMORY_ALLOCATION_ERROR,
    HASH_COMPUTATION_ERROR,
    INVALID_ARGUMENTS,
    CUDA_ERROR,
    CUDA_KERNEL_LAUNCH_FAILED,
    HIP_ERROR,
    HIP_KERNEL_LAUNCH_FAILED,
} entropy_chunk_errors;
