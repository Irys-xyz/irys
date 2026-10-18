pub(super) mod cpu;
pub(super) mod remote;

#[cfg(feature = "nvidia")]
pub(super) mod cuda;

use std::sync::Arc;

use async_trait::async_trait;
use irys_domain::StorageModule;
use irys_types::{IrysAddress, PartitionChunkRange, partition::PartitionHash};

/// Bytes in one CUDA entropy batch.
///
/// `num_chunks * chunk_size` is not a `u32` product. 16384 chunks of 256KiB
/// is 2^32, which panics in debug and wraps to 0 in release. `None` when the
/// product does not fit in `usize`.
#[cfg(any(test, feature = "nvidia"))]
pub(super) fn cuda_entropy_batch_bytes(num_chunks: u32, chunk_size: u64) -> Option<usize> {
    let chunks = usize::try_from(num_chunks).ok()?;
    let chunk = usize::try_from(chunk_size).ok()?;
    chunks.checked_mul(chunk)
}

/// Byte range of chunk `index` in a buffer from [`cuda_entropy_batch_bytes`].
#[cfg(any(test, feature = "nvidia"))]
pub(super) fn cuda_chunk_byte_range(index: u32, chunk_size: u64) -> Option<std::ops::Range<usize>> {
    let chunk = usize::try_from(chunk_size).ok()?;
    let start = usize::try_from(index).ok()?.checked_mul(chunk)?;
    let end = start.checked_add(chunk)?;
    Some(start..end)
}

/// Trait for different packing strategies
#[async_trait]
pub(super) trait PackingStrategy: Send + Sync {
    /// Pack chunks using this strategy
    async fn pack(
        &self,
        storage_module: &Arc<StorageModule>,
        chunk_range: PartitionChunkRange,
        mining_address: IrysAddress,
        partition_hash: PartitionHash,
        storage_module_id: usize,
        short_writes_before_sync: u32,
    ) -> Result<(), String>;
}

#[cfg(test)]
mod cuda_batch_bytes_tests {
    use super::{cuda_chunk_byte_range, cuda_entropy_batch_bytes};

    /// Protocol chunk size. A batch of 16384 of these is 4 GiB, which is `2^32`.
    const CHUNK: u64 = 256 * 1024;

    #[test]
    fn batch_of_16384_chunks_does_not_wrap_the_buffer_length() {
        let num_chunks = 16_384_u32;
        let bytes = cuda_entropy_batch_bytes(num_chunks, CHUNK).expect("batch byte length");
        assert_eq!(bytes, 16_384_usize * (CHUNK as usize));
        let last = cuda_chunk_byte_range(num_chunks - 1, CHUNK).expect("last chunk");
        assert_eq!(last.start, bytes - CHUNK as usize);
        assert_eq!(last.end, bytes);
    }

    #[test]
    fn template_batch_of_1024_chunks_keeps_its_length() {
        let bytes = cuda_entropy_batch_bytes(1024, CHUNK).expect("batch byte length");
        assert_eq!(bytes, 1024_usize * (CHUNK as usize));
        let first = cuda_chunk_byte_range(0, CHUNK).expect("first chunk");
        assert_eq!(first, 0..CHUNK as usize);
    }

    #[test]
    fn batch_byte_length_is_none_when_the_product_does_not_fit() {
        assert!(cuda_entropy_batch_bytes(2, u64::MAX).is_none());
        assert!(cuda_chunk_byte_range(u32::MAX, u64::from(u32::MAX) + 1).is_none());
    }
}
