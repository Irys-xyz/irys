//! Utilities to construct and decode Programmable Data (PD) access list entries and transactions.

use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_primitives::{B256, Bytes, U256};
use borsh::{BorshDeserialize, BorshSerialize};
use irys_types::precompile::PD_PRECOMPILE_ADDRESS;
use irys_types::range_specifier::{PdAccessListArg, PdAccessListArgSerde as _};
use std::io::{Read, Write};

/// Create a PD access list for a list of ChunkRangeSpecifiers, under the PD precompile address.
///
/// This is the canonical way to build PD access lists for testing and transaction construction.
/// Uses the `range_specifier` encoding format.
pub fn build_pd_access_list(
    specs: impl IntoIterator<Item = irys_types::range_specifier::ChunkRangeSpecifier>,
) -> AccessList {
    let storage_keys: Vec<B256> = specs
        .into_iter()
        .map(|spec| B256::from(spec.encode()))
        .collect();
    AccessList::from(vec![AccessListItem {
        address: PD_PRECOMPILE_ADDRESS,
        storage_keys,
    }])
}

/// Compute total PD chunks referenced in an access list (simple sum, no deduplication).
///
/// This function decodes access list keys using the canonical `range_specifier` encoding.
/// It handles both ChunkRead and ByteRead access list arguments:
/// - ChunkRead: Counted as `chunk_count` chunks
/// - ByteRead: Counted as 0 chunks
/// - Invalid encodings: Skipped with a warning log
pub fn sum_pd_chunks_in_access_list(access_list: &AccessList) -> u64 {
    access_list
        .0
        .iter()
        .filter(|item| item.address == PD_PRECOMPILE_ADDRESS)
        .flat_map(|item| item.storage_keys.iter())
        .filter_map(|key| {
            match PdAccessListArg::decode(&key.0) {
                Ok(PdAccessListArg::ChunkRead(spec)) => Some(spec.chunk_count as u64),
                Ok(PdAccessListArg::ByteRead(_byte_spec)) => {
                    // ByteRead references chunks already declared in a corresponding ChunkRead entry
                    // via its index field. Those chunks are counted via the ChunkRead entry to avoid
                    // double-counting the same chunks for network bandwidth calculations.
                    Some(0)
                }
                Err(e) => {
                    // Invalid encoding - log warning and skip
                    tracing::warn!("Invalid PD access list key encoding, skipping: {}", e);
                    None
                }
            }
        })
        .sum()
}

/// Magic prefix to identify PD metadata header in transaction calldata.
/// Length is fixed to avoid ambiguity and aid quick detection.
pub const IRYS_PD_HEADER_MAGIC: &[u8; 12] = b"irys-pd-meta";

/// PD header version values.
pub const PD_HEADER_VERSION_V1: u16 = 1;

/// Size of version field in bytes (u16 big-endian).
const PD_HEADER_VERSION_SIZE: usize = 2;

/// Size of a U256 field in bytes.
const U256_SIZE: usize = 32;

/// Total size of PdHeaderV1 payload in bytes (2 x U256).
const PD_HEADER_V1_SIZE: usize = U256_SIZE + U256_SIZE;

/// V1 PD header carrying pricing-related metadata for PD reads.
/// Manual Borsh impls to keep a stable, fixed-size encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdHeaderV1 {
    /// User-offered PD priority fee per chunk (scaled 1e18 tokens).
    pub max_priority_fee_per_chunk: U256,
    /// User-accepted maximum PD base fee per chunk (scaled 1e18 tokens).
    /// Acts like EIP-1559 `max_fee_per_gas` but for PD base fee exposure.
    pub max_base_fee_per_chunk: U256,
}

impl BorshSerialize for PdHeaderV1 {
    fn serialize<W: Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        // U256 be (32 bytes) priority per chunk
        writer.write_all(&self.max_priority_fee_per_chunk.to_be_bytes::<U256_SIZE>())?;
        // U256 be (32 bytes) max base per chunk
        writer.write_all(&self.max_base_fee_per_chunk.to_be_bytes::<U256_SIZE>())?;
        Ok(())
    }
}

impl BorshDeserialize for PdHeaderV1 {
    fn deserialize_reader<R: Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let mut prio_buf = [0_u8; U256_SIZE];
        reader.read_exact(&mut prio_buf)?;
        let max_priority_fee_per_chunk = U256::from_be_bytes(prio_buf);

        let mut base_buf = [0_u8; U256_SIZE];
        reader.read_exact(&mut base_buf)?;
        let max_base_fee_per_chunk = U256::from_be_bytes(base_buf);

        Ok(Self {
            max_priority_fee_per_chunk,
            max_base_fee_per_chunk,
        })
    }
}

/// Encodes a PD header and prepends it to the provided calldata bytes.
/// Result layout: [magic][version:u16 be][borsh(header)][rest]
pub fn prepend_pd_header_v1_to_calldata(header: &PdHeaderV1, rest: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(
        IRYS_PD_HEADER_MAGIC.len() + PD_HEADER_VERSION_SIZE + PD_HEADER_V1_SIZE + rest.len(),
    );
    out.extend_from_slice(IRYS_PD_HEADER_MAGIC);
    out.extend_from_slice(&PD_HEADER_VERSION_V1.to_be_bytes());
    let mut buf = Vec::with_capacity(PD_HEADER_V1_SIZE);
    header
        .serialize(&mut buf)
        .expect("borsh serialize PdHeaderV1");
    out.extend_from_slice(&buf);
    out.extend_from_slice(rest);
    out.into()
}

