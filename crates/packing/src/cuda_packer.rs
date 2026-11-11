//! A reusable CUDA entropy packer: one per concurrent GPU batch.
//!
//! Wraps `irys_cuda_packer` (crates/c/c_src/capacity_cuda.h): a CUDA stream, the device working
//! buffer and two pinned host slabs, allocated once and reused across batches. `pack` streams the
//! batch back slab by slab: the caller consumes one slab while the GPU converts and copies the
//! next, and the whole batch never has to exist in host memory.

use crate::{capacity_cuda as ffi, capacity_single, cuda_config::CUDAConfig};
use irys_types::{ConsensusConfig, IrysAddress, partition::PartitionHash};
use rand::Rng as _;
use std::{ffi::CStr, ptr::NonNull};

/// Chunks per slab handed to `pack`'s sink (128 MiB).
pub const SLAB_CHUNKS: u32 = ffi::IRYS_CUDA_SLAB_CHUNKS;

const CHUNK_BYTES: usize = ConsensusConfig::CHUNK_SIZE as usize;

fn cuda_error(what: &str, code: i32) -> eyre::Report {
    // SAFETY: cudaGetErrorString returns a static NUL-terminated string for any value.
    let message = unsafe { CStr::from_ptr(ffi::irys_cuda_error_string(code)) };
    eyre::eyre!(
        "{what} failed: CUDA error {code} ({})",
        message.to_string_lossy()
    )
}

/// See the module docs.
pub struct CudaPacker {
    raw: NonNull<ffi::irys_cuda_packer>,
    device_id: i32,
    capacity: u32,
    threads_per_block: i32,
}

// SAFETY: the packer exclusively owns its CUDA stream and buffers; they are only used through
// `&mut self`, and every C call selects the packer's device for the calling thread first.
unsafe impl Send for CudaPacker {}

impl CudaPacker {
    /// Allocates a packer for batches of up to `capacity` chunks on `device_id`: `capacity` x 256 KiB
    /// of device memory plus two 128 MiB slabs (device and pinned host).
    pub fn new(device_id: i32, capacity: u32, config: &CUDAConfig) -> eyre::Result<Self> {
        eyre::ensure!(
            capacity > 0,
            "a CUDA packer needs room for at least one chunk"
        );
        let mut raw = std::ptr::null_mut();
        // SAFETY: `raw` is a valid out-pointer; on success it holds a packer we now own.
        let rc = unsafe { ffi::irys_cuda_packer_new(device_id, u64::from(capacity), &mut raw) };
        if rc != 0 {
            return Err(cuda_error(
                &format!("allocating a CUDA packer for {capacity} chunks on device {device_id}"),
                rc,
            ));
        }
        Ok(Self {
            raw: NonNull::new(raw)
                .ok_or_else(|| eyre::eyre!("CUDA packer allocation returned null"))?,
            device_id,
            capacity,
            threads_per_block: config.threads_per_block,
        })
    }

    pub fn device_id(&self) -> i32 {
        self.device_id
    }

