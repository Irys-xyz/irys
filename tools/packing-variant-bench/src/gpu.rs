//! Thin safe wrapper over the CUDA host API in `cuda/pvb.cu`. Every call's return code is checked.

use crate::{CHUNK_SIZE, Chunk, ChunkId, Variant};
use std::ffi::{CStr, c_char, c_int, c_void};

unsafe extern "C" {
    fn pvb_cuda_error_string(err: c_int) -> *const c_char;
    fn pvb_cuda_device_info(
        sm_count: *mut c_int,
        cc_major: *mut c_int,
        cc_minor: *mut c_int,
        max_threads_per_sm: *mut c_int,
        l2_bytes: *mut c_int,
    ) -> c_int;
    fn pvb_cuda_mem_info(free: *mut usize, total: *mut usize) -> c_int;
    fn pvb_cuda_driver_version(version: *mut c_int) -> c_int;
    fn pvb_cuda_kernel_regs(variant: c_int, regs: *mut c_int, local_bytes: *mut usize) -> c_int;
    fn pvb_cuda_occupancy(
        variant: c_int,
        threads_per_block: c_int,
        blocks_per_sm: *mut c_int,
    ) -> c_int;
    fn pvb_cuda_malloc(bytes: usize, ptr: *mut *mut c_void) -> c_int;
    fn pvb_cuda_free(ptr: *mut c_void) -> c_int;
    fn pvb_cuda_set_chunk_id_prefix(prefix: *const u8) -> c_int;
    fn pvb_cuda_run(
        variant: c_int,
        chunk_offset_start: u64,
        chunks_count: u64,
        d_chunks: *mut c_void,
        iterations: u32,
        blocks: c_int,
        threads_per_block: c_int,
        out_ms: *mut f32,
    ) -> c_int;
    fn pvb_cuda_copy_chunk(d_chunks: *const c_void, index: u64, host_out: *mut u8) -> c_int;
    fn pvb_fast_kernel_regs(
        variant: c_int,
        layout: c_int,
        regs: *mut c_int,
        local_bytes: *mut usize,
    ) -> c_int;
    fn pvb_fast_occupancy(
        variant: c_int,
        layout: c_int,
        threads_per_block: c_int,
        blocks_per_sm: *mut c_int,
    ) -> c_int;
    fn pvb_fast_run(
        variant: c_int,
        layout: c_int,
        chunk_offset_start: u64,
        chunks_count: u64,
        d_chunks: *mut c_void,
        iterations: u32,
        slice: u32,
        blocks: c_int,
        threads_per_block: c_int,
        out_ms: *mut f32,
    ) -> c_int;
    fn pvb_fast_copy_chunk(
        layout: c_int,
        d_chunks: *const c_void,
        chunks_count: u64,
        index: u64,
        host_out: *mut u8,
    ) -> c_int;
}

/// Which CUDA implementation to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kernel {
    /// Production-equivalent kernel (crates/c/c_src/capacity_cuda.cu approach).
    Ref,
    /// Optimised kernel; each chunk is a contiguous 256 KiB.
    FastContig,
    /// Optimised kernel; segment s of every chunk is stored side by side.
    FastInterleaved,
    /// Diagnostic: the fast kernel's Phase 2 hashing with no memory traffic (compute ceiling).
    /// Its output is not entropy and is never verified.
    FastNomem,
}

impl Kernel {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ref => "ref",
            Self::FastContig => "fast-contig",
            Self::FastInterleaved => "fast-interleaved",
            Self::FastNomem => "fast-nomem",
        }
    }

    /// Whether the kernel produces real entropy chunks that can be checked against the CPU.
    pub fn verifiable(self) -> bool {
        self != Self::FastNomem
    }

    /// Memory layout argument of the fast kernels.
    fn layout(self) -> c_int {
        match self {
            Self::Ref | Self::FastContig => 0,
            Self::FastInterleaved => 1,
            Self::FastNomem => 2,
        }
    }
}

#[derive(Debug)]
pub struct CudaError {
    pub what: &'static str,
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for CudaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} failed: CUDA error {} ({})",
            self.what, self.code, self.message
        )
    }
}

impl std::error::Error for CudaError {}

fn check(what: &'static str, code: c_int) -> Result<(), CudaError> {
    if code == 0 {
        return Ok(());
    }
    // SAFETY: cudaGetErrorString returns a static NUL-terminated string for any code.
    let message = unsafe { CStr::from_ptr(pvb_cuda_error_string(code)) }
        .to_string_lossy()
        .into_owned();
    let err = CudaError {
        what,
        code,
        message,
    };
    eprintln!("{err}");
    Err(err)
}