/// Attempts to detect and decode a PD header at the beginning of `input`.
/// If present and valid, returns (header, offset_after_header).
pub fn detect_and_decode_pd_header(
    input: &[u8],
) -> Result<Option<(PdHeaderV1, usize)>, borsh::io::Error> {
    let magic_len = IRYS_PD_HEADER_MAGIC.len();
    if input.len() < magic_len + PD_HEADER_VERSION_SIZE {
        return Ok(None);
    }
    if &input[..magic_len] != IRYS_PD_HEADER_MAGIC {
        return Ok(None);
    }

    let ver_bytes = [input[magic_len], input[magic_len + 1]];
    let version = u16::from_be_bytes(ver_bytes);
    if version != PD_HEADER_VERSION_V1 {
        return Err(borsh::io::Error::new(
            borsh::io::ErrorKind::InvalidData,
            format!("unsupported PD header version: {}", version),
        ));
    }

    let hdr_start = magic_len + PD_HEADER_VERSION_SIZE;
    let mut rdr = &input[hdr_start..];
    let header = PdHeaderV1::deserialize_reader(&mut rdr)?;
    let consumed = input.len() - rdr.len();
    Ok(Some((header, consumed)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::chunk_spec_with_params;
    use alloy_primitives::Address;

    fn other_address() -> Address {
        Address::repeat_byte(0xff)
    }

    fn chunk_spec(chunk_count: u16) -> irys_types::range_specifier::ChunkRangeSpecifier {
        chunk_spec_with_params([0; 25], 0, chunk_count)
    }

    #[test]
    fn test_sum_pd_chunks_empty_access_list() {
        let access_list = AccessList::default();
        assert_eq!(sum_pd_chunks_in_access_list(&access_list), 0);
    }

    #[test]
    fn test_sum_pd_chunks_no_pd_precompile() {
        let access_list = AccessList::from(vec![AccessListItem {
            address: other_address(),
            storage_keys: vec![B256::ZERO, B256::repeat_byte(0x01)],
        }]);
        assert_eq!(sum_pd_chunks_in_access_list(&access_list), 0);
    }

    #[test]
    fn test_sum_pd_chunks_no_storage_keys() {
        let access_list = AccessList::from(vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![],
        }]);
        assert_eq!(sum_pd_chunks_in_access_list(&access_list), 0);
    }

    // Basic Functionality Tests

    #[test]
    fn test_sum_pd_chunks_single_key() {
        let access_list = build_pd_access_list(vec![chunk_spec(42)]);
        assert_eq!(sum_pd_chunks_in_access_list(&access_list), 42);
    }

    #[test]
    fn test_sum_pd_chunks_multiple_keys() {
        let access_list =
            build_pd_access_list(vec![chunk_spec(10), chunk_spec(20), chunk_spec(30)]);
        assert_eq!(sum_pd_chunks_in_access_list(&access_list), 60);
    }

    #[test]
    fn test_sum_pd_chunks_zero_chunks() {
        let access_list = build_pd_access_list(vec![chunk_spec(0), chunk_spec(0)]);
        assert_eq!(sum_pd_chunks_in_access_list(&access_list), 0);
    }

    // Complex Scenario Tests

    #[test]
    fn test_sum_pd_chunks_mixed_addresses() {
        let pd_spec = chunk_spec_with_params([3; 25], 123, 50);
        let pd_key = B256::from(pd_spec.encode());
        let access_list = AccessList::from(vec![
            AccessListItem {
                address: other_address(),
                storage_keys: vec![
                    B256::from(chunk_spec_with_params([1; 25], 0, 999).encode()), // This should be ignored
                ],
            },
            AccessListItem {
                address: PD_PRECOMPILE_ADDRESS,
                storage_keys: vec![pd_key],
            },
            AccessListItem {
                address: Address::repeat_byte(0xaa),
                storage_keys: vec![
                    B256::from(chunk_spec_with_params([2; 25], 0, 888).encode()), // This should be ignored
                ],
            },
        ]);
        // Only the PD precompile entry should be counted
        assert_eq!(sum_pd_chunks_in_access_list(&access_list), 50);
    }

    #[test]
    fn test_sum_pd_chunks_multiple_pd_entries() {
        let access_list = AccessList::from(vec![
            AccessListItem {
                address: PD_PRECOMPILE_ADDRESS,
                storage_keys: vec![
                    B256::from(chunk_spec_with_params([1; 25], 0, 10).encode()),
                    B256::from(chunk_spec_with_params([2; 25], 100, 15).encode()),
                ],
            },
            AccessListItem {
                address: PD_PRECOMPILE_ADDRESS,
                storage_keys: vec![B256::from(
                    chunk_spec_with_params([3; 25], 200, 20).encode(),
                )],
            },
        ]);
        // All PD entries should be summed: 10 + 15 + 20 = 45
        assert_eq!(sum_pd_chunks_in_access_list(&access_list), 45);
    }

    #[test]
    fn test_sum_pd_chunks_no_deduplication() {
        // Same key appears multiple times - should be counted multiple times
        let duplicate_key = B256::from(chunk_spec_with_params([5; 25], 42, 25).encode());
        let access_list = AccessList::from(vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![duplicate_key, duplicate_key, duplicate_key],
        }]);
        // 25 * 3 = 75 (no deduplication per spec)
        assert_eq!(sum_pd_chunks_in_access_list(&access_list), 75);
    }

    #[test]
    fn test_sum_pd_chunks_large_values() {
        let access_list = build_pd_access_list(vec![chunk_spec(u16::MAX), chunk_spec(u16::MAX)]);
        // 65535 * 2 = 131070
        assert_eq!(sum_pd_chunks_in_access_list(&access_list), 131_070);
    }

    #[test]
    fn test_sum_pd_chunks_max_single_key() {
        let access_list = build_pd_access_list(vec![chunk_spec(u16::MAX)]);
        assert_eq!(sum_pd_chunks_in_access_list(&access_list), 65_535);
    }
}
