use std::ops::BitXor;

pub use irys_c::{capacity, capacity_cuda, capacity_single};
use irys_types::{
    partition::PartitionHash, Address, Base64, ChunkBytes, PackedChunk, UnpackedChunk, CONFIG, CHUNK_SIZE
};

/// Unpacks a PackedChunk into an UnpackedChunk by recomputing the required entropy,
/// unpacking & trimming the data, and passing through metadata (size, tx_offset, etc)
pub fn unpack(
    packed_chunk: &PackedChunk,
    entropy_packing_iterations: u32,
    chunk_size: usize,
) -> UnpackedChunk {
    let mut entropy: Vec<u8> = Vec::with_capacity(chunk_size);
    capacity_single::compute_entropy_chunk(
        packed_chunk.packing_address,
        packed_chunk.partition_offset as u64 * chunk_size as u64,
        packed_chunk.partition_hash.0,
        entropy_packing_iterations,
        chunk_size,
        &mut entropy,
    );

    let unpacked_data = unpack_with_entropy(packed_chunk, entropy, chunk_size);

    UnpackedChunk {
        data_root: packed_chunk.data_root,
        data_size: packed_chunk.data_size,
        data_path: packed_chunk.data_path.clone(),
        bytes: Base64(unpacked_data),
        tx_offset: packed_chunk.tx_offset,
    }
}

/// Unpacks a PackedChunk using the supplied entropy, returning *just the unpacked data*
#[inline]
pub fn unpack_with_entropy(
    packed_chunk: &PackedChunk,
    entropy: Vec<u8>,
    chunk_size: usize,
) -> Vec<u8> {
    debug_assert_eq!(
        entropy.len(),
        chunk_size,
        "entropy needs to be exactly chunk_size"
    );
    let chunk_size_u64: u64 = chunk_size.try_into().unwrap();
    let mut unpacked_data = packing_xor_vec_u8(entropy, &(packed_chunk.bytes.0));

    let data_size = packed_chunk.data_size;
    let num_chunks_in_tx = data_size.div_ceil(chunk_size_u64);
    // trim if this is the last chunk & if data_size isn't aligned to chunk_size
    if (packed_chunk.tx_offset as u64) == num_chunks_in_tx - 1 {
        let trailing_bytes = data_size % chunk_size_u64;
        // 0 means this last chunk is a full chunk
        if trailing_bytes != 0 {
            unpacked_data.truncate(trailing_bytes.try_into().unwrap());
        }
    };
    unpacked_data
}

/// Performs the entropy packing for the specified chunk offset, partition, and mining address
/// defaults to [`PACKING_SHA_1_5_S`]`, returns entropy chunk in out_entropy_chunk parameter.
/// Precondition: `out_entropy_chunk` should have at least DATA_CONFIG.chunk_size = 256KB (definded in capacity.h file) capacity
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

    let iterations: u32 = iterations.unwrap_or(CONFIG.packing_sha_1_5_s);

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

/// 2D Packing CUDA C implementation
pub fn capacity_pack_range_cuda_c(
    num_chunks: u32,
    mining_address: Address,
    chunk_offset: std::ffi::c_ulong,
    partition_hash: PartitionHash,
    iterations: Option<u32>,
    entropy: &mut Vec<u8>,
) {
    let mining_addr_len = mining_address.len(); // note: might not line up with capacity? that should be fine...
    let partition_hash_len = partition_hash.0.len();
    let mining_addr = mining_address.as_ptr() as *const std::os::raw::c_uchar;
    let partition_hash = partition_hash.as_ptr() as *const std::os::raw::c_uchar;
    let iterations: u32 = iterations.unwrap_or(CONFIG.packing_sha_1_5_s);

    let entropy_ptr = entropy.as_ptr() as *mut u8;

    unsafe {
        capacity_cuda::compute_entropy_chunks_cuda(
            mining_addr,
            mining_addr_len,
            chunk_offset,
            num_chunks as i64,
            partition_hash,
            partition_hash_len,
            entropy_ptr,
            iterations,
        );

        entropy.set_len(entropy.capacity());
    }
}