    /// Most chunks one `pack` call may produce.
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Packs the entropy of chunks `chunk_offset .. chunk_offset + num_chunks` and calls
    /// `sink(first, bytes)` for each slab in order: `bytes` holds contiguous 256 KiB chunks starting
    /// at chunk `first` of the batch. Byte-identical to `capacity_single::compute_entropy_chunk`.
    ///
    /// The first chunk, the last chunk and one random chunk are checked against the CPU
    /// implementation (computed while the GPU works). The first is checked before any slab reaches
    /// `sink`: under WDDM (Windows / WSL2) a kernel was seen to not execute while every CUDA call
    /// reported success, which would otherwise hand out a whole batch of wrong entropy. The others
    /// catch faults confined to part of a batch (e.g. one SM, or a bit flip in VRAM without ECC):
    /// a slab is handed out only after the checked chunks in it match, so a mismatch stops the
    /// batch there, though earlier slabs have already reached `sink`. An error from `sink` stops
    /// the batch; the packer stays usable.
    #[expect(
        clippy::too_many_arguments,
        reason = "the packing inputs, then the output sink"
    )]
    pub fn pack(
        &mut self,
        num_chunks: u32,
        mining_address: IrysAddress,
        chunk_offset: u64,
        partition_hash: PartitionHash,
        iterations: u32,
        chain_id: u64,
        mut sink: impl FnMut(u32, &[u8]) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        eyre::ensure!(
            (1..=self.capacity).contains(&num_chunks),
            "batch of {num_chunks} chunks does not fit a CUDA packer of {} chunks",
            self.capacity
        );
        let address: [u8; 20] = mining_address.0.into();
        let partition_hash_bytes: [u8; 32] = partition_hash.0;
        // SAFETY: the packer is valid; the address and hash buffers are 20 and 32 bytes as required.
        let rc = unsafe {
            ffi::irys_cuda_packer_start(
                self.raw.as_ptr(),
                address.as_ptr(),
                partition_hash_bytes.as_ptr(),
                chain_id,
                chunk_offset,
                u64::from(num_chunks),
                iterations,
                self.threads_per_block,
            )
        };
        if rc != 0 {
            return Err(cuda_error("queueing CUDA packing", rc));
        }

        // Chunks checked against the CPU: the first, the last and one at random. Each is computed
        // on its own CPU thread while the GPU works, and the slab holding it is handed out only
        // after it matches.
        let mut checked = vec![
            0,
            num_chunks - 1,
            rand::thread_rng().gen_range(0..num_chunks),
        ];
        checked.sort_unstable();
        checked.dedup();

        let raw = self.raw;
        std::thread::scope(|scope| {
            let mut expected = checked
                .into_iter()
                .map(|i| {
                    let cpu = scope.spawn(move || {
                        let mut chunk = Vec::with_capacity(CHUNK_BYTES);
                        capacity_single::compute_entropy_chunk(
                            mining_address,
                            chunk_offset + u64::from(i),
                            partition_hash_bytes,
                            iterations,
                            CHUNK_BYTES,
                            &mut chunk,
                            chain_id,
                        );
                        chunk
                    });
                    (i, cpu)
                })
                .collect::<Vec<_>>()
                .into_iter()
                .peekable();
            loop {
                let (mut bytes, mut first, mut count) = (std::ptr::null(), 0_u64, 0_u64);
                // SAFETY: the packer is valid and exclusively borrowed; the out-pointers are valid.
                let rc = unsafe {
                    ffi::irys_cuda_packer_next(raw.as_ptr(), &mut bytes, &mut first, &mut count)
                };
                if rc == ffi::IRYS_CUDA_PACKER_DONE {
                    return Ok(());
                }
                if rc != 0 {
                    return Err(cuda_error("CUDA packing", rc));
                }
                // SAFETY: the packer lends `count` chunks at `bytes` until the next `next` call,
                // which happens only after `sink` returns.
                let slab =
                    unsafe { std::slice::from_raw_parts(bytes, count as usize * CHUNK_BYTES) };
                while let Some((i, _)) = expected.peek()
                    && u64::from(*i) < first + count
                {
                    let (i, cpu) = expected.next().expect("peeked");
                    let want = cpu
                        .join()
                        .map_err(|_| eyre::eyre!("CPU entropy check panicked"))?;
                    let at = (u64::from(i) - first) as usize * CHUNK_BYTES;
                    eyre::ensure!(
                        slab[at..at + CHUNK_BYTES] == want[..],
                        "GPU entropy for partition offset {} does not match the CPU \
                         implementation; refusing the batch",
                        chunk_offset + u64::from(i)
                    );
                }
                sink(first as u32, slab)?;
            }
        })
    }
}

/// Idle packers for device 0, reused across batches. It holds at most one packer per batch that
/// ran concurrently, since a packer is only allocated when none is idle.
#[derive(Default)]
pub struct CudaPackerPool {
    idle: std::sync::Mutex<Vec<CudaPacker>>,
}

