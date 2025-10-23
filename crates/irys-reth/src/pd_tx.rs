//! Utilities to construct and decode Programmable Data (PD) access list entries and transactions.

use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_primitives::{Bytes, B256, U256};
use borsh::{BorshDeserialize, BorshSerialize};
use irys_types::precompile::PD_PRECOMPILE_ADDRESS;
use std::io::{Read, Write};

/// PD storage key components extracted from a 32-byte access-list key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdKey {
    pub slot_index_be: [u8; 26],
    pub offset: u32,
    pub chunk_count: u16,
}

/// Encode a PD storage key `<slot_index:26><offset:4><chunk_count:2>` into a 32-byte big-endian word.
pub fn encode_pd_storage_key(slot_index_be: [u8; 26], offset: u32, chunk_count: u16) -> B256 {
    let mut buf = [0_u8; 32];
    buf[0..26].copy_from_slice(&slot_index_be);
    buf[26..30].copy_from_slice(&offset.to_be_bytes());
    buf[30..32].copy_from_slice(&chunk_count.to_be_bytes());
    B256::from(buf)
}

/// Decode a PD storage key from a 32-byte big-endian word.
pub fn decode_pd_storage_key(key: B256) -> PdKey {
    let bytes = key.0;
    let mut slot = [0_u8; 26];
    slot.copy_from_slice(&bytes[0..26]);
    let mut off = [0_u8; 4];
    off.copy_from_slice(&bytes[26..30]);
    let mut cnt = [0_u8; 2];
    cnt.copy_from_slice(&bytes[30..32]);
    PdKey {
        slot_index_be: slot,
        offset: u32::from_be_bytes(off),
        chunk_count: u16::from_be_bytes(cnt),
    }
}

/// Create a PD access list for a list of PD keys, under the PD precompile address.
pub fn build_pd_access_list(keys: impl IntoIterator<Item = PdKey>) -> AccessList {
    let storage_keys: Vec<B256> = keys
        .into_iter()
        .map(|k| encode_pd_storage_key(k.slot_index_be, k.offset, k.chunk_count))
        .collect();
    AccessList::from(vec![AccessListItem {
        address: PD_PRECOMPILE_ADDRESS,
        storage_keys,
    }])
}

/// Compute total PD chunks referenced in an access list (simple sum, no deduplication).
pub fn sum_pd_chunks_in_access_list(access_list: &AccessList) -> u64 {
    access_list
        .0
        .iter()
        .filter(|item| item.address == PD_PRECOMPILE_ADDRESS)
        .flat_map(|item| item.storage_keys.iter())
        .map(|key| decode_pd_storage_key(*key).chunk_count as u64)
        .sum()
}

/// Magic prefix to identify PD metadata header in transaction calldata.
/// Length is fixed to avoid ambiguity and aid quick detection.
pub const IRYS_PD_HEADER_MAGIC: &[u8; 12] = b"irys-pd-meta";

/// PD header version values.
pub const PD_HEADER_VERSION_V1: u16 = 1;

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
        writer.write_all(&self.max_priority_fee_per_chunk.to_be_bytes::<32>())?;
        // U256 be (32 bytes) max base per chunk
        writer.write_all(&self.max_base_fee_per_chunk.to_be_bytes::<32>())?;
        Ok(())
    }
}

impl BorshDeserialize for PdHeaderV1 {
    fn deserialize_reader<R: Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let mut prio_buf = [0_u8; 32];
        reader.read_exact(&mut prio_buf)?;
        let max_priority_fee_per_chunk = U256::from_be_bytes(prio_buf);

        let mut base_buf = [0_u8; 32];
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
    let mut out = Vec::with_capacity(IRYS_PD_HEADER_MAGIC.len() + 2 + 32 + 32 + rest.len());
    out.extend_from_slice(IRYS_PD_HEADER_MAGIC);
    out.extend_from_slice(&PD_HEADER_VERSION_V1.to_be_bytes());
    let mut buf = Vec::with_capacity(32 + 32);
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
    if input.len() < magic_len + 2 {
        return Ok(None);
    }
    if &input[..magic_len] != IRYS_PD_HEADER_MAGIC {
        return Ok(None);
    }

