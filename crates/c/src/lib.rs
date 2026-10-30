// The C/CUDA code hashes `chain_id` and the chunk offset in native byte order, while the Rust
// implementation uses little-endian. Both agree only on little-endian targets.
#[cfg(target_endian = "big")]
compile_error!("irys-c supports little-endian targets only");
const _: () = assert!(
    u32::from_ne_bytes([1, 0, 0, 0]) == 1,
    "irys-c supports little-endian targets only"
);

pub mod capacity;
pub mod capacity_single;

#[cfg(feature = "nvidia")]
pub mod capacity_cuda;
#[cfg(feature = "nvidia")]
pub mod nvidia_gpu_config;
