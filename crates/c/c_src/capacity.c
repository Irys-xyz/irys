#include <openssl/evp.h>
#include <string.h>
#include <stdlib.h>
#include <sys/time.h>
#include "erl_nif.h"

#include "capacity.h"

// Function prototypes
static ERL_NIF_TERM compute_entropy_chunk_nif(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]);
static ERL_NIF_TERM compute_chunks_parallel_nif(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]);

// Helper functions to convert entropy_chunk_errors to atoms
static ERL_NIF_TERM entropy_chunk_error_to_atom(ErlNifEnv* env, entropy_chunk_errors error) {
    switch (error) {
        case NO_ERROR:
            return enif_make_atom(env, "ok");
        case PARTITION_HASH_ERROR:
            return enif_make_atom(env, "partition_hash_error");
        case SEED_HASH_ERROR:
            return enif_make_atom(env, "seed_hash_error");
        case MEMORY_ALLOCATION_ERROR:
            return enif_make_atom(env, "memory_allocation_error");
        case HASH_COMPUTATION_ERROR:
            return enif_make_atom(env, "hash_computation_error");
        case INVALID_ARGUMENTS:
            return enif_make_atom(env, "invalid_arguments");
        case CUDA_ERROR:
            return enif_make_atom(env, "cuda_error");
        case CUDA_KERNEL_LAUNCH_FAILED:
            return enif_make_atom(env, "cuda_kernel_launch_failed");
        case HIP_ERROR:
            return enif_make_atom(env, "hip_error");
        case HIP_KERNEL_LAUNCH_FAILED:
            return enif_make_atom(env, "hip_kernel_launch_failed");
        default:
            return enif_make_atom(env, "unknown_error");
    }
}

/*! @brief NIF function to compute the capacity chunk
 *
 * This function is a NIF wrapper around the compute_entropy_chunk function.
 * It takes four arguments: the mining address (bin), the chunk offset (integer), the partition ID (bin) and packing sha parameter.
 * It returns a tuple with the first element being an atom indicating the result status and the second element being the
 * capacity chunk.
 *
 * @param env The Erlang environment
 * @param argc The number of arguments
 * @param argv The arguments
 * @return The result tuple
 */
static ERL_NIF_TERM compute_entropy_chunk_nif(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifBinary mining_addr, partition;
    long int chunk_offset;
    unsigned int packing_sha_1_5_s;

    if (!enif_inspect_binary(env, argv[0], &mining_addr) ||
        !enif_get_long(env, argv[1], &chunk_offset) ||
        !enif_inspect_binary(env, argv[2], &partition) ||
        !enif_get_uint(env, argv[3], &packing_sha_1_5_s)) {
        return enif_make_badarg(env);
    }

    if (mining_addr.size == 0 || partition.size == 0 || chunk_offset < 0) {
        return enif_make_badarg(env);
    }

    ErlNifBinary entropy_chunk;
    if (!enif_alloc_binary(DATA_CHUNK_SIZE, &entropy_chunk)) {
        return enif_make_tuple2(env, enif_make_atom(env, "error"), entropy_chunk_error_to_atom(env, MEMORY_ALLOCATION_ERROR));
    }

    entropy_chunk.size = DATA_CHUNK_SIZE;
    entropy_chunk_errors error = compute_entropy_chunk(mining_addr.data, mining_addr.size, chunk_offset, partition.data, partition.size, entropy_chunk.data, packing_sha_1_5_s);
    if (error != NO_ERROR) {
        enif_release_binary(&entropy_chunk);
        return enif_make_tuple2(env, enif_make_atom(env, "error"), entropy_chunk_error_to_atom(env, error));
    }
    ERL_NIF_TERM result = enif_make_binary(env, &entropy_chunk);
    return enif_make_tuple2(env, enif_make_atom(env, "ok"), result);
}

/*! @brief NIF function to compute the capacity chunks
 *
 * This function is a NIF for calculating many chunks in parallel.
 * The function takes five arguments:
 * 1) the mining address (bin),
 * 2) the partition ID (bin)
 * 3) the chunk offset (integer) - start,
 * 4) how many chunks to calculate (integer),
 * 5) packing sha parameter.
 * It returns a tuple with the first element being an atom indicating the result status and the second element being the
 * binary of generated capacity chunks.
 *
 * @param env The Erlang environment
 * @param argc The number of arguments
 * @param argv The arguments
 * @return The result tuple
 */
