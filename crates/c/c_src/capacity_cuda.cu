#include <openssl/evp.h>
#include <string.h>
#include <stdlib.h>
#include <cuda.h>
#include <stdint.h>

#include "types.h"
#include "sha256.cuh"

// Max threads - this is the maximum number of threads per block for the GPU
// This is a constant defined by the GPU architecture.
#define THREADS_PER_BLOCK 3 // TODO @ernius: how to correctly setup this parameter ?
#define NUM_HASHES (DATA_CHUNK_SIZE / PACKING_HASH_SIZE)
#define INPUT_SIZE PACKING_HASH_SIZE

#define SHA256_DIGEST_LENGTH   PACKING_HASH_SIZE

/**
 * Computes the seed hash for the given chunk.
 * @param chunk_id The chunk id
 * @param chunk_id_length The length of the chunk id
 * @param seed_hash The output seed hash
 */
__device__ void compute_seed_hash_cuda(const unsigned char *chunk_id, size_t chunk_id_length, unsigned char *seed_hash) {
    SHA256_CTX sha256;
    sha256_init(&sha256);
    sha256_update(&sha256, chunk_id, chunk_id_length);
    sha256_final(&sha256, seed_hash);
}

/**
 * Computes the start entropy chunk for the given chunk.
 * @param previous_segment The previous segment
 * @param previous_segment_len The length of the previous segment
 * @param chunk The output chunk
 */
__device__ void compute_start_entropy_chunk2_cuda(const unsigned char *previous_segment, size_t previous_segment_len, unsigned char *chunk) {
    size_t chunk_len = 0;
    SHA256_CTX sha256;

    while (chunk_len < DATA_CHUNK_SIZE) {
        sha256_init(&sha256);
        sha256_update(&sha256, previous_segment, previous_segment_len);
        sha256_final(&sha256, chunk + chunk_len);
        previous_segment = chunk + chunk_len;
        chunk_len += previous_segment_len;
    }
}

/**
 * Computes the start entropy chunk for the given chunk.
 * @param chunk_id The chunk id
 * @param chunk_id_length The length of the chunk id
 * @param chunk The output chunk
 */
__device__ void compute_start_entropy_chunk_cuda(const unsigned char *chunk_id, size_t chunk_id_length, unsigned char *chunk) {
    unsigned char seed_hash[PACKING_HASH_SIZE];

    compute_seed_hash_cuda(chunk_id, chunk_id_length, seed_hash);
    compute_start_entropy_chunk2_cuda(seed_hash, PACKING_HASH_SIZE, chunk);
}

/**
 * Computes the entropy chunk for the given chunk.
 * @param segment The segment
 * @param entropy_chunk The entropy chunk
 * @param new_entropy_chunk The new entropy chunk
 * @param packing_sha_1_5_s The number of iterations
 */
__device__ void compute_entropy_chunk2_cuda(const unsigned char *segment, unsigned char *entropy_chunk, unsigned int packing_sha_1_5_s) {
    SHA256_CTX sha256;

    for (int hash_count = HASH_ITERATIONS_PER_BLOCK; hash_count < packing_sha_1_5_s; ++hash_count) {
        size_t start_offset = (hash_count % HASH_ITERATIONS_PER_BLOCK) * PACKING_HASH_SIZE;

        sha256_init(&sha256);
        sha256_update(&sha256, segment, SHA256_DIGEST_LENGTH);
        if (hash_count / HASH_ITERATIONS_PER_BLOCK < 2)
            sha256_update(&sha256, entropy_chunk + start_offset, SHA256_DIGEST_LENGTH);
        else 
            sha256_update(&sha256, entropy_chunk + start_offset, SHA256_DIGEST_LENGTH);
        sha256_final(&sha256, entropy_chunk + start_offset);
        segment = entropy_chunk + start_offset;
    }
}

/**
 * Computes the entropy chunk for the given chunk.
 * @param chunk_id The chunk id
 * @param chunk_id_length The length of the chunk id
 * @param entropy_chunk The entropy chunk
 * @param chunk_1 First layer of the chunk
 * @param packing_sha_1_5_s The number of iterations
 */
