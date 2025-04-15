use irys_primitives::Address;
use openssl::sha;

pub const SHA_HASH_SIZE: usize = 32;

#[inline]
pub fn compute_seed_hash(
    address: Address,
    offset: std::ffi::c_ulong,
    hash: [u8; SHA_HASH_SIZE],
    irys_chain_id: u64,
) -> [u8; SHA_HASH_SIZE] {
    let mut hasher = sha::Sha256::new();
    let address_buffer: [u8; 20] = address.0.into();
    hasher.update(&address_buffer);
    hasher.update(&hash);
    hasher.update(&irys_chain_id.to_le_bytes());
    hasher.update(&offset.to_le_bytes());
    hasher.finish()
}

/// Performs the entropy packing for the specified chunk offset, partition, and mining address
/// defaults to `[CONFIG.packing_sha_1_5_S]`, returns entropy chunk in `out_entropy_chunk` parameter.
/// Precondition: `out_entropy_chunk` should have at least `chunk_size` capacity
#[inline]
pub fn compute_entropy_chunk(
    mining_address: Address,
    chunk_offset: std::ffi::c_ulong,
    partition_hash: [u8; SHA_HASH_SIZE],
    iterations: u32,
    chunk_size: usize,
    out_entropy_chunk: &mut Vec<u8>,
    irys_chain_id: u64,
) {
    let mut previous_segment =
        compute_seed_hash(mining_address, chunk_offset, partition_hash, irys_chain_id);
    out_entropy_chunk.clear();
    // Phase 1: sequential hashing
    for _i in 0..(chunk_size / SHA_HASH_SIZE) {
        previous_segment = sha::sha256(&previous_segment);
        for j in 0..SHA_HASH_SIZE {
            out_entropy_chunk.push(previous_segment[j]); // inserting in [i * SHA_HASH_SIZE + j] entropy_chunk vector
        }
    }

    // Phase 2: 2D hash packing
    let mut hash_count = chunk_size / SHA_HASH_SIZE;
    debug_assert_ne!(
        hash_count, 0,
        "0 2D packing iterations - is your chunk_size < 32?"
    );
    while hash_count < iterations as usize {
        let i = (hash_count % (chunk_size / SHA_HASH_SIZE)) * SHA_HASH_SIZE;
        let mut hasher = sha::Sha256::new();
        if i == 0 {
            hasher.update(&out_entropy_chunk[chunk_size - SHA_HASH_SIZE..]);
        } else {
            hasher.update(&out_entropy_chunk[i - SHA_HASH_SIZE..i]);
        }
        hasher.update(&out_entropy_chunk[i..i + SHA_HASH_SIZE]);
        let hash = hasher.finish();
        out_entropy_chunk[i..i + SHA_HASH_SIZE].copy_from_slice(&hash);

        hash_count += 1
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        capacity::{compute_entropy_chunk, compute_seed_hash},
        capacity_single::{self, SHA_HASH_SIZE},
    };
    use irys_primitives::Address;
    use rand;
    use rand::Rng;
    use std::time::Instant;

    #[test]
    fn test_seed_hash() {
        let testnet_config = Config::testnet();
        let mut rng = rand::thread_rng();
        let mining_address = Address::random();
        let chunk_offset = rng.gen_range(1..=1000);
        let mut partition_hash = [0u8; SHA_HASH_SIZE];
        rng.fill(&mut partition_hash[..]);
        let now = Instant::now();
        let rust_hash = capacity_single::compute_seed_hash(
            mining_address,
            chunk_offset,
            partition_hash,
            testnet_config.chain_id,
        );

        let elapsed = now.elapsed();
        println!("Rust seed implementation: {:.2?}", elapsed);

        let mut c_hash = Vec::<u8>::with_capacity(SHA_HASH_SIZE);
        let mining_addr_len = mining_address.len(); // note: might not line up with capacity? that should be fine...
        let mining_addr = mining_address.as_ptr() as *const std::os::raw::c_uchar;

        let partition_hash_len = partition_hash.len();
        let partition_hash = partition_hash.as_ptr() as *const std::os::raw::c_uchar;
        let c_hash_ptr = c_hash.as_ptr() as *mut u8;

        let now = Instant::now();
        let chain_id = testnet_config.chain_id;

        unsafe {
            compute_seed_hash(
                mining_addr,
                mining_addr_len,
                chunk_offset,
                chain_id,
                partition_hash,
                partition_hash_len,
                c_hash_ptr,
            );
            // we need to move the `len` ptr so rust picks up on the data the C fn wrote to the vec
            c_hash.set_len(c_hash.capacity());
        }

        let elapsed = now.elapsed();
        println!("C seed implementation: {:.2?}", elapsed);

        assert_eq!(rust_hash.to_vec(), c_hash, "Seed hashes should be equal")
    }

    #[test]
    fn test_compute_entropy_chunk() {
        let testnet_config = Config::testnet();
        let mut rng = rand::thread_rng();
        let mining_address = Address::random();
        let chunk_offset = rng.gen_range(1..=1000);
        let mut partition_hash = [0u8; SHA_HASH_SIZE];
        rng.fill(&mut partition_hash[..]);
        let iterations = 1_000;

        let mut chunk: Vec<u8> = Vec::<u8>::with_capacity(CHUNK_SIZE as usize);

        let now = Instant::now();

        capacity_single::compute_entropy_chunk(
            mining_address,
            chunk_offset,
            partition_hash,
            iterations,
            testnet_config.chunk_size as usize,
            &mut chunk,
            testnet_config.chain_id,
        );

        let elapsed = now.elapsed();
        println!("Rust implementation: {:.2?}", elapsed);

        let mut c_chunk = Vec::<u8>::with_capacity(CHUNK_SIZE as usize);

        let mining_addr_len = mining_address.len(); // note: might not line up with capacity? that should be fine...
        let partition_hash_len = partition_hash.len();

        let mining_addr = mining_address.as_ptr() as *const std::os::raw::c_uchar;
        let partition_hash = partition_hash.as_ptr() as *const std::os::raw::c_uchar;
        let c_chunk_ptr = c_chunk.as_ptr() as *mut u8;

        let now = Instant::now();
        let chain_id = testnet_config.chain_id;

        unsafe {
            compute_entropy_chunk(
                mining_addr,
                mining_addr_len,
                chunk_offset,
                chain_id,
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

        assert_eq!(chunk, c_chunk, "Chunks should be equal")
    }
}
