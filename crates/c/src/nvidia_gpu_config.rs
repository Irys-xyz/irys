use std::os::raw::c_int;

// CUDA Runtime API bindings (minimal subset)
#[cfg(feature = "nvidia")]
#[link(name = "cudart")]
unsafe extern "C" {
    fn cudaGetDeviceCount(count: *mut c_int) -> c_int;
    fn cudaGetDeviceProperties(prop: *mut CudaDeviceProp, device: c_int) -> c_int;
    fn cudaSetDevice(device: c_int) -> c_int;
}

#[cfg(feature = "nvidia")]
#[repr(C)]
#[derive(Debug)]
struct CudaDeviceProp {
    name: [u8; 256],
    uuid: [u8; 16],
    luid: [u8; 8],
    luid_device_node_mask: u32,
    total_global_mem: usize,
    shared_mem_per_block: usize,
    regs_per_block: c_int,
    warp_size: c_int,
    mem_pitch: usize,
    max_threads_per_block: c_int,
    max_threads_dim: [c_int; 3],
    max_grid_size: [c_int; 3],
    clock_rate: c_int,
    total_const_mem: usize,
    major: c_int,
    minor: c_int,
    texture_alignment: usize,
    texture_pitch_alignment: usize,
    device_overlap: c_int,
    multi_processor_count: c_int,
    kernel_exec_timeout_enabled: c_int,
    integrated: c_int,
    can_map_host_memory: c_int,
    compute_mode: c_int,
    max_texture_1d: c_int,
    max_texture_1d_mipmap: c_int,
    max_texture_1d_linear: c_int,
    max_texture_2d: [c_int; 2],
    max_texture_2d_mipmap: [c_int; 2],
    max_texture_2d_linear: [c_int; 3],
    max_texture_2d_gather: [c_int; 2],
    max_texture_3d: [c_int; 3],
    max_texture_3d_alt: [c_int; 3],
    max_texture_cubemap: c_int,
    max_texture_1d_layered: [c_int; 2],
    max_texture_2d_layered: [c_int; 3],
    max_texture_cubemap_layered: [c_int; 2],
    max_surface_1d: c_int,
    max_surface_2d: [c_int; 2],
    max_surface_3d: [c_int; 3],
    max_surface_1d_layered: [c_int; 2],
    max_surface_2d_layered: [c_int; 3],
    max_surface_cubemap: c_int,
    max_surface_cubemap_layered: [c_int; 2],
    surface_alignment: usize,
    concurrent_kernels: c_int,
    ecc_enabled: c_int,
    pci_bus_id: c_int,
    pci_device_id: c_int,
    pci_domain_id: c_int,
    tcc_driver: c_int,
    async_engine_count: c_int,
    unified_addressing: c_int,
    memory_clock_rate: c_int,
    memory_bus_width: c_int,
    l2_cache_size: c_int,
    persisting_l2_cache_max_size: c_int,
    max_threads_per_multi_processor: c_int,
    stream_priorities_supported: c_int,
    global_l1_cache_supported: c_int,
    local_l1_cache_supported: c_int,
    shared_mem_per_multiprocessor: usize,
    regs_per_multiprocessor: c_int,
    managed_memory: c_int,
    is_multi_gpu_board: c_int,
    multi_gpu_board_group_id: c_int,
    host_native_atomic_supported: c_int,
    single_to_double_precision_perf_ratio: c_int,
    pageable_memory_access: c_int,
    concurrent_managed_access: c_int,
    compute_preemption_supported: c_int,
    can_use_host_pointer_for_registered_mem: c_int,
    cooperative_launch: c_int,
    cooperative_multi_device_launch: c_int,
    // spellchecker:ignore-next-line
    shared_mem_per_block_optin: usize,
    pageable_memory_access_uses_host_page_tables: c_int,
    direct_managed_mem_access_from_host: c_int,
    max_blocks_per_multi_processor: c_int,
    access_policy_max_window_size: c_int,
    reserved_shared_mem_per_block: usize,
}

pub fn set_current_cuda_device_for_thread(device_id: i32) -> Result<(), String> {
    match unsafe { cudaSetDevice(device_id) } {
        0 => Ok(()),
        n => Err(format!(
            "Switching thread to CUDA device {} failed - error code {}",
            &device_id, n
        )),
    }
}

#[derive(Debug)]
pub struct NvidiaGpuConfig {
    pub device_id: i32,
    pub device_name: String,
    pub compute_capability: (i32, i32),
    pub max_threads_per_block: i32,
    pub max_block_dim: (i32, i32, i32),
    pub max_grid_size: (i32, i32, i32),
    pub warp_size: i32,
    pub multiprocessor_count: i32,
    pub max_threads_per_multiprocessor: i32,
    pub max_blocks_per_multiprocessor: i32,
    pub regs_per_multiprocessor: i32,
}

#[cfg(feature = "nvidia")]
impl NvidiaGpuConfig {
    /// Query GPU properties for the specified device
    pub fn query_device(device_id: i32) -> Result<Self, String> {
        unsafe {
            let mut prop: CudaDeviceProp = std::mem::zeroed();
            let result = cudaGetDeviceProperties(&mut prop, device_id);

            if result != 0 {
                return Err(format!(
                    "Failed to get device properties: error code {}",
                    result
                ));
            }

            // Convert device name from C string
            let name_bytes: Vec<u8> = prop.name.iter().take_while(|&&b| b != 0).copied().collect();
            let device_name = String::from_utf8_lossy(&name_bytes).to_string();

            Ok(Self {
                device_id,
                device_name,
                compute_capability: (prop.major, prop.minor),
                max_threads_per_block: prop.max_threads_per_block,
                max_block_dim: (
                    prop.max_threads_dim[0],
                    prop.max_threads_dim[1],
                    prop.max_threads_dim[2],
                ),
                max_grid_size: (
                    prop.max_grid_size[0],
                    prop.max_grid_size[1],
                    prop.max_grid_size[2],
                ),
                warp_size: prop.warp_size,
                multiprocessor_count: prop.multi_processor_count,
                max_threads_per_multiprocessor: prop.max_threads_per_multi_processor,
                max_blocks_per_multiprocessor: prop.max_blocks_per_multi_processor,
                regs_per_multiprocessor: prop.regs_per_multiprocessor,
            })
        }
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
        println!("  Device Name: {}", self.device_name);
        println!(
            "  Compute Capability: {}.{}",
            self.compute_capability.0, self.compute_capability.1
        );
        println!("  Max Threads per Block: {}", self.max_threads_per_block);
        println!(
            "  Max Block Dimensions: ({}, {}, {})",
            self.max_block_dim.0, self.max_block_dim.1, self.max_block_dim.2
        );
        println!(
            "  Max Grid Size: ({}, {}, {})",
            self.max_grid_size.0, self.max_grid_size.1, self.max_grid_size.2
        );
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
        println!(" registers per SM: {}", &self.regs_per_multiprocessor);
    }
}
