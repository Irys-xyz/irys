#include <openssl/evp.h>
#include <string.h>
#include <stdlib.h>
#include <stdint.h>

#include "capacity.h"

entropy_chunk_errors compute_seed_hash(const unsigned char *mining_addr, size_t mining_addr_size, unsigned long int chunk_offset,  unsigned long int chain_id, const unsigned char *partition_hash, size_t partition_hash_size, unsigned char *seed_hash) {
    int input_len = mining_addr_size + partition_hash_size + sizeof(uint64_t) + sizeof(uint64_t);
    uint64_t chunk_offset_u64 = (uint64_t) chunk_offset;
    uint64_t chain_id_u64 = (uint64_t) chain_id;

    // Compute the hash
    EVP_MD_CTX *mdctx = EVP_MD_CTX_new();
    if (!mdctx) {
        return SEED_HASH_ERROR;
    }

    unsigned int hash_len;
    EVP_DigestInit_ex(mdctx, PACKING_HASH_ALG, NULL);
    EVP_DigestUpdate(mdctx, mining_addr, mining_addr_size);
    EVP_DigestUpdate(mdctx, partition_hash, partition_hash_size);
    EVP_DigestUpdate(mdctx, &chain_id_u64, sizeof(uint64_t));            
    EVP_DigestUpdate(mdctx, &chunk_offset_u64, sizeof(uint64_t));            
    EVP_DigestFinal_ex(mdctx, seed_hash, &hash_len);
    EVP_MD_CTX_free(mdctx);

    return NO_ERROR;
}

entropy_chunk_errors compute_start_entropy_chunk(const unsigned char *mining_addr, size_t mining_addr_size, unsigned long int chunk_offset, unsigned long int chain_id, const unsigned char *partition_hash, size_t partition_hash_size, unsigned char *chunk) {
    size_t hash_size = EVP_MD_size(EVP_sha256());
    unsigned char seed_hash[hash_size];

    entropy_chunk_errors error = compute_seed_hash(mining_addr, mining_addr_size, chunk_offset, chain_id, partition_hash, partition_hash_size, seed_hash);
    if (error != NO_ERROR) {
        return error;
    }

    return compute_start_entropy_chunk2(seed_hash, hash_size, chunk);
}

entropy_chunk_errors compute_start_entropy_chunk2(const unsigned char *previous_segment, size_t previous_segment_len, unsigned char *chunk) {
    size_t chunk_len = 0;
    unsigned int segment_len;

    EVP_MD_CTX *mdctx = EVP_MD_CTX_new();
    if (!mdctx) {
        return HASH_COMPUTATION_ERROR;
    }

    while (chunk_len < DATA_CHUNK_SIZE) {
        //printf("compute_start_entropy_chunk2 chunk_len %d\n", chunk_len);        
        EVP_DigestInit_ex(mdctx, PACKING_HASH_ALG, NULL);
        EVP_DigestUpdate(mdctx, previous_segment, previous_segment_len);
        EVP_DigestFinal_ex(mdctx, chunk + chunk_len, &segment_len);
        previous_segment = chunk + chunk_len;
        previous_segment_len = segment_len;
        chunk_len += segment_len;
    }

    EVP_MD_CTX_free(mdctx);
    return NO_ERROR;
}

entropy_chunk_errors compute_entropy_chunk(const unsigned char *mining_addr, size_t mining_addr_size, unsigned long int chunk_offset, unsigned long int chain_id, const unsigned char *partition_hash, size_t partition_hash_size, unsigned char *entropy_chunk, unsigned int packing_sha_1_5_s) {
    int partial_entropy_chunk_size = (HASH_ITERATIONS_PER_BLOCK - 1) * PACKING_HASH_SIZE;
    unsigned char *start_entropy_chunk = (unsigned char *) malloc(DATA_CHUNK_SIZE);
    if (!start_entropy_chunk) {
        return MEMORY_ALLOCATION_ERROR;
    }

    entropy_chunk_errors error = compute_start_entropy_chunk(mining_addr, mining_addr_size, chunk_offset, chain_id, partition_hash, partition_hash_size, start_entropy_chunk);
    if (error != NO_ERROR) {
        free(start_entropy_chunk);
        return error;
    }

    unsigned char last_entropy_chunk_segment[PACKING_HASH_SIZE];
    memcpy(last_entropy_chunk_segment, start_entropy_chunk + partial_entropy_chunk_size, PACKING_HASH_SIZE);

    error = compute_entropy_chunk2(last_entropy_chunk_segment, start_entropy_chunk, entropy_chunk, packing_sha_1_5_s);
    free(start_entropy_chunk);

    return error;
}

entropy_chunk_errors compute_entropy_chunk2(const unsigned char *segment, const unsigned char *entropy_chunk, unsigned char *new_entropy_chunk, unsigned int packing_sha_1_5_s) {
    memcpy(new_entropy_chunk, entropy_chunk, DATA_CHUNK_SIZE);
    unsigned int segment_hash_len;
    size_t segment_len = PACKING_HASH_SIZE;

    EVP_MD_CTX *mdctx = EVP_MD_CTX_new();
    if (!mdctx) {
        return HASH_COMPUTATION_ERROR;
    }

    for (int hash_count = HASH_ITERATIONS_PER_BLOCK; hash_count < packing_sha_1_5_s; ++hash_count) {
        //printf("compute_entropy_chunk2 hash count %d iterations %d\n", hash_count, packing_sha_1_5_s);
        size_t start_offset = (hash_count % HASH_ITERATIONS_PER_BLOCK) * PACKING_HASH_SIZE;

        EVP_DigestInit_ex(mdctx, PACKING_HASH_ALG, NULL);
        EVP_DigestUpdate(mdctx, segment, segment_len);
        if (hash_count / HASH_ITERATIONS_PER_BLOCK < 2)
          EVP_DigestUpdate(mdctx, entropy_chunk + start_offset, PACKING_HASH_SIZE);
        else
          EVP_DigestUpdate(mdctx, new_entropy_chunk + start_offset, PACKING_HASH_SIZE);
        EVP_DigestFinal_ex(mdctx, new_entropy_chunk + start_offset, &segment_hash_len);

        segment = new_entropy_chunk + start_offset;
        segment_len = segment_hash_len;
    }

    EVP_MD_CTX_free(mdctx);

    return NO_ERROR;
}