static ERL_NIF_TERM compute_chunks_parallel_nif(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
#ifdef BENCHMARK_PARALLEL
    //Timer start using gettimeofday
    struct timeval start, end;
    gettimeofday(&start, NULL);
#endif

    unsigned int packing_sha_1_5_s;
    ErlNifBinary mining_addr, partition;
    unsigned long int chunk_offset_start;
    long int chunk_count;

    if (!enif_inspect_binary(env, argv[0], &mining_addr) ||
        !enif_inspect_binary(env, argv[1], &partition) ||
        !enif_get_ulong(env, argv[2], &chunk_offset_start) ||
        !enif_get_long(env, argv[3], &chunk_count) ||
        !enif_get_uint(env, argv[4], &packing_sha_1_5_s)) {
        return enif_make_badarg(env);
    }

    if (mining_addr.size == 0 || partition.size == 0) {
        return enif_make_badarg(env);
    }

    ErlNifBinary entropy_chunks;
    if (!enif_alloc_binary(DATA_CHUNK_SIZE * chunk_count, &entropy_chunks)) {
        return enif_make_tuple2(env, enif_make_atom(env, "error"), entropy_chunk_error_to_atom(env, MEMORY_ALLOCATION_ERROR));
    }

    entropy_chunks.size = DATA_CHUNK_SIZE * chunk_count;
    entropy_chunk_errors error = NO_ERROR;

#ifdef BENCHMARK_PARALLEL
    // Elapsed
    gettimeofday(&end, NULL);
    double elapsed = (end.tv_sec - start.tv_sec) + ((end.tv_usec - start.tv_usec) / 1000000.0);
    printf("Parallel Benchmark: Elapsed time (before parallel call): %f\n", elapsed);
#endif

#if defined(CAP_IMPL_CUDA)
    error = compute_entropy_chunks_cuda(mining_addr.data, mining_addr.size, chunk_count, partition.data, partition.size, entropy_chunks.data, packing_sha_1_5_s);
#elif defined(CAP_IMPL_HIP)
    error = compute_entropy_chunks_hip(mining_addr.data, mining_addr.size, chunk_offset_start, chunk_count, partition.data, partition.size, entropy_chunks.data, packing_sha_1_5_s);
#else
    //TODO: check how many CPU cores we have and run several threads.
    // Right now, counting sequentially for CPU
    unsigned int i = 0;
    unsigned long int chunk_offset = chunk_offset_start;
    for (i = 0; i < chunk_count; i++) {
        chunk_offset = chunk_offset_start + i * DATA_CHUNK_SIZE;
        error = compute_entropy_chunk(mining_addr.data, mining_addr.size, chunk_offset, partition.data, partition.size, entropy_chunks.data + i * DATA_CHUNK_SIZE, packing_sha_1_5_s);
        if (error != NO_ERROR)
            break;                  // Break on first error.
    }
#endif

#ifdef DEBUG_DUMP_ENTROPY_CHUNKS
    // Dump entropy_chunks.data to file for debugging
    FILE *f = fopen("entropy_chunks.data", "wb");
    fwrite(entropy_chunks.data, 1, entropy_chunks.size, f);
    fclose(f);
#endif

#ifdef BENCHMARK_PARALLEL
    // Elapsed
    gettimeofday(&end, NULL);
    elapsed = (end.tv_sec - start.tv_sec) + ((end.tv_usec - start.tv_usec) / 1000000.0);
    printf("Parallel Benchmark: Elapsed time (after parallel call): %f\n", elapsed);
#endif

    if (error != NO_ERROR) {
        enif_release_binary(&entropy_chunks);
        return enif_make_tuple2(env, enif_make_atom(env, "error"), entropy_chunk_error_to_atom(env, error));
    }

    // Release the binary - enif_make_binary should take the ownership so we don't need to release it.
    ERL_NIF_TERM result = enif_make_binary(env, &entropy_chunks);

#ifdef BENCHMARK_PARALLEL
    //Timer end
    gettimeofday(&end, NULL);
    elapsed = (end.tv_sec - start.tv_sec) + ((end.tv_usec - start.tv_usec) / 1000000.0);
    printf("Parallel Benchmark: Elapsed time (full): %f\n", elapsed);
#endif

    return enif_make_tuple2(env, enif_make_atom(env, "ok"), result);
}

ErlNifFunc nif_funcs[] = {
        {"compute_entropy_chunk_nif", 4, compute_entropy_chunk_nif, ERL_NIF_DIRTY_JOB_CPU_BOUND},
        {"compute_chunks_parallel_nif", 5, compute_chunks_parallel_nif, ERL_NIF_DIRTY_JOB_CPU_BOUND}
};

ERL_NIF_INIT(ar_capacity_packing, nif_funcs, NULL, NULL, NULL, NULL)
