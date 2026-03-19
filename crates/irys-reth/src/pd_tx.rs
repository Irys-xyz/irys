//! Utilities to construct and decode Programmable Data (PD) access list entries and transactions.

use std::collections::HashSet;

use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_primitives::{B256, U256};

use irys_types::precompile::PD_PRECOMPILE_ADDRESS;
use irys_types::range_specifier::{
    ByteRangeSpecifier, ChunkRangeSpecifier, PdAccessListArg, PdAccessListArgSerde as _,
    PdAccessListArgsTypeId, decode_pd_fee, encode_pd_fee,
};

/// Result of parsing a transaction's access list for PD metadata.
#[derive(Debug)]
pub enum PdParseResult {
    /// No PD keys found under `PD_PRECOMPILE_ADDRESS`. Not a PD transaction.
    NotPd,
    /// Valid PD transaction with extracted metadata.
    ValidPd(PdTransactionMeta),
    /// Keys found under `PD_PRECOMPILE_ADDRESS` but validation failed.
    InvalidPd(PdValidationError),
}

/// Extracted PD transaction metadata from a valid access list.
#[derive(Debug, Clone)]
pub struct PdTransactionMeta {
    pub data_reads: Vec<ChunkRangeSpecifier>,
    pub byte_reads: Vec<ByteRangeSpecifier>,
    pub priority_fee_per_chunk: U256,
    pub base_fee_cap_per_chunk: U256,
    pub total_chunks: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum PdValidationError {
    #[error("multiple AccessListItems targeting PD_PRECOMPILE_ADDRESS")]
    MultiplePdItems,
    #[error("duplicate PdPriorityFee key")]
    DuplicatePriorityFee,
    #[error("duplicate PdBaseFeeCap key")]
    DuplicateBaseFeeCap,
    #[error("missing PdPriorityFee key")]
    MissingPriorityFee,
    #[error("missing PdBaseFeeCap key")]
    MissingBaseFeeCap,
    #[error("no ChunkRead keys found")]
    NoDataReads,
    #[error("fee value is zero")]
    ZeroFee,
    #[error("fee value exceeds MAX_PD_FEE (u248)")]
    FeeOverflow,
    #[error("unknown type byte {0:#04x} in PD access list key")]
    UnknownTypeByte(u8),
    #[error("duplicate ChunkRead key")]
    DuplicateDataRead,
    #[error("invalid ChunkRead encoding: {0}")]
    InvalidDataRead(String),
    #[error("invalid ByteRead encoding: {0}")]
    InvalidByteRead(String),
    #[error("invalid fee encoding: {0}")]
    InvalidFeeEncoding(String),
}

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
                // Fee keys don't contribute to chunk count
                Ok(PdAccessListArg::PdPriorityFee(_) | PdAccessListArg::PdBaseFeeCap(_)) => Some(0),
                Err(e) => {
                    // Invalid encoding - log warning and skip
                    tracing::warn!("Invalid PD access list key encoding, skipping: {}", e);
                    None
                }
            }
        })
        .sum()
}

/// Extracts PD chunk range specifiers from a transaction's access list.
///
/// Returns all `ChunkRangeSpecifier` entries found under the PD precompile address.
/// Invalid encodings are logged as warnings and skipped.
pub fn extract_pd_chunk_specs(
    access_list: &alloy_eips::eip2930::AccessList,
) -> Vec<ChunkRangeSpecifier> {
    access_list
        .0
        .iter()
        .filter(|item| item.address == PD_PRECOMPILE_ADDRESS)
        .flat_map(|item| item.storage_keys.iter())
        .filter_map(|key| match PdAccessListArg::decode(&key.0) {
            Ok(PdAccessListArg::ChunkRead(spec)) => Some(spec),
            Ok(PdAccessListArg::ByteRead(_)) => None,
            Ok(PdAccessListArg::PdPriorityFee(_) | PdAccessListArg::PdBaseFeeCap(_)) => None,
            Err(e) => {
                tracing::warn!("Invalid PD access list key encoding, skipping: {}", e);
                None
            }
        })
        .collect()
}

