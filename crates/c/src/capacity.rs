use std::ffi::c_ulong;

use irys_types::PACKING_SHA_1_5_S;

#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
include!(concat!(env!("OUT_DIR"), "/capacity_bindings.rs"));

fn capacity_single_test() {
    // 20 bytes - evm
    let mining_address = vec![
        46, 6, 63, 55, 199, 241, 63, 198, 140, 201, 68, 178, 74, 212, 59, 13, 150, 185, 126, 37,
    ];

    let chunk_offset: c_ulong = 0;
    // 32 bytes
    let partition_hash = vec![
        76, 64, 86, 6, 191, 79, 136, 60, 155, 21, 146, 95, 199, 64, 10, 144, 248, 11, 6, 144, 135,
        179, 149, 173, 126, 18, 174, 58, 214, 83, 89, 74,
    ];
    // alloc space for the C fn to write into
    let mut entropy_chunk = Vec::<u8>::with_capacity(DATA_CHUNK_SIZE.try_into().unwrap());
    let mining_addr_len = mining_address.len(); // note: might not line up with capacity? that should be fine...
    let mining_addr = mining_address.as_ptr() as *const std::os::raw::c_uchar;

    let partition_hash_len = partition_hash.len();
    let partition_hash = partition_hash.as_ptr() as *const std::os::raw::c_uchar;
    let entropy_chunk_ptr = entropy_chunk.as_ptr() as *mut u8;
    unsafe {
        compute_entropy_chunk(
            mining_addr,
            mining_addr_len,
            chunk_offset,
            partition_hash,
            partition_hash_len,
            entropy_chunk_ptr,
            PACKING_SHA_1_5_S,
        );
        // we need to move the `len` ptr so rust picks up on the data the C fn wrote to the vec
        entropy_chunk.set_len(entropy_chunk.capacity());
    }
    dbg!(entropy_chunk);
}