/// 2D Packing CUDA C implementation
pub fn capacity_pack_range_with_data_cuda_c(
    data: &mut Vec<u8>,
    mining_address: Address,
    chunk_offset: std::ffi::c_ulong,
    partition_hash: PartitionHash,
    iterations: Option<u32>,
) {
    let num_chunks: u32 = data.len() as u32 / CHUNK_SIZE as u32;
    let mut entropy: Vec<u8> = Vec::with_capacity(data.len());
    capacity_pack_range_cuda_c(
        num_chunks,
        mining_address,
        chunk_offset,
        partition_hash,
        iterations,
        &mut entropy,
    );

    // TODO: check if it is worth to move this to GPU ? implies big data transfer from host to device that now is not needed
    xor_vec_u8_arrays_in_place(data, &entropy);
}

#[derive(PartialEq)]
pub enum PackingType {
    CPU,
    #[allow(unused)]
    CUDA,
    #[allow(unused)]
    AMD,
}

pub const PACKING_TYPE: PackingType = PackingType::CPU; //PackingType::CUDA;

/// 2D Packing Rust implementation
pub fn capacity_pack_range_with_data(
    data: &mut Vec<ChunkBytes>,
    mining_address: Address,
    chunk_offset: std::ffi::c_ulong,
    partition_hash: PartitionHash,
    iterations: Option<u32>,
    chunk_size: usize,
) {
    let iterations: u32 = iterations.unwrap_or(CONFIG.packing_sha_1_5_s);

    match PACKING_TYPE {
        PackingType::CPU => {
            let mut entropy_chunk = Vec::<u8>::with_capacity(chunk_size);
            data.iter_mut().enumerate().for_each(|(pos, chunk)| {
                capacity_single::compute_entropy_chunk(
                    mining_address,
                    chunk_offset + pos as u64 * chunk_size as u64,
                    partition_hash.0,
                    iterations,
                    chunk_size,
                    &mut entropy_chunk,
                );
                xor_vec_u8_arrays_in_place(chunk, &entropy_chunk);
            })
        }
        // PackingType::CUDA => {
        //     capacity_pack_range_with_data_cuda_c(data, mining_address, chunk_offset, partition_hash, iterations)
        // }
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
            let mut entropy_chunk = Vec::<u8>::with_capacity(CONFIG.chunk_size as usize);
            data.iter_mut().enumerate().for_each(|(pos, chunk)| {
                capacity_pack_range_c(
                    mining_address,
                    chunk_offset + pos as u64 * CONFIG.chunk_size,
                    partition_hash,
                    iterations,
                    &mut entropy_chunk,
                );
                xor_vec_u8_arrays_in_place(chunk, &entropy_chunk);
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

/// Specialized variant, used when we pass in the entropy as argument a (which will always be chunk_size), and unpacked data in b (which can be smaller than chunk_size)
/// as xor is commutative, this allows us to avoid a clone of the chunk's data when writing, as oftentimes we have mutable access to the required entropy, but only a ref to the unpacked data.
/// note: this will always produce full chunk_size vecs, as expected by the storage module
#[inline]
pub fn packing_xor_vec_u8(mut entropy: Vec<u8>, data: &[u8]) -> Vec<u8> {
    debug_assert!(data.len() <= entropy.len());
    for i in 0..data.len() {
        entropy[i] = entropy[i].bitxor(data[i]);
    }
    entropy
}

#[cfg(test)]
mod tests {
    use crate::capacity_single::SHA_HASH_SIZE;
    use crate::*;
    use irys_types::H256;
    use rand::{Rng, RngCore};
    use std::time::*;

    #[test]
    fn test_compute_entropy_chunk() {
        let mut rng = rand::thread_rng();
        let mining_address = Address::random();
        let chunk_offset = rng.gen_range(1..=1000);
        let mut partition_hash = [0u8; SHA_HASH_SIZE];
        rng.fill(&mut partition_hash[..]);
        let iterations = 2 * CONFIG.chunk_size as u32;

        let mut chunk: Vec<u8> = Vec::<u8>::with_capacity(CHUNK_SIZE as usize);
        let mut chunk2: Vec<u8> = Vec::<u8>::with_capacity(CHUNK_SIZE as usize);

        let now = Instant::now();

        capacity_single::compute_entropy_chunk(
            mining_address,
            chunk_offset,
            partition_hash,
            iterations,
            CONFIG.chunk_size as usize,
            &mut chunk,
        );

        capacity_single::compute_entropy_chunk(
            mining_address,
            chunk_offset + CHUNK_SIZE,
            partition_hash,
            iterations,
            CHUNK_SIZE as usize,
            &mut chunk2,
        );

        let elapsed = now.elapsed();
        println!("Rust implementation: {:.2?}", elapsed);

        let mut c_chunk = Vec::<u8>::with_capacity(CHUNK_SIZE as usize);
        let mut c_chunk2 = Vec::<u8>::with_capacity(CHUNK_SIZE as usize);
        let now = Instant::now();

        capacity_pack_range_c(
            mining_address,
            chunk_offset,
            partition_hash.into(),
            Some(iterations),
            &mut c_chunk,
        );

        capacity_pack_range_c(
            mining_address,
            chunk_offset + CHUNK_SIZE,
            partition_hash.into(),
            Some(iterations),
            &mut c_chunk2,
        );

        let elapsed = now.elapsed();
        println!("C implementation: {:.2?}", elapsed);

        assert_eq!(chunk, c_chunk, "C chunks should be equal");
        assert_eq!(chunk2, c_chunk2, "Second C chunks should be equal");

        let mut c_chunk_cuda = Vec::<u8>::with_capacity(2 * CHUNK_SIZE as usize);
        let now = Instant::now();

        capacity_pack_range_cuda_c(
            2,
            mining_address,
            chunk_offset,
            partition_hash.into(),
            Some(iterations),
            &mut c_chunk_cuda,
        );

        let elapsed = now.elapsed();
        println!("C CUDA implementation: {:.2?}", elapsed);

        assert_eq!(
            chunk,
            c_chunk_cuda[0..CHUNK_SIZE as usize].to_vec(),
            "CUDA chunk should be equal"
        );
        assert_eq!(
            chunk2,
            c_chunk_cuda[CHUNK_SIZE as usize..(2 * CHUNK_SIZE) as usize].to_vec(),
            "Second CUDA chunk should be equal"
        );
    }

    #[test]
    fn test_bench_chunks_packing() {
        let mut rng = rand::thread_rng();
        let mining_address = Address::random();
        let chunk_offset = rng.gen_range(1..=1000);
        let mut partition_hash: [u8; SHA_HASH_SIZE] = [0; SHA_HASH_SIZE];
        rng.fill(&mut partition_hash);

        let num_chunks: usize = 4;
        let mut chunks: Vec<ChunkBytes> = Vec::with_capacity(num_chunks);
        let mut chunks_rust: Vec<ChunkBytes> = Vec::with_capacity(num_chunks);

        for _i in 0..num_chunks {
            let mut chunk = [0u8; CONFIG.chunk_size as usize];
            rng.fill_bytes(&mut chunk);
            chunks.push(chunk.to_vec());
            chunks_rust.push(chunk.to_vec());
        }

        assert_eq!(chunks, chunks_rust, "Rust and C packing should start equal");

        // pick random chunk to verify later
        let rnd_chunk_pos = rng.gen_range(0..num_chunks);
        let mut rnd_chunk = chunks[rnd_chunk_pos].clone();

        let iterations = Some(2 * CHUNK_SIZE as u32);
        let now = Instant::now();

        capacity_pack_range_with_data_c(
            &mut chunks,
            mining_address,
            chunk_offset,
            partition_hash.into(),
            iterations,
        );

        let elapsed = now.elapsed();
        println!("C implementation: {:.2?}", elapsed);

        let now = Instant::now();

        capacity_pack_range_with_data(
            &mut chunks_rust,
            mining_address,
            chunk_offset,
            partition_hash.into(),
            iterations,
            CHUNK_SIZE as usize,
        );

        let elapsed = now.elapsed();
        println!("Rust implementation: {:.2?}", elapsed);

        assert_eq!(chunks, chunks_rust, "Rust and C packing should be equal");

        // calculate entropy for choosen random chunk
        let mut entropy_chunk = Vec::<u8>::with_capacity(CONFIG.chunk_size.try_into().unwrap());
        capacity_pack_range_c(
            mining_address,
            chunk_offset + rnd_chunk_pos as u64 * CONFIG.chunk_size,
            partition_hash.into(),
            iterations,
            &mut entropy_chunk,
        );

        // sign picked random chunk with entropy
        xor_vec_u8_arrays_in_place(&mut rnd_chunk, &entropy_chunk);

        assert_eq!(chunks[rnd_chunk_pos], rnd_chunk, "Wrong packed chunk")
    }

    #[test]
    fn test_bench_chunks_packing_cuda() {
        let mut rng = rand::thread_rng();
        let mining_address = Address::random();
        let chunk_offset = rng.gen_range(1..=1000);
        let mut partition_hash: [u8; SHA_HASH_SIZE] = [0; SHA_HASH_SIZE];
        rng.fill(&mut partition_hash);

        let num_chunks: usize = 2;
        let mut chunks: Vec<u8> = Vec::with_capacity(num_chunks * CHUNK_SIZE as usize);
        let mut chunks_rust: Vec<ChunkBytes> = Vec::with_capacity(num_chunks);

        for _i in 0..num_chunks {
            let mut chunk = [0u8; CHUNK_SIZE as usize];
            rng.fill_bytes(&mut chunk);
            chunks_rust.push(chunk.to_vec());
            for j in 0..CHUNK_SIZE as usize {
                chunks.push(chunk[j]);
            }
        }

        let iterations = Some(2 * CHUNK_SIZE as u32);
        let now = Instant::now();

        capacity_pack_range_with_data_cuda_c(
            &mut chunks,
            mining_address,
            chunk_offset,
            partition_hash.into(),
            iterations,
        );

        let elapsed = now.elapsed();
        println!("C CUDA implementation: {:.2?}", elapsed);

        let now = Instant::now();
        capacity_pack_range_with_data(
            &mut chunks_rust,
            mining_address,
            chunk_offset,
            partition_hash.into(),
            iterations,
            CHUNK_SIZE as usize,
        );

        let elapsed = now.elapsed();
        println!("Rust implementation: {:.2?}", elapsed);

        for i in 0..num_chunks {
            for j in 0..CHUNK_SIZE as usize {
                //println!("chunk {} pos {}", i, j);
                assert_eq!(chunks_rust[i][j], chunks[i * CHUNK_SIZE as usize + j]);
            }
        }
    }

    #[test]
    fn test_chunk_packing_unpacking() {
        let mut rng = rand::thread_rng();

        let mining_address = Address::random();
        let chunk_offset = rng.gen_range(1..=1000);
        let mut partition_hash = [0u8; SHA_HASH_SIZE];
        rng.fill(&mut partition_hash[..]);

        let chunk_size = 32;
        let iterations = 2 * chunk_size as u32;

        let mut entropy_chunk: Vec<u8> = Vec::<u8>::with_capacity(chunk_size);

        capacity_single::compute_entropy_chunk(
            mining_address,
            chunk_offset,
            partition_hash,
            iterations,
            chunk_size,
            &mut entropy_chunk,
        );

        // simulate a smaller end chunk
        let data_size = chunk_size - rng.gen_range(0..chunk_size);
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        // pack the data
        let packed_data = packing_xor_vec_u8(entropy_chunk.clone(), &data_bytes);

        let packed_chunk = PackedChunk {
            data_root: H256::zero(),
            data_size: data_size as u64,
            data_path: Base64(vec![]),
            bytes: Base64(packed_data.clone()),
            tx_offset: 0,
            packing_address: mining_address,
            partition_offset: chunk_offset as u32,
            partition_hash: H256::from(partition_hash),
        };

        let unpacked_chunk = unpack(&packed_chunk, iterations, chunk_size);

        assert_eq!(unpacked_chunk.bytes.0, data_bytes);
    }
}
