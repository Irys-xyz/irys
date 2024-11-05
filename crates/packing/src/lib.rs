use irys_c::capacity::compute_entropy_chunk;
use irys_primitives::IrysTxId;
use irys_types::{Address, CHUNK_SIZE};

pub const PACKING_SHA_1_5_S: u32 = 22_500_000;

/// Performs the entropy packing for the specified chunk offset, partition, and mining address
/// defaults to [`PACKING_SHA_1_5_S`]`
pub fn capacity_pack_range(
    mining_address: Address,
    chunk_offset: std::ffi::c_ulong,
    partition_hash: IrysTxId,
    iterations: Option<u32>,
) -> eyre::Result<Vec<u8>> {
    let mining_address: [u8; 20] = mining_address.0.into();
    // TODO @JesseTheRobot - allow a vec to get passed back for writing to so we don't de/reallocate memory
    let mut entropy_chunk = Vec::<u8>::with_capacity(CHUNK_SIZE.try_into().unwrap());
    let partition_hash: [u8; 32] = partition_hash.0.into();

    let mining_addr_len = mining_address.len(); // note: might not line up with capacity? that should be fine...
    let mining_addr = mining_address.as_ptr() as *const std::os::raw::c_uchar;

    let partition_hash_len = partition_hash.len();
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
    Ok(entropy_chunk)
}

enum PackingType {
    CPU,
    CUDA,
    AMD,
}

const PACKING_TYPE: PackingType = PackingType::CPU;

pub fn capacity_pack_range_with_data(
    mut data: Vec<u8>,
    mining_address: Address,
    chunk_offset: std::ffi::c_ulong,
    partition_hash: IrysTxId,
    iterations: Option<u32>,
) -> eyre::Result<Vec<u8>> {
    match PACKING_TYPE {
        PackingType::CPU => {
            capacity_pack_range(mining_address, chunk_offset, partition_hash, iterations).map(|r| {
                data.iter_mut()
                    .zip(r.iter())
                    .for_each(|(x1, x2)| *x1 ^= *x2);

                data
            })
        }
        _ => unimplemented!(),
    }
}
