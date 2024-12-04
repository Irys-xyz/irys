use std::ops::BitXor;

pub use irys_c::{capacity, capacity_single};
use irys_types::{partition::PartitionHash, Address, ChunkBytes, CHUNK_SIZE, PACKING_SHA_1_5_S};

/// Performs the entropy packing for the specified chunk offset, partition, and mining address
/// defaults to [`PACKING_SHA_1_5_S`]`, returns entropy chunk in out_entropy_chunk parameter.
/// Precondition: `out_entropy_chunk` should have at least DATA_CHUNK_SIZE = 256KB (definded in capacity.h file) capacity
/// Uses C 2D Packing implementation
pub fn capacity_pack_range_c(
    mining_address: Address,
    chunk_offset: std::ffi::c_ulong,
    partition_hash: PartitionHash,
    iterations: Option<u32>,
    out_entropy_chunk: &mut Vec<u8>,
) {
    let mining_addr_len = mining_address.len(); // note: might not line up with capacity? that should be fine...
    let partition_hash_len = partition_hash.0.len();

    let mining_addr = mining_address.as_ptr() as *const std::os::raw::c_uchar;
    let partition_hash = partition_hash.as_ptr() as *const std::os::raw::c_uchar;
    let entropy_chunk_ptr = out_entropy_chunk.as_ptr() as *mut u8;

    let iterations: u32 = iterations.unwrap_or(PACKING_SHA_1_5_S);

    unsafe {
        capacity::compute_entropy_chunk(
            mining_addr,
            mining_addr_len,
            chunk_offset,
            partition_hash,
            partition_hash_len,
            entropy_chunk_ptr,
            iterations,
        );
        // we need to move the `len` ptr so rust picks up on the data the C fn wrote to the vec
        out_entropy_chunk.set_len(out_entropy_chunk.capacity());
    }
}

enum PackingType {
    CPU,
    CUDA,
    AMD,
}

const PACKING_TYPE: PackingType = PackingType::CPU;

/// 2D Packing Rust implementation
pub fn capacity_pack_range_with_data(
    data: &mut Vec<ChunkBytes>,
    mining_address: Address,
    chunk_offset: std::ffi::c_ulong,
    partition_hash: PartitionHash,
    iterations: Option<u32>,
    chunk_size: usize,
) {
    let iterations: u32 = iterations.unwrap_or(PACKING_SHA_1_5_S);

    match PACKING_TYPE {
        PackingType::CPU => {
            let mut entropy_chunk = Vec::<u8>::with_capacity(chunk_size);
            data.iter_mut().enumerate().for_each(|(pos, mut chunk)| {
                capacity_single::compute_entropy_chunk(
                    mining_address,
                    chunk_offset + pos as u64 * chunk_size as u64,
                    partition_hash.0.into(),
                    iterations,
                    chunk_size,
                    &mut entropy_chunk,
                );
                xor_vec_u8_arrays_in_place(&mut chunk, &entropy_chunk);
            })
        }
        _ => unimplemented!(),
    }
}

/// 2D Packing C implementation
pub fn capacity_pack_range_with_data_c(
    data: &mut Vec<ChunkBytes>,
    mining_address: Address,
    chunk_offset: std::ffi::c_ulong,
    partition_hash: PartitionHash,
    iterations: Option<u32>,
) {
    match PACKING_TYPE {
        PackingType::CPU => {
            let mut entropy_chunk = Vec::<u8>::with_capacity(CHUNK_SIZE as usize);
            data.iter_mut().enumerate().for_each(|(pos, mut chunk)| {
                capacity_pack_range_c(
                    mining_address,
                    chunk_offset + pos as u64 * CHUNK_SIZE,
                    partition_hash,
                    iterations,
                    &mut entropy_chunk,
                );
                xor_vec_u8_arrays_in_place(&mut chunk, &entropy_chunk);
            })
        }
        _ => unimplemented!(),
    }
}

#[inline]
pub fn xor_vec_u8_arrays_in_place(a: &mut Vec<u8>, b: &Vec<u8>) {
    for i in 0..a.len() {
        a[i] = a[i].bitxor(b[i]);
    }
}