/// Canonical PD transaction parser. Single entry point for all components.
///
/// Examines the transaction's access list for keys under `PD_PRECOMPILE_ADDRESS`.
/// Returns `NotPd` if no such keys exist, `InvalidPd` if validation fails,
/// or `ValidPd(meta)` with extracted fee parameters and chunk specifiers.
pub fn parse_pd_transaction(access_list: &AccessList) -> PdParseResult {
    // 1. Find AccessListItems targeting PD_PRECOMPILE_ADDRESS
    let pd_items: Vec<&AccessListItem> = access_list
        .0
        .iter()
        .filter(|item| item.address == PD_PRECOMPILE_ADDRESS)
        .collect();

    if pd_items.is_empty() {
        return PdParseResult::NotPd;
    }

    if pd_items.len() > 1 {
        return PdParseResult::InvalidPd(PdValidationError::MultiplePdItems);
    }

    let pd_item = pd_items[0];

    if pd_item.storage_keys.is_empty() {
        return PdParseResult::NotPd;
    }

    // 2. Decode all storage keys by type byte
    let mut chunk_reads = Vec::new();
    let mut byte_reads = Vec::new();
    let mut priority_fee: Option<U256> = None;
    let mut base_fee_cap: Option<U256> = None;
    let mut seen_chunk_reads: HashSet<[u8; 32]> = HashSet::new();

    for key in &pd_item.storage_keys {
        let type_byte = key.0[0];
        match PdAccessListArgsTypeId::try_from(type_byte) {
            Ok(PdAccessListArgsTypeId::ChunkRead) => match ChunkRangeSpecifier::decode(&key.0) {
                Ok(spec) => {
                    if !seen_chunk_reads.insert(key.0) {
                        return PdParseResult::InvalidPd(PdValidationError::DuplicateDataRead);
                    }
                    chunk_reads.push(spec);
                }
                Err(e) => {
                    return PdParseResult::InvalidPd(PdValidationError::InvalidDataRead(
                        e.to_string(),
                    ));
                }
            },
            Ok(PdAccessListArgsTypeId::ByteRead) => match ByteRangeSpecifier::decode(&key.0) {
                Ok(spec) => byte_reads.push(spec),
                Err(e) => {
                    return PdParseResult::InvalidPd(PdValidationError::InvalidByteRead(
                        e.to_string(),
                    ));
                }
            },
            Ok(PdAccessListArgsTypeId::PdPriorityFee) => {
                if priority_fee.is_some() {
                    return PdParseResult::InvalidPd(PdValidationError::DuplicatePriorityFee);
                }
                match decode_pd_fee(&key.0, PdAccessListArgsTypeId::PdPriorityFee as u8) {
                    Ok(fee) => priority_fee = Some(fee),
                    Err(e) => {
                        return PdParseResult::InvalidPd(PdValidationError::InvalidFeeEncoding(
                            e.to_string(),
                        ));
                    }
                }
            }
            Ok(PdAccessListArgsTypeId::PdBaseFeeCap) => {
                if base_fee_cap.is_some() {
                    return PdParseResult::InvalidPd(PdValidationError::DuplicateBaseFeeCap);
                }
                match decode_pd_fee(&key.0, PdAccessListArgsTypeId::PdBaseFeeCap as u8) {
                    Ok(fee) => base_fee_cap = Some(fee),
                    Err(e) => {
                        return PdParseResult::InvalidPd(PdValidationError::InvalidFeeEncoding(
                            e.to_string(),
                        ));
                    }
                }
            }
            Err(_) => {
                return PdParseResult::InvalidPd(PdValidationError::UnknownTypeByte(type_byte));
            }
        }
    }

    // 3. At least one ChunkRead required
    if chunk_reads.is_empty() {
        return PdParseResult::InvalidPd(PdValidationError::NoDataReads);
    }

    // 4. Both fee keys required
    let priority_fee = match priority_fee {
        Some(f) => f,
        None => return PdParseResult::InvalidPd(PdValidationError::MissingPriorityFee),
    };
    let base_fee_cap = match base_fee_cap {
        Some(f) => f,
        None => return PdParseResult::InvalidPd(PdValidationError::MissingBaseFeeCap),
    };

    // 5. Compute total chunks
    let total_chunks: u64 = chunk_reads.iter().map(|s| s.chunk_count as u64).sum();

    PdParseResult::ValidPd(PdTransactionMeta {
        data_reads: chunk_reads,
        byte_reads,
        priority_fee_per_chunk: priority_fee,
        base_fee_cap_per_chunk: base_fee_cap,
        total_chunks,
    })
}

