use std::ops::BitXor;

use irys_c::capacity::compute_entropy_chunk;
use irys_primitives::IrysTxId;
use irys_types::{Address, ChunkBin, CHUNK_SIZE};

pub const PACKING_SHA_1_5_S: u32 = 22_500_000;

/// Performs the entropy packing for the specified chunk offset, partition, and mining address
/// defaults to [`PACKING_SHA_1_5_S`]`
pub fn capacity_pack_range(
    mining_address: Address,
    chunk_offset: std::ffi::c_ulong,
    partition_hash: IrysTxId,
    iterations: Option<u32>,
    entropy_chunk: &mut Vec<u8>,
) {
    let mining_address: [u8; 20] = mining_address.0.into();
    let partition_hash: [u8; 32] = partition_hash.0.into();

    let mining_addr_len = mining_address.len(); // note: might not line up with capacity? that should be fine...
    let partition_hash_len = partition_hash.len();

    let mining_addr = mining_address.as_ptr() as *const std::os::raw::c_uchar;
    let partition_hash = partition_hash.as_ptr() as *const std::os::raw::c_uchar;
    let entropy_chunk_ptr = entropy_chunk.as_ptr() as *mut u8;

    let iterations: u32 = iterations.unwrap_or(PACKING_SHA_1_5_S);

    unsafe {
        compute_entropy_chunk(
            mining_addr,
            mining_addr_len,
            chunk_offset,
            partition_hash,
            partition_hash_len,
            entropy_chunk_ptr,
            iterations,
        );
        // we need to move the `len` ptr so rust picks up on the data the C fn wrote to the vec
        entropy_chunk.set_len(entropy_chunk.capacity());
    }
}

enum PackingType {
    CPU,
    CUDA,
    AMD,
}

const PACKING_TYPE: PackingType = PackingType::CPU;

pub fn capacity_pack_range_with_data(
    data: &mut Vec<ChunkBin>,
    mining_address: Address,
    chunk_offset: std::ffi::c_ulong,
    partition_hash: IrysTxId,
    iterations: Option<u32>,
) {
    match PACKING_TYPE {
        PackingType::CPU => {
            let mut entropy_chunk = Vec::<u8>::with_capacity(CHUNK_SIZE as usize);
            data.iter_mut().enumerate().for_each(|(pos, mut chunk)| {
                capacity_pack_range(
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

fn xor_vec_u8_arrays_in_place<const N: usize>(a: &mut [u8; N], b: &Vec<u8>) {
    for i in 0..a.len() {
        a[i] = a[i].bitxor(b[i]);
    }
}

#[test]
fn test_chunks_packing() {
    use irys_c::capacity_single::SHA_HASH_SIZE;
    use rand;
    use rand::{Rng, RngCore};

    let mut rng = rand::thread_rng();
    let mining_address = Address::random();
    let chunk_offset = rng.gen_range(1..=1000);
    let mut partition_hash: [u8; SHA_HASH_SIZE] = [0; SHA_HASH_SIZE];
    rng.fill(&mut partition_hash);

    let num_chunks: usize = 3;
    let mut chunks: Vec<ChunkBin> = Vec::with_capacity(num_chunks);

    for _i in 0..num_chunks {
        let mut chunk = [0u8; CHUNK_SIZE as usize];
        rng.fill_bytes(&mut chunk);
        chunks.push(chunk);
    }

    // pick random chunk to verify later
    let rnd_chunk_pos = rng.gen_range(0..num_chunks);
    let mut rnd_chunk = chunks[rnd_chunk_pos];

    capacity_pack_range_with_data(
        &mut chunks,
        mining_address,
        chunk_offset,
        partition_hash.into(),
        Some(2 * CHUNK_SIZE as u32),
    );

    assert_eq!(
        num_chunks,
        chunks.len(),
        "Packed chunks should have same length of original chunks"
    );

    // calculate entropy for choosen random chunk
    let mut entropy_chunk = Vec::<u8>::with_capacity(CHUNK_SIZE.try_into().unwrap());
    capacity_pack_range(
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
