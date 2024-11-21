use irys_primitives::Address;
use irys_types::{ChunkBin, CHUNK_SIZE};
use openssl::sha;

pub const SHA_HASH_SIZE: usize = 32;

pub fn compute_seed_hash(
    address: Address,
    offset: std::ffi::c_ulong,
    hash: [u8; SHA_HASH_SIZE],
) -> [u8; SHA_HASH_SIZE] {
    let mut hasher = sha::Sha256::new();
    let address_buffer: [u8; 20] = address.0.into();
    hasher.update(&address_buffer);
    hasher.update(&hash);
    hasher.update(&offset.to_le_bytes());
    hasher.finish()
}

/// 2D Packing
pub fn compute_entropy_chunk(
    mining_address: Address,
    chunk_offset: std::ffi::c_ulong,
    partition_hash: [u8; SHA_HASH_SIZE],
    iterations: u32,
) -> ChunkBin {
    let mut previous_segment = compute_seed_hash(mining_address, chunk_offset, partition_hash);
    let mut entropy_chunk: ChunkBin = [0; CHUNK_SIZE as usize];

    // Phase1: secuential hashing
    for i in 0..(CHUNK_SIZE as usize / SHA_HASH_SIZE) {
        previous_segment = sha::sha256(&previous_segment);
        for j in 0..SHA_HASH_SIZE as usize {
            entropy_chunk[i * SHA_HASH_SIZE + j] = previous_segment[j]
        }
    }

    // Phase2: 2D hash packing
    // Phase1 number hashes
    let mut hash_count = CHUNK_SIZE as usize / SHA_HASH_SIZE;
    while hash_count < iterations as usize {
        let i = (hash_count % (CHUNK_SIZE as usize / SHA_HASH_SIZE)) * SHA_HASH_SIZE;
        let mut hasher = sha::Sha256::new();
        if i == 0 {
            hasher.update(&entropy_chunk[CHUNK_SIZE as usize - SHA_HASH_SIZE..]);
        } else {
            hasher.update(&entropy_chunk[i - 32..i]);
        }
        hasher.update(&entropy_chunk[i..i + SHA_HASH_SIZE]);
        let hash = hasher.finish();
        for j in 0..SHA_HASH_SIZE as usize {
            entropy_chunk[i + j] = hash[j];
        }
        hash_count = hash_count + 1;
    }

    entropy_chunk
}

#[cfg(test)]
mod tests {
    use crate::{
        capacity::{compute_entropy_chunk, compute_seed_hash},
        capacity_single,
        capacity_single::SHA_HASH_SIZE,
    };
    use irys_primitives::Address;
    use irys_types::CHUNK_SIZE;
    use rand;
    use rand::Rng;
    use std::time::Instant;

    #[test]
    fn test_seed_hash() {
        let mut rng = rand::thread_rng();
        let mining_address = Address::random();
        let chunk_offset = rng.gen_range(1..=1000);
        let mut partition_hash = [0u8; SHA_HASH_SIZE];
        rng.fill(&mut partition_hash[..]);

        let rust_hash =
            capacity_single::compute_seed_hash(mining_address, chunk_offset, partition_hash);

        let mut c_hash = Vec::<u8>::with_capacity(SHA_HASH_SIZE);
        let mining_addr_len = mining_address.len(); // note: might not line up with capacity? that should be fine...
        let mining_addr = mining_address.as_ptr() as *const std::os::raw::c_uchar;

        let partition_hash_len = partition_hash.len();
        let partition_hash = partition_hash.as_ptr() as *const std::os::raw::c_uchar;
        let c_hash_ptr = c_hash.as_ptr() as *mut u8;

        unsafe {
            compute_seed_hash(
                mining_addr,
                mining_addr_len,
                chunk_offset,
                partition_hash,
                partition_hash_len,
                c_hash_ptr,
            );
            // we need to move the `len` ptr so rust picks up on the data the C fn wrote to the vec
            c_hash.set_len(c_hash.capacity());
        }

        assert_eq!(rust_hash.to_vec(), c_hash, "Seed hashes should be equal")
    }

    #[test]
    fn test_compute_entropy_chunk() {
        let mut rng = rand::thread_rng();
        let mining_address = Address::random();
        let chunk_offset = rng.gen_range(1..=1000);
        let mut partition_hash = [0u8; SHA_HASH_SIZE];
        rng.fill(&mut partition_hash[..]);
        let iterations = 22_500_000;

        let now = Instant::now();
        let chunk = capacity_single::compute_entropy_chunk(
            mining_address,
            chunk_offset,
            partition_hash,
            iterations,
        );

        let elapsed = now.elapsed();
        println!("Rust implementation: {:.2?}", elapsed);

        let mut c_chunk = Vec::<u8>::with_capacity(CHUNK_SIZE as usize);
        let mining_addr_len = mining_address.len(); // note: might not line up with capacity? that should be fine...
        let mining_addr = mining_address.as_ptr() as *const std::os::raw::c_uchar;

        let partition_hash_len = partition_hash.len();
        let partition_hash = partition_hash.as_ptr() as *const std::os::raw::c_uchar;
        let c_chunk_ptr = c_chunk.as_ptr() as *mut u8;

        let now = Instant::now();

        unsafe {
            compute_entropy_chunk(
                mining_addr,
                mining_addr_len,
                chunk_offset,
                partition_hash,
                partition_hash_len,
                c_chunk_ptr,
                iterations,
            );
            // we need to move the `len` ptr so rust picks up on the data the C fn wrote to the vec
            c_chunk.set_len(c_chunk.capacity());
        }

        let elapsed = now.elapsed();
        println!("C implementation: {:.2?}", elapsed);

        assert_eq!(chunk.to_vec(), c_chunk, "Chunks should be equal")
    }
}