/// Build a PD access list with chunk/byte specifiers AND fee parameters.
///
/// This is the canonical constructor for PD transaction access lists.
pub fn build_pd_access_list_with_fees(
    chunk_specs: impl IntoIterator<Item = ChunkRangeSpecifier>,
    byte_specs: impl IntoIterator<Item = ByteRangeSpecifier>,
    priority_fee_per_chunk: U256,
    base_fee_cap_per_chunk: U256,
) -> AccessList {
    let mut storage_keys: Vec<B256> = chunk_specs
        .into_iter()
        .map(|spec| B256::from(spec.encode()))
        .collect();
    storage_keys.extend(byte_specs.into_iter().map(|spec| B256::from(spec.encode())));
    storage_keys.push(B256::from(
        encode_pd_fee(
            PdAccessListArgsTypeId::PdPriorityFee as u8,
            priority_fee_per_chunk,
        )
        .expect("priority fee must be > 0 and <= MAX_PD_FEE"),
    ));
    storage_keys.push(B256::from(
        encode_pd_fee(
            PdAccessListArgsTypeId::PdBaseFeeCap as u8,
            base_fee_cap_per_chunk,
        )
        .expect("base fee cap must be > 0 and <= MAX_PD_FEE"),
    ));

    AccessList::from(vec![AccessListItem {
        address: PD_PRECOMPILE_ADDRESS,
        storage_keys,
    }])
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

    // ========== parse_pd_transaction tests ==========

    #[test]
    fn test_parse_not_pd_empty_access_list() {
        assert!(matches!(
            parse_pd_transaction(&AccessList::default()),
            PdParseResult::NotPd
        ));
    }

    #[test]
    fn test_parse_not_pd_no_pd_address() {
        let access_list = AccessList::from(vec![AccessListItem {
            address: other_address(),
            storage_keys: vec![B256::ZERO],
        }]);
        assert!(matches!(
            parse_pd_transaction(&access_list),
            PdParseResult::NotPd
        ));
    }

    #[test]
    fn test_parse_not_pd_empty_keys() {
        let access_list = AccessList::from(vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![],
        }]);
        assert!(matches!(
            parse_pd_transaction(&access_list),
            PdParseResult::NotPd
        ));
    }

    #[test]
    fn test_parse_valid_minimal() {
        let access_list = build_pd_access_list_with_fees(
            vec![chunk_spec(10)],
            std::iter::empty(),
            U256::from(1_000_000_u64),
            U256::from(5_000_000_u64),
        );
        match parse_pd_transaction(&access_list) {
            PdParseResult::ValidPd(meta) => {
                assert_eq!(meta.data_reads.len(), 1);
                assert_eq!(meta.byte_reads.len(), 0);
                assert_eq!(meta.total_chunks, 10);
                assert_eq!(meta.priority_fee_per_chunk, U256::from(1_000_000_u64));
                assert_eq!(meta.base_fee_cap_per_chunk, U256::from(5_000_000_u64));
            }
            other => panic!("expected ValidPd, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_valid_multiple_reads() {
        let access_list = build_pd_access_list_with_fees(
            vec![chunk_spec(10), chunk_spec(20), chunk_spec(30)],
            std::iter::empty(),
            U256::from(100_u64),
            U256::from(200_u64),
        );
        match parse_pd_transaction(&access_list) {
            PdParseResult::ValidPd(meta) => {
                assert_eq!(meta.data_reads.len(), 3);
                assert_eq!(meta.total_chunks, 60);
            }
            other => panic!("expected ValidPd, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_invalid_multiple_pd_items() {
        let access_list = AccessList::from(vec![
            AccessListItem {
                address: PD_PRECOMPILE_ADDRESS,
                storage_keys: vec![B256::from(chunk_spec(1).encode())],
            },
            AccessListItem {
                address: PD_PRECOMPILE_ADDRESS,
                storage_keys: vec![B256::from(chunk_spec(2).encode())],
            },
        ]);
        assert!(matches!(
            parse_pd_transaction(&access_list),
            PdParseResult::InvalidPd(PdValidationError::MultiplePdItems)
        ));
    }

    #[test]
    fn test_parse_invalid_missing_priority_fee() {
        // Only base fee cap, no priority fee
        let mut storage_keys: Vec<B256> = vec![B256::from(chunk_spec(5).encode())];
        storage_keys.push(B256::from(
            encode_pd_fee(
                PdAccessListArgsTypeId::PdBaseFeeCap as u8,
                U256::from(100_u64),
            )
            .expect("valid fee"),
        ));
        let access_list = AccessList::from(vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys,
        }]);
        assert!(matches!(
            parse_pd_transaction(&access_list),
            PdParseResult::InvalidPd(PdValidationError::MissingPriorityFee)
        ));
    }

    #[test]
    fn test_parse_invalid_missing_base_fee() {
        let mut storage_keys: Vec<B256> = vec![B256::from(chunk_spec(5).encode())];
        storage_keys.push(B256::from(
            encode_pd_fee(
                PdAccessListArgsTypeId::PdPriorityFee as u8,
                U256::from(100_u64),
            )
            .expect("valid fee"),
        ));
        let access_list = AccessList::from(vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys,
        }]);
        assert!(matches!(
            parse_pd_transaction(&access_list),
            PdParseResult::InvalidPd(PdValidationError::MissingBaseFeeCap)
        ));
    }

    #[test]
    fn test_parse_invalid_no_data_reads() {
        // Fee keys only, no ChunkRead
        let storage_keys = vec![
            B256::from(
                encode_pd_fee(
                    PdAccessListArgsTypeId::PdPriorityFee as u8,
                    U256::from(100_u64),
                )
                .expect("valid fee"),
            ),
            B256::from(
                encode_pd_fee(
                    PdAccessListArgsTypeId::PdBaseFeeCap as u8,
                    U256::from(200_u64),
                )
                .expect("valid fee"),
            ),
        ];
        let access_list = AccessList::from(vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys,
        }]);
        assert!(matches!(
            parse_pd_transaction(&access_list),
            PdParseResult::InvalidPd(PdValidationError::NoDataReads)
        ));
    }

    #[test]
    fn test_parse_invalid_unknown_type_byte() {
        let mut bad_key = [0_u8; 32];
        bad_key[0] = 0xFF; // Unknown type
        let mut storage_keys: Vec<B256> = vec![B256::from(chunk_spec(5).encode())];
        storage_keys.push(B256::from(bad_key));
        storage_keys.push(B256::from(
            encode_pd_fee(
                PdAccessListArgsTypeId::PdPriorityFee as u8,
                U256::from(100_u64),
            )
            .expect("valid fee"),
        ));
        storage_keys.push(B256::from(
            encode_pd_fee(
                PdAccessListArgsTypeId::PdBaseFeeCap as u8,
                U256::from(200_u64),
            )
            .expect("valid fee"),
        ));
        let access_list = AccessList::from(vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys,
        }]);
        assert!(matches!(
            parse_pd_transaction(&access_list),
            PdParseResult::InvalidPd(PdValidationError::UnknownTypeByte(0xFF))
        ));
    }

    #[test]
    fn test_parse_invalid_duplicate_priority_fee() {
        let mut storage_keys: Vec<B256> = vec![B256::from(chunk_spec(5).encode())];
        storage_keys.push(B256::from(
            encode_pd_fee(
                PdAccessListArgsTypeId::PdPriorityFee as u8,
                U256::from(100_u64),
            )
            .expect("valid fee"),
        ));
        storage_keys.push(B256::from(
            encode_pd_fee(
                PdAccessListArgsTypeId::PdPriorityFee as u8,
                U256::from(200_u64),
            )
            .expect("valid fee"),
        ));
        storage_keys.push(B256::from(
            encode_pd_fee(
                PdAccessListArgsTypeId::PdBaseFeeCap as u8,
                U256::from(300_u64),
            )
            .expect("valid fee"),
        ));
        let access_list = AccessList::from(vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys,
        }]);
        assert!(matches!(
            parse_pd_transaction(&access_list),
            PdParseResult::InvalidPd(PdValidationError::DuplicatePriorityFee)
        ));
    }

    #[test]
    fn test_parse_invalid_duplicate_chunk_read() {
        let spec = chunk_spec(5);
        let key = B256::from(spec.encode());
        let mut storage_keys: Vec<B256> = vec![key, key]; // duplicate
        storage_keys.push(B256::from(
            encode_pd_fee(
                PdAccessListArgsTypeId::PdPriorityFee as u8,
                U256::from(100_u64),
            )
            .expect("valid fee"),
        ));
        storage_keys.push(B256::from(
            encode_pd_fee(
                PdAccessListArgsTypeId::PdBaseFeeCap as u8,
                U256::from(200_u64),
            )
            .expect("valid fee"),
        ));
        let access_list = AccessList::from(vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys,
        }]);
        assert!(matches!(
            parse_pd_transaction(&access_list),
            PdParseResult::InvalidPd(PdValidationError::DuplicateDataRead)
        ));
    }

    #[test]
    fn test_parse_order_independent() {
        // Fee keys first, then data reads
        let storage_keys = vec![
            B256::from(
                encode_pd_fee(
                    PdAccessListArgsTypeId::PdBaseFeeCap as u8,
                    U256::from(500_u64),
                )
                .expect("valid fee"),
            ),
            B256::from(
                encode_pd_fee(
                    PdAccessListArgsTypeId::PdPriorityFee as u8,
                    U256::from(100_u64),
                )
                .expect("valid fee"),
            ),
            B256::from(chunk_spec(7).encode()),
        ];
        let access_list = AccessList::from(vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys,
        }]);
        match parse_pd_transaction(&access_list) {
            PdParseResult::ValidPd(meta) => {
                assert_eq!(meta.total_chunks, 7);
                assert_eq!(meta.priority_fee_per_chunk, U256::from(100_u64));
                assert_eq!(meta.base_fee_cap_per_chunk, U256::from(500_u64));
            }
            other => panic!("expected ValidPd, got {other:?}"),
        }
    }

    #[test]
    fn test_build_and_parse_roundtrip() {
        let prio = U256::from(42_000_u64);
        let base = U256::from(99_000_u64);
        let access_list = build_pd_access_list_with_fees(
            vec![chunk_spec(3), chunk_spec(7)],
            std::iter::empty(),
            prio,
            base,
        );
        match parse_pd_transaction(&access_list) {
            PdParseResult::ValidPd(meta) => {
                assert_eq!(meta.total_chunks, 10);
                assert_eq!(meta.priority_fee_per_chunk, prio);
                assert_eq!(meta.base_fee_cap_per_chunk, base);
                assert_eq!(meta.data_reads.len(), 2);
            }
            other => panic!("expected ValidPd, got {other:?}"),
        }
    }
}