#[test]
fn test_compute_entropy_chunk() {
    use irys_c::capacity_single::SHA_HASH_SIZE;
    use rand;
    use rand::Rng;
    use std::time::Instant;

    let mut rng = rand::thread_rng();
    let mining_address = Address::random();
    let chunk_offset = rng.gen_range(1..=1000);
    let mut partition_hash = [0u8; SHA_HASH_SIZE];
    rng.fill(&mut partition_hash[..]);
    let iterations = 2 * CHUNK_SIZE as u32;

    let mut chunk: Vec<u8> = Vec::<u8>::with_capacity(CHUNK_SIZE as usize);

    let now = Instant::now();

    capacity_single::compute_entropy_chunk(
        mining_address,
        chunk_offset,
        partition_hash,
        iterations,
        CHUNK_SIZE as usize,
        &mut chunk,
    );

    let elapsed = now.elapsed();
    println!("Rust implementation: {:.2?}", elapsed);

    let mut c_chunk = Vec::<u8>::with_capacity(CHUNK_SIZE as usize);
    let now = Instant::now();

    capacity_pack_range_c(
        mining_address,
        chunk_offset,
        partition_hash.into(),
        Some(iterations),
        &mut c_chunk,
    );

    let elapsed = now.elapsed();
    println!("C implementation: {:.2?}", elapsed);

    assert_eq!(chunk, c_chunk, "Chunks should be equal")
}

#[test]
fn test_chunks_packing() {
    use irys_c::capacity_single::SHA_HASH_SIZE;
    use rand;
    use rand::{Rng, RngCore};
    use std::time::Instant;

    let mut rng = rand::thread_rng();
    let mining_address = Address::random();
    let chunk_offset = rng.gen_range(1..=1000);
    let mut partition_hash: [u8; SHA_HASH_SIZE] = [0; SHA_HASH_SIZE];
    rng.fill(&mut partition_hash);

    let num_chunks: usize = 4;
    let mut chunks: Vec<ChunkBytes> = Vec::with_capacity(num_chunks);
    let mut chunks_rust: Vec<ChunkBytes> = Vec::with_capacity(num_chunks);

    for _i in 0..num_chunks {
        let mut chunk = [0u8; CHUNK_SIZE as usize];
        rng.fill_bytes(&mut chunk);
        chunks.push(chunk.to_vec());
        chunks_rust.push(chunk.to_vec());
    }

    assert_eq!(chunks, chunks_rust, "Rust and C packing should start equal");

    // pick random chunk to verify later
    let rnd_chunk_pos = rng.gen_range(0..num_chunks);
    let mut rnd_chunk = chunks[rnd_chunk_pos].clone();

    let now = Instant::now();

    capacity_pack_range_with_data_c(
        &mut chunks,
        mining_address,
        chunk_offset,
        partition_hash.into(),
        Some(2 * CHUNK_SIZE as u32),
    );

    let elapsed = now.elapsed();
    println!("C implementation: {:.2?}", elapsed);

    let now = Instant::now();

    capacity_pack_range_with_data(
        &mut chunks_rust,
        mining_address,
        chunk_offset,
        partition_hash.into(),
        Some(2 * CHUNK_SIZE as u32),
        CHUNK_SIZE as usize,
    );

    let elapsed = now.elapsed();
    println!("Rust implementation: {:.2?}", elapsed);

    assert_eq!(chunks, chunks_rust, "Rust and C packing should be equal");

    // calculate entropy for choosen random chunk
    let mut entropy_chunk = Vec::<u8>::with_capacity(CHUNK_SIZE.try_into().unwrap());
    capacity_pack_range_c(
        mining_address,
        chunk_offset + rnd_chunk_pos as u64 * CHUNK_SIZE,
        partition_hash.into(),
        Some(2 * CHUNK_SIZE as u32),
        &mut entropy_chunk,
    );

    // sign picked random chunk with entropy
    xor_vec_u8_arrays_in_place(&mut rnd_chunk, &entropy_chunk);

    assert_eq!(chunks[rnd_chunk_pos], rnd_chunk, "Wrong packed chunk")
}
