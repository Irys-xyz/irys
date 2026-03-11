pub use irys_types::remote_packing::*;

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{H256, IrysAddress, PartitionChunkOffset, PartitionChunkRange, ii};
    use proptest::prelude::*;

    fn h256_strategy() -> impl Strategy<Value = H256> {
        any::<[u8; 32]>().prop_map(H256::from)
    }

    fn partition_chunk_range_strategy() -> impl Strategy<Value = PartitionChunkRange> {
        (any::<u32>(), any::<u32>()).prop_map(|(a, b)| {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(lo),
                PartitionChunkOffset::from(hi),
            ))
        })
    }

    proptest! {
        #[test]
        fn prop_remote_packing_request_serde_roundtrip(
            mining_address in any::<IrysAddress>(),
            partition_hash in h256_strategy(),
            chunk_range in partition_chunk_range_strategy(),
            chain_id in any::<u64>(),
            chunk_size in any::<u64>(),
            entropy_packing_iterations in any::<u32>(),
        ) {
            let request = RemotePackingRequest {
                mining_address,
                partition_hash,
                chunk_range,
                chain_id,
                chunk_size,
                entropy_packing_iterations,
            };
            let json = serde_json::to_string(&request).unwrap();
            let deserialized: RemotePackingRequest = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(request, deserialized);
        }

        #[test]
        fn prop_packing_worker_config_serde_roundtrip(
            bind_port in any::<u8>(),
            bind_addr in "[a-z0-9.]{1,20}",
            cpu_packing_concurrency in any::<u16>(),
            gpu_packing_batch_size in any::<u32>(),
            max_pending in (1_u8..=255_u8).prop_map(|v| std::num::NonZeroU8::new(v).unwrap()),
        ) {
            let config = PackingWorkerConfig {
                bind_port,
                bind_addr,
                cpu_packing_concurrency,
                gpu_packing_batch_size,
                max_pending,
            };
            let json = serde_json::to_string(&config).unwrap();
            let deserialized: PackingWorkerConfig = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(config, deserialized);
        }
    }
}
