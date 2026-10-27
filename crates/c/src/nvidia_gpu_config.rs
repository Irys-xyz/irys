use std::os::raw::c_int;

// CUDA Runtime API bindings (minimal subset)
#[link(name = "cudart")]
unsafe extern "C" {
    fn cudaGetDeviceCount(count: *mut c_int) -> c_int;
    fn cudaDeviceGetAttribute(value: *mut c_int, attr: c_int, device: c_int) -> c_int;
    fn cudaSetDevice(device: c_int) -> c_int;
}

/// The subset of CUDA's `cudaDeviceAttr` enum (driver_types.h) that this crate reads, with the
/// same numeric values. Attributes are queried one by one so this code does not depend on the
/// toolkit's `cudaDeviceProp` struct layout, which grows between toolkit versions.
#[derive(Debug, Clone, Copy)]
#[repr(i32)]
enum CudaDeviceAttribute {
    MaxThreadsPerBlock = 1,
    WarpSize = 10,
    MultiProcessorCount = 16,
    MaxThreadsPerMultiProcessor = 39,
    ComputeCapabilityMajor = 75,
    ComputeCapabilityMinor = 76,
    MaxRegistersPerMultiprocessor = 82,
    MaxBlocksPerMultiprocessor = 106,
}

fn get_device_attribute(device_id: i32, attribute: CudaDeviceAttribute) -> Result<i32, String> {
    let mut value: c_int = 0;
    match unsafe { cudaDeviceGetAttribute(&mut value, attribute as c_int, device_id) } {
        0 => Ok(value),
        n => Err(format!(
            "Failed to get device attribute {:?} for device {}: error code {}",
            attribute, device_id, n
        )),
    }
}

pub fn set_current_cuda_device_for_thread(device_id: i32) -> Result<(), String> {
    match unsafe { cudaSetDevice(device_id) } {
        0 => Ok(()),
        n => Err(format!(
            "Switching thread to CUDA device {} failed - error code {}",
            device_id, n
        )),
    }
}

#[derive(Debug)]
pub struct NvidiaGpuConfig {
    pub device_id: i32,
    pub compute_capability: (i32, i32),
    pub max_threads_per_block: i32,
    pub warp_size: i32,
    pub multiprocessor_count: i32,
    pub max_threads_per_multiprocessor: i32,
    pub max_blocks_per_multiprocessor: i32,
    pub regs_per_multiprocessor: i32,
}

impl NvidiaGpuConfig {
    /// Query GPU properties for the specified device
    pub fn query_device(device_id: i32) -> Result<Self, String> {
        let attr = |attribute| get_device_attribute(device_id, attribute);
        Ok(Self {
            device_id,
            compute_capability: (
                attr(CudaDeviceAttribute::ComputeCapabilityMajor)?,
                attr(CudaDeviceAttribute::ComputeCapabilityMinor)?,
            ),
            max_threads_per_block: attr(CudaDeviceAttribute::MaxThreadsPerBlock)?,
            warp_size: attr(CudaDeviceAttribute::WarpSize)?,
            multiprocessor_count: attr(CudaDeviceAttribute::MultiProcessorCount)?,
            max_threads_per_multiprocessor: attr(CudaDeviceAttribute::MaxThreadsPerMultiProcessor)?,
            max_blocks_per_multiprocessor: attr(CudaDeviceAttribute::MaxBlocksPerMultiprocessor)?,
            regs_per_multiprocessor: attr(CudaDeviceAttribute::MaxRegistersPerMultiprocessor)?,
        })
    }

    /// Get the number of available CUDA devices
    pub fn get_device_count() -> Result<i32, String> {
        unsafe {
            let mut count: c_int = 0;
            let result = cudaGetDeviceCount(&mut count);

            if result != 0 {
                return Err(format!("Failed to get device count: error code {}", result));
            }

            Ok(count)
        }
    }

    pub fn cores_per_sm(&self) -> i32 {
        match self.compute_capability {
            // Kepler (3.x)
            (3, 0) => 192,
            (3, 2) => 192,
            (3, 5) => 192,
            (3, 7) => 192,
            // Maxwell (5.x)
            (5, 0) => 128,
            (5, 2) => 128,
            (5, 3) => 128,
            // Pascal (6.x)
            (6, 0) => 64,
            (6, 1) => 128,
            (6, 2) => 128,
            // Volta (7.0)
            (7, 0) => 64,
            // Turing (7.5)
            (7, 5) => 64,
            // Ampere (8.x)
            (8, 0) => 64,
            (8, 6) => 128,
            (8, 7) => 128,
            (8, 9) => 128, // Ada Lovelace
            // Hopper (9.0)
            (9, 0) => 128,
            // Default fallback for unknown architectures
            _ => {
                // For newer architectures, assume 128 (common for recent GPUs)
                if self.compute_capability.0 >= 6 {
                    128
                } else {
                    64
                }
            }
        }
    }

    /// Print detailed device information
    pub fn print_info(&self) {
        println!("GPU Configuration:");
        println!("  Device ID: {}", self.device_id);
        println!(
            "  Compute Capability: {}.{}",
            self.compute_capability.0, self.compute_capability.1
        );
        println!("  Max Threads per Block: {}", self.max_threads_per_block);
        println!("  Warp Size: {}", self.warp_size);
        println!("  Multiprocessor Count: {}", self.multiprocessor_count);
        println!(
            "  Max Threads per Multiprocessor: {}",
            self.max_threads_per_multiprocessor
        );
        println!(
            "  Max Blocks per Multiprocessor: {}",
            self.max_blocks_per_multiprocessor
        );
        println!(" registers per SM: {}", self.regs_per_multiprocessor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_device_reads_nonzero_attributes() {
        let config = NvidiaGpuConfig::query_device(0).expect("query device 0");
        assert!(config.multiprocessor_count > 0);
        assert!(config.compute_capability.0 > 0);
        assert!(config.max_threads_per_block > 0);
        assert!(config.max_threads_per_multiprocessor > 0);
        assert!(config.max_blocks_per_multiprocessor > 0);
        assert!(config.regs_per_multiprocessor > 0);
        assert_eq!(config.warp_size, 32);
    }
}