impl CudaPackerPool {
    /// Runs `f` with an idle packer that has room for `capacity` chunks, allocating one if there
    /// is none (an idle packer that is too small is freed), then returns it to the pool.
    pub fn with_packer<R>(
        &self,
        capacity: u32,
        f: impl FnOnce(&mut CudaPacker) -> eyre::Result<R>,
    ) -> eyre::Result<R> {
        let pooled = self.idle.lock().expect("CUDA packer pool poisoned").pop();
        let mut packer = match pooled {
            Some(packer) if packer.capacity() >= capacity => packer,
            _ => CudaPacker::new(0, capacity, &CUDAConfig::from_device_default()?)?,
        };
        let result = f(&mut packer);
        self.idle
            .lock()
            .expect("CUDA packer pool poisoned")
            .push(packer);
        result
    }
}

impl Drop for CudaPacker {
    fn drop(&mut self) {
        // SAFETY: we own the packer; it waits for queued work before freeing its buffers.
        unsafe { ffi::irys_cuda_packer_free(self.raw.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::H256;

    fn cpu_chunk(address: IrysAddress, offset: u64, hash: H256, iterations: u32) -> Vec<u8> {
        let mut chunk = Vec::with_capacity(CHUNK_BYTES);
        capacity_single::compute_entropy_chunk(
            address,
            offset,
            hash.0,
            iterations,
            CHUNK_BYTES,
            &mut chunk,
            1,
        );
        chunk
    }

    fn packer(capacity: u32) -> CudaPacker {
        CudaPacker::new(0, capacity, &CUDAConfig::from_device_default().unwrap()).unwrap()
    }

    /// One packer, several batches: sizes below, at and above one slab, slabs in order, every
    /// chunk equal to the CPU's.
    #[test]
    fn reused_packer_streams_slabs_in_order() {
        let mut packer = packer(1_200);
        let (address, hash) = (IrysAddress::random(), H256::random());
        for (num_chunks, offset) in [(1_u32, 5_u64), (SLAB_CHUNKS, 1_000), (1_200, 77)] {
            let mut next_first = 0;
            let mut all = Vec::new();
            packer
                .pack(
                    num_chunks,
                    address,
                    offset,
                    hash,
                    9_000,
                    1,
                    |first, slab| {
                        assert_eq!(first, next_first, "slabs out of order");
                        assert_eq!(slab.len() % CHUNK_BYTES, 0);
                        next_first += (slab.len() / CHUNK_BYTES) as u32;
                        all.extend_from_slice(slab);
                        Ok(())
                    },
                )
                .unwrap();
            assert_eq!(next_first, num_chunks);
            // spot-check a few chunks across slabs (all chunks are covered by other tests)
            for i in [0, num_chunks / 2, num_chunks - 1] {
                let got = &all[i as usize * CHUNK_BYTES..][..CHUNK_BYTES];
                assert!(
                    got == cpu_chunk(address, offset + u64::from(i), hash, 9_000).as_slice(),
                    "batch of {num_chunks}: chunk {i} differs"
                );
            }
        }
    }

    #[test]
    fn sink_error_stops_the_batch_and_the_packer_stays_usable() {
        let mut packer = packer(1_100);
        let (address, hash) = (IrysAddress::random(), H256::random());
        let mut slabs = 0;
        let err = packer
            .pack(1_100, address, 0, hash, 9_000, 1, |_, _| {
                slabs += 1;
                eyre::bail!("disk full")
            })
            .unwrap_err();
        assert!(err.to_string().contains("disk full"));
        assert_eq!(slabs, 1, "no slab after the sink failed");

        let mut chunks = 0;
        packer
            .pack(3, address, 10, hash, 9_000, 1, |_, slab| {
                chunks += slab.len() / CHUNK_BYTES;
                assert!(slab[..CHUNK_BYTES] == cpu_chunk(address, 10, hash, 9_000)[..]);
                Ok(())
            })
            .unwrap();
        assert_eq!(chunks, 3);
    }

    #[test]
    fn batch_larger_than_capacity_is_rejected() {
        let mut packer = packer(10);
        let result = packer.pack(
            11,
            IrysAddress::random(),
            0,
            H256::random(),
            9_000,
            1,
            |_, _| panic!("no slab expected"),
        );
        assert!(result.is_err());
    }
}