fn variant_id(v: Variant) -> c_int {
    match v {
        Variant::A => 0,
        Variant::B => 1,
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceInfo {
    pub sm_count: i32,
    pub compute_capability: String,
    pub max_threads_per_sm: i32,
    pub l2_bytes: i32,
    pub mem_free_bytes: usize,
    pub mem_total_bytes: usize,
    /// CUDA driver version, e.g. 13040 for 13.4.
    pub driver_version: i32,
}

pub fn device_info() -> Result<DeviceInfo, CudaError> {
    let (mut sm, mut maj, mut min, mut mt, mut l2) = (0, 0, 0, 0, 0);
    let (mut free, mut total) = (0_usize, 0_usize);
    // SAFETY: all out-pointers are valid for writes.
    unsafe {
        check(
            "cudaDeviceGetAttribute",
            pvb_cuda_device_info(&mut sm, &mut maj, &mut min, &mut mt, &mut l2),
        )?;
        check("cudaMemGetInfo", pvb_cuda_mem_info(&mut free, &mut total))?;
    }
    let mut driver = 0;
    // SAFETY: valid out-pointer.
    check("cudaDriverGetVersion", unsafe {
        pvb_cuda_driver_version(&mut driver)
    })?;
    Ok(DeviceInfo {
        sm_count: sm,
        compute_capability: format!("{maj}.{min}"),
        max_threads_per_sm: mt,
        l2_bytes: l2,
        mem_free_bytes: free,
        mem_total_bytes: total,
        driver_version: driver,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct KernelInfo {
    pub registers: i32,
    pub local_bytes: usize,
}

pub fn kernel_info(k: Kernel, v: Variant) -> Result<KernelInfo, CudaError> {
    let (mut regs, mut local) = (0, 0_usize);
    // SAFETY: valid out-pointers.
    let code = unsafe {
        match k {
            Kernel::Ref => pvb_cuda_kernel_regs(variant_id(v), &mut regs, &mut local),
            _ => pvb_fast_kernel_regs(variant_id(v), k.layout(), &mut regs, &mut local),
        }
    };
    check("cudaFuncGetAttributes", code)?;
    Ok(KernelInfo {
        registers: regs,
        local_bytes: local,
    })
}

pub fn occupancy_blocks_per_sm(
    k: Kernel,
    v: Variant,
    threads_per_block: u32,
) -> Result<i32, CudaError> {
    let mut b = 0;
    let tpb = threads_per_block as c_int;
    // SAFETY: valid out-pointer.
    let code = unsafe {
        match k {
            Kernel::Ref => pvb_cuda_occupancy(variant_id(v), tpb, &mut b),
            _ => pvb_fast_occupancy(variant_id(v), k.layout(), tpb, &mut b),
        }
    };
    check("cudaOccupancyMaxActiveBlocksPerMultiprocessor", code)?;
    Ok(b)
}

/// Uploads address || partition_hash || chain_id (LE) to constant memory; the kernel appends
/// the per-chunk partition offset.
pub fn set_chunk_id_prefix(id: &ChunkId) -> Result<(), CudaError> {
    let mut prefix = [0_u8; 60];
    prefix[..20].copy_from_slice(&id.address);
    prefix[20..52].copy_from_slice(&id.partition_hash);
    prefix[52..60].copy_from_slice(&id.chain_id.to_le_bytes());
    // SAFETY: prefix is exactly CHUNK_ID_PREFIX_LEN (60) bytes.
    unsafe {
        check(
            "cudaMemcpyToSymbol",
            pvb_cuda_set_chunk_id_prefix(prefix.as_ptr()),
        )
    }
}

/// Device buffer holding `chunks` entropy chunks.
pub struct DeviceChunks {
    ptr: *mut c_void,
    pub chunks: u64,
}

impl DeviceChunks {
    pub fn new(chunks: u64) -> Result<Self, CudaError> {
        let mut ptr = std::ptr::null_mut();
        // SAFETY: valid out-pointer.
        unsafe {
            check(
                "cudaMalloc",
                pvb_cuda_malloc(chunks as usize * CHUNK_SIZE, &mut ptr),
            )?;
        }
        Ok(Self { ptr, chunks })
    }

    /// Runs the kernel over all chunks; returns cudaEvent-timed kernel milliseconds. For the fast
    /// kernels, `slice` > 0 splits Phase 2 into launches of at most `slice` hash counts; the
    /// production-equivalent `Ref` kernel always runs as one launch, as in production.
    #[allow(
        clippy::too_many_arguments,
        reason = "launch parameters forwarded one-to-one to the FFI call"
    )]
    pub fn run(
        &mut self,
        k: Kernel,
        v: Variant,
        offset_start: u64,
        iterations: u32,
        slice: u32,
        blocks: u32,
        threads_per_block: u32,
    ) -> Result<f32, CudaError> {
        let mut ms = 0.0_f32;
        // SAFETY: self.ptr is a live allocation of self.chunks chunks; the kernel stays in bounds.
        let (blocks, tpb) = (blocks as c_int, threads_per_block as c_int);
        let code = unsafe {
            match k {
                Kernel::Ref => pvb_cuda_run(
                    variant_id(v),
                    offset_start,
                    self.chunks,
                    self.ptr,
                    iterations,
                    blocks,
                    tpb,
                    &mut ms,
                ),
                _ => pvb_fast_run(
                    variant_id(v),
                    k.layout(),
                    offset_start,
                    self.chunks,
                    self.ptr,
                    iterations,
                    slice,
                    blocks,
                    tpb,
                    &mut ms,
                ),
            }
        };
        check("kernel launch/run", code)?;
        Ok(ms)
    }

    /// Copies chunk `index`, as written by kernel `k`, back in entropy byte order.
    pub fn copy_chunk(&self, k: Kernel, index: u64, out: &mut Chunk) -> Result<(), CudaError> {
        assert!(index < self.chunks);
        // SAFETY: index is in bounds and `out` is CHUNK_SIZE bytes.
        let code = unsafe {
            match k {
                Kernel::Ref => pvb_cuda_copy_chunk(self.ptr, index, out.as_mut_ptr()),
                _ => {
                    pvb_fast_copy_chunk(k.layout(), self.ptr, self.chunks, index, out.as_mut_ptr())
                }
            }
        };
        check("cudaMemcpy D2H", code)
    }
}

impl Drop for DeviceChunks {
    fn drop(&mut self) {
        // SAFETY: ptr came from cudaMalloc and is freed exactly once.
        let _ = check("cudaFree", unsafe { pvb_cuda_free(self.ptr) });
    }
}