__device__ void compute_entropy_chunk_cuda(const unsigned char *chunk_id, size_t chunk_id_length, unsigned char *entropy_chunk, unsigned int packing_sha_1_5_s) {

    const int partial_entropy_chunk_size = (HASH_ITERATIONS_PER_BLOCK - 1) * PACKING_HASH_SIZE;

    compute_start_entropy_chunk_cuda(chunk_id, chunk_id_length, entropy_chunk);

    unsigned char last_entropy_chunk_segment[PACKING_HASH_SIZE];
    memcpy(last_entropy_chunk_segment, entropy_chunk + partial_entropy_chunk_size, PACKING_HASH_SIZE);

    compute_entropy_chunk2_cuda(last_entropy_chunk_segment, entropy_chunk, packing_sha_1_5_s);
}

/**
 * Computes the entropy chunks for the given list of chunks.
 * The entropy chunks are computed in parallel using the GPU.
 */
__global__ void compute_entropy_chunks_cuda_kernel(unsigned char *chunk_id, unsigned long int chunk_offset_start, long int chunks_count, unsigned char *chunks, unsigned int packing_sha_1_5_s) {
    // Get the index of the current thread - as we are using a 1D grid, we only need to get the index of the current block.
    // The index of the current thread is then the index of the block times the number of threads per block plus the index of the current thread in the block.
    int idx = blockIdx.x * blockDim.x + threadIdx.x;
    if (idx < chunks_count) {
        unsigned char *output = chunks + idx * DATA_CHUNK_SIZE;
        unsigned char chunk_id_thread[CHUNK_ID_LEN];
        memcpy(chunk_id_thread, chunk_id, CHUNK_ID_LEN - sizeof(uint64_t));
        *((uint32_t*)&chunk_id_thread[CHUNK_ID_LEN - sizeof(uint64_t)]) = chunk_offset_start + idx;
        compute_entropy_chunk_cuda(chunk_id_thread, CHUNK_ID_LEN, output, packing_sha_1_5_s);
    }
}

/**
 * Computes the entropy chunks for the given list of chunks.
 * The entropy chunks are computed in parallel using the GPU.
 */
extern "C" entropy_chunk_errors compute_entropy_chunks_cuda(const unsigned char *mining_addr, size_t mining_addr_size, unsigned long int chunk_offset_start, unsigned long int chain_id, long int chunks_count, const unsigned char *partition_hash, size_t partition_hash_size, unsigned char *chunks, unsigned int packing_sha_1_5_s)
{
    unsigned char *d_chunks;
    unsigned char *d_chunk_id;

    // We need to allocate memory for each layer here, as kernel/device functions can't allocate any
    // additional memory, so we need to prepare it for them.
    if (cudaMalloc(&d_chunks, DATA_CHUNK_SIZE * chunks_count) != cudaSuccess) {
        return CUDA_ERROR;
    }

    if (cudaMalloc(&d_chunk_id, CHUNK_ID_LEN) != cudaSuccess) {
        return CUDA_ERROR;
    }

    if (cudaMemcpy(d_chunk_id, mining_addr, mining_addr_size, cudaMemcpyHostToDevice) != cudaSuccess) {
        return CUDA_ERROR;
    }

    if (cudaMemcpy(d_chunk_id + mining_addr_size, partition_hash, partition_hash_size, cudaMemcpyHostToDevice) != cudaSuccess) {
        return CUDA_ERROR;
    }

    if (cudaMemcpy(d_chunk_id + mining_addr_size + partition_hash_size, &chain_id, sizeof(uint64_t), cudaMemcpyHostToDevice) != cudaSuccess) {
        return CUDA_ERROR;
    }

    // Launch kernel
    int blocks = (chunks_count + THREADS_PER_BLOCK - 1) / THREADS_PER_BLOCK;
    compute_entropy_chunks_cuda_kernel<<<blocks, THREADS_PER_BLOCK>>>(d_chunk_id, chunk_offset_start, chunks_count, d_chunks, packing_sha_1_5_s);

    // Check for errors after kernel launch
    cudaError_t err = cudaGetLastError();
    if (err != cudaSuccess) {
        return CUDA_KERNEL_LAUNCH_FAILED;
    }

    cudaError_t errm = cudaMemcpy(chunks, d_chunks, DATA_CHUNK_SIZE * chunks_count, cudaMemcpyDeviceToHost);
    if (errm != cudaSuccess) {
        return CUDA_ERROR;
    }

    cudaFree(d_chunks);
    cudaFree(d_chunk_id);

    return NO_ERROR;
}