    // parse version (u16 be)
    let ver_bytes = [input[magic_len], input[magic_len + 1]];
    let version = u16::from_be_bytes(ver_bytes);
    if version != PD_HEADER_VERSION_V1 {
        return Err(borsh::io::Error::new(
            borsh::io::ErrorKind::InvalidData,
            format!("unsupported PD header version: {}", version),
        ));
    }

    // Decode fixed-size V1 header
    let hdr_start = magic_len + 2;
    let mut rdr = &input[hdr_start..];
    let header = PdHeaderV1::deserialize_reader(&mut rdr)?;
    let consumed = input.len() - rdr.len();
    Ok(Some((header, consumed)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;

    fn other_address() -> Address {
        Address::repeat_byte(0xff)
    }

    fn pd_key(chunk_count: u16) -> PdKey {
        PdKey {
            slot_index_be: [0; 26],
            offset: 0,
            chunk_count,
        }
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
        let access_list = build_pd_access_list(vec![pd_key(42)]);
        assert_eq!(sum_pd_chunks_in_access_list(&access_list), 42);
    }

    #[test]
    fn test_sum_pd_chunks_multiple_keys() {
        let access_list = build_pd_access_list(vec![pd_key(10), pd_key(20), pd_key(30)]);
        assert_eq!(sum_pd_chunks_in_access_list(&access_list), 60);
    }

    #[test]
    fn test_sum_pd_chunks_zero_chunks() {
        let access_list = build_pd_access_list(vec![pd_key(0), pd_key(0)]);
        assert_eq!(sum_pd_chunks_in_access_list(&access_list), 0);
    }

    // Complex Scenario Tests

    #[test]
    fn test_sum_pd_chunks_mixed_addresses() {
        let pd_key = encode_pd_storage_key([3; 26], 123, 50);
        let access_list = AccessList::from(vec![
            AccessListItem {
                address: other_address(),
                storage_keys: vec![
                    encode_pd_storage_key([1; 26], 0, 999), // This should be ignored
                ],
            },
            AccessListItem {
                address: PD_PRECOMPILE_ADDRESS,
                storage_keys: vec![pd_key],
            },
            AccessListItem {
                address: Address::repeat_byte(0xaa),
                storage_keys: vec![
                    encode_pd_storage_key([2; 26], 0, 888), // This should be ignored
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
                    encode_pd_storage_key([1; 26], 0, 10),
                    encode_pd_storage_key([2; 26], 100, 15),
                ],
            },
            AccessListItem {
                address: PD_PRECOMPILE_ADDRESS,
                storage_keys: vec![encode_pd_storage_key([3; 26], 200, 20)],
            },
        ]);
        // All PD entries should be summed: 10 + 15 + 20 = 45
        assert_eq!(sum_pd_chunks_in_access_list(&access_list), 45);
    }

    #[test]
    fn test_sum_pd_chunks_no_deduplication() {
        // Same key appears multiple times - should be counted multiple times
        let duplicate_key = encode_pd_storage_key([5; 26], 42, 25);
        let access_list = AccessList::from(vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![duplicate_key, duplicate_key, duplicate_key],
        }]);
        // 25 * 3 = 75 (no deduplication per spec)
        assert_eq!(sum_pd_chunks_in_access_list(&access_list), 75);
    }

    #[test]
    fn test_sum_pd_chunks_large_values() {
        let access_list = build_pd_access_list(vec![pd_key(u16::MAX), pd_key(u16::MAX)]);
        // 65535 * 2 = 131070
        assert_eq!(sum_pd_chunks_in_access_list(&access_list), 131_070);
    }

    #[test]
    fn test_sum_pd_chunks_max_single_key() {
        let access_list = build_pd_access_list(vec![pd_key(u16::MAX)]);
        assert_eq!(sum_pd_chunks_in_access_list(&access_list), 65_535);
    }
}
