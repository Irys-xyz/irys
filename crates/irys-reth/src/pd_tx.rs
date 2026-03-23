//! Utilities to construct and decode Programmable Data (PD) access list entries and transactions.

use std::collections::HashSet;

use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_primitives::{B256, U256};

use irys_types::chunk_provider::ChunkConfig;
use irys_types::precompile::PD_PRECOMPILE_ADDRESS;
use irys_types::range_specifier::{
    PdAccessListArgsTypeId, PdDataRead, PdFeeDecodeError, decode_pd_fee, encode_pd_fee,
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
    pub data_reads: Vec<PdDataRead>,
    pub priority_fee_per_chunk: U256,
    pub base_fee_cap_per_chunk: U256,
    pub total_chunks: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum PdValidationError {
    #[error("multiple AccessListItems targeting PD_PRECOMPILE_ADDRESS")]
    MultiplePdItems,
    #[error("no DataRead keys found")]
    NoDataReads,
    #[error("missing PdPriorityFee key")]
    MissingPriorityFee,
    #[error("missing PdBaseFeeCap key")]
    MissingBaseFeeCap,
    #[error("duplicate DataRead key")]
    DuplicateDataReadKey,
    #[error("duplicate PdPriorityFee key")]
    DuplicatePriorityFee,
    #[error("duplicate PdBaseFeeCap key")]
    DuplicateBaseFeeCap,
    #[error("invalid DataRead encoding: {reason}")]
    InvalidDataReadEncoding { reason: String },
    #[error("invalid fee encoding ({key_type}): {reason}")]
    InvalidFeeEncoding {
        key_type: &'static str,
        reason: String,
    },
    #[error("unknown type byte {0:#04x} in PD access list key")]
    UnknownTypeByte(u8),
}

/// Create a PD access list for a list of `PdDataRead` specs, under the PD precompile address.
///
/// This is the canonical way to build PD access lists for testing and transaction construction.
pub fn build_pd_access_list(specs: &[PdDataRead]) -> AccessList {
    let keys: Vec<B256> = specs.iter().map(|s| B256::from(s.encode())).collect();
    AccessList::from(vec![AccessListItem {
        address: PD_PRECOMPILE_ADDRESS,
        storage_keys: keys,
    }])
}

/// Build a PD access list with data read specifiers AND fee parameters.
///
/// This is the canonical constructor for PD transaction access lists.
pub fn build_pd_access_list_with_fees(
    data_reads: &[PdDataRead],
    priority_fee: U256,
    base_fee_cap: U256,
) -> eyre::Result<AccessList> {
    let mut keys: Vec<B256> = data_reads.iter().map(|s| B256::from(s.encode())).collect();
    keys.push(B256::from(encode_pd_fee(
        PdAccessListArgsTypeId::PdPriorityFee as u8,
        priority_fee,
    )?));
    keys.push(B256::from(encode_pd_fee(
        PdAccessListArgsTypeId::PdBaseFeeCap as u8,
        base_fee_cap,
    )?));
    Ok(AccessList::from(vec![AccessListItem {
        address: PD_PRECOMPILE_ADDRESS,
        storage_keys: keys,
    }]))
}

/// Compute total PD chunks referenced in an access list (simple sum, no deduplication).
///
/// Decodes access list keys using the canonical `PdDataRead` encoding.
/// Fee keys are skipped. Invalid encodings are logged as warnings and skipped.
pub fn sum_pd_chunks_in_access_list(access_list: &AccessList, chunk_config: &ChunkConfig) -> u64 {
    access_list
        .0
        .iter()
        .filter(|item| item.address == PD_PRECOMPILE_ADDRESS)
        .flat_map(|item| item.storage_keys.iter())
        .filter_map(|key| {
            let type_byte = key.0[0];
            match PdAccessListArgsTypeId::try_from(type_byte) {
                Ok(PdAccessListArgsTypeId::DataRead) => {
                    match PdDataRead::decode(&key.0, chunk_config) {
                        Ok(spec) => Some(spec.chunks_needed(chunk_config.chunk_size)),
                        Err(e) => {
                            tracing::warn!(
                                "Invalid PdDataRead encoding in access list, skipping: {e}"
                            );
                            None
                        }
                    }
                }
                // Fee keys don't contribute to chunk count
                Ok(
                    PdAccessListArgsTypeId::PdPriorityFee | PdAccessListArgsTypeId::PdBaseFeeCap,
                ) => Some(0),
                Err(e) => {
                    tracing::warn!("Invalid PD access list key encoding, skipping: {e}");
                    None
                }
            }
        })
        .sum()
}

/// Extracts PD data read specifiers from a transaction's access list.
///
/// Returns all `PdDataRead` entries found under the PD precompile address.
/// Fee keys and invalid encodings are skipped (invalid ones logged as warnings).
pub fn extract_pd_chunk_specs(
    access_list: &AccessList,
    chunk_config: &ChunkConfig,
) -> Vec<PdDataRead> {
    access_list
        .0
        .iter()
        .filter(|item| item.address == PD_PRECOMPILE_ADDRESS)
        .flat_map(|item| item.storage_keys.iter())
        .filter_map(|key| {
            let type_byte = key.0[0];
            match PdAccessListArgsTypeId::try_from(type_byte) {
                Ok(PdAccessListArgsTypeId::DataRead) => {
                    match PdDataRead::decode(&key.0, chunk_config) {
                        Ok(spec) => Some(spec),
                        Err(e) => {
                            tracing::warn!(
                                "Invalid PdDataRead encoding in access list, skipping: {e}"
                            );
                            None
                        }
                    }
                }
                Ok(
                    PdAccessListArgsTypeId::PdPriorityFee | PdAccessListArgsTypeId::PdBaseFeeCap,
                ) => None,
                Err(e) => {
                    tracing::warn!("Invalid PD access list key encoding, skipping: {e}");
                    None
                }
            }
        })
        .collect()
}

/// Map a [`PdFeeDecodeError`] to the corresponding [`PdValidationError`].
fn pd_fee_decode_error_to_validation(
    e: PdFeeDecodeError,
    key_type: &'static str,
) -> PdValidationError {
    PdValidationError::InvalidFeeEncoding {
        key_type,
        reason: e.to_string(),
    }
}

/// Canonical PD transaction parser. Single entry point for all components.
///
/// Examines the transaction's access list for keys under `PD_PRECOMPILE_ADDRESS`.
/// Returns `NotPd` if no such keys exist, `InvalidPd` if validation fails,
/// or `ValidPd(meta)` with extracted fee parameters and data read specifiers.
pub fn parse_pd_transaction(access_list: &AccessList, chunk_config: &ChunkConfig) -> PdParseResult {
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
    let mut data_reads = Vec::new();
    let mut priority_fee: Option<U256> = None;
    let mut base_fee_cap: Option<U256> = None;
    let mut seen_data_reads: HashSet<[u8; 32]> = HashSet::new();

    for key in &pd_item.storage_keys {
        let type_byte = key.0[0];
        match PdAccessListArgsTypeId::try_from(type_byte) {
            Ok(PdAccessListArgsTypeId::DataRead) => {
                match PdDataRead::decode(&key.0, chunk_config) {
                    Ok(spec) => {
                        if !seen_data_reads.insert(key.0) {
                            return PdParseResult::InvalidPd(
                                PdValidationError::DuplicateDataReadKey,
                            );
                        }
                        data_reads.push(spec);
                    }
                    Err(e) => {
                        return PdParseResult::InvalidPd(
                            PdValidationError::InvalidDataReadEncoding {
                                reason: e.to_string(),
                            },
                        );
                    }
                }
            }
            Ok(PdAccessListArgsTypeId::PdPriorityFee) => {
                if priority_fee.is_some() {
                    return PdParseResult::InvalidPd(PdValidationError::DuplicatePriorityFee);
                }
                match decode_pd_fee(&key.0, PdAccessListArgsTypeId::PdPriorityFee as u8) {
                    Ok(fee) => priority_fee = Some(fee),
                    Err(e) => {
                        return PdParseResult::InvalidPd(pd_fee_decode_error_to_validation(
                            e,
                            "PdPriorityFee",
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
                        return PdParseResult::InvalidPd(pd_fee_decode_error_to_validation(
                            e,
                            "PdBaseFeeCap",
                        ));
                    }
                }
            }
            Err(_) => {
                return PdParseResult::InvalidPd(PdValidationError::UnknownTypeByte(type_byte));
            }
        }
    }

    // 3. At least one DataRead required
    if data_reads.is_empty() {
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
    let total_chunks: u64 = data_reads
        .iter()
        .map(|r| r.chunks_needed(chunk_config.chunk_size))
        .sum();

    PdParseResult::ValidPd(PdTransactionMeta {
        data_reads,
        priority_fee_per_chunk: priority_fee,
        base_fee_cap_per_chunk: base_fee_cap,
        total_chunks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::data_read_with_params;
    use alloy_primitives::Address;

    fn test_chunk_config() -> ChunkConfig {
        ChunkConfig {
            chunk_size: 262_144, // 256 KB
            num_chunks_in_partition: 51_872_000,
            entropy_packing_iterations: 0,
            chain_id: 1,
        }
    }

    fn other_address() -> Address {
        Address::repeat_byte(0xff)
    }

    /// Create a `PdDataRead` reading `len` bytes from partition 0, start 0, byte_off 0.
    /// `chunks_needed` will be `ceil(len / chunk_size)`.
    fn data_read(len: u32) -> PdDataRead {
        data_read_with_params(0, 0, len, 0)
    }

    // ========== sum_pd_chunks_in_access_list tests ==========

    #[test]
    fn test_sum_pd_chunks_empty_access_list() {
        let access_list = AccessList::default();
        assert_eq!(
            sum_pd_chunks_in_access_list(&access_list, &test_chunk_config()),
            0
        );
    }

    #[test]
    fn test_sum_pd_chunks_no_pd_precompile() {
        let access_list = AccessList::from(vec![AccessListItem {
            address: other_address(),
            storage_keys: vec![B256::ZERO, B256::repeat_byte(0x01)],
        }]);
        assert_eq!(
            sum_pd_chunks_in_access_list(&access_list, &test_chunk_config()),
            0
        );
    }

    #[test]
    fn test_sum_pd_chunks_no_storage_keys() {
        let access_list = AccessList::from(vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![],
        }]);
        assert_eq!(
            sum_pd_chunks_in_access_list(&access_list, &test_chunk_config()),
            0
        );
    }

    #[test]
    fn test_sum_pd_chunks_single_key() {
        // 262_144 bytes = exactly 1 chunk
        let access_list = build_pd_access_list(&[data_read(262_144)]);
        assert_eq!(
            sum_pd_chunks_in_access_list(&access_list, &test_chunk_config()),
            1
        );
    }

    #[test]
    fn test_sum_pd_chunks_multiple_keys() {
        // Three reads: 1 chunk + 1 chunk + 1 chunk = 3 chunks
        let access_list = build_pd_access_list(&[
            data_read_with_params(0, 0, 100, 0),
            data_read_with_params(1, 0, 100, 0),
            data_read_with_params(2, 0, 100, 0),
        ]);
        assert_eq!(
            sum_pd_chunks_in_access_list(&access_list, &test_chunk_config()),
            3
        );
    }

    #[test]
    fn test_sum_pd_chunks_spanning_multiple_chunks() {
        // byte_off=262_100, len=100 => total_bytes = 262_200, ceil(262_200/262_144) = 2 chunks
        let access_list = build_pd_access_list(&[data_read_with_params(0, 0, 100, 262_100)]);
        assert_eq!(
            sum_pd_chunks_in_access_list(&access_list, &test_chunk_config()),
            2
        );
    }

    #[test]
    fn test_sum_pd_chunks_mixed_addresses() {
        let config = test_chunk_config();
        let pd_spec = data_read_with_params(3, 123, 500, 0);
        let pd_key = B256::from(pd_spec.encode());
        let access_list = AccessList::from(vec![
            AccessListItem {
                address: other_address(),
                storage_keys: vec![
                    B256::from(data_read_with_params(1, 0, 999, 0).encode()), // Ignored
                ],
            },
            AccessListItem {
                address: PD_PRECOMPILE_ADDRESS,
                storage_keys: vec![pd_key],
            },
            AccessListItem {
                address: Address::repeat_byte(0xaa),
                storage_keys: vec![
                    B256::from(data_read_with_params(2, 0, 888, 0).encode()), // Ignored
                ],
            },
        ]);
        // Only the PD precompile entry should be counted: 1 chunk
        assert_eq!(sum_pd_chunks_in_access_list(&access_list, &config), 1);
    }

    #[test]
    fn test_sum_pd_chunks_multiple_pd_entries() {
        let config = test_chunk_config();
        // Two AccessListItems targeting PD address
        let access_list = AccessList::from(vec![
            AccessListItem {
                address: PD_PRECOMPILE_ADDRESS,
                storage_keys: vec![
                    B256::from(data_read_with_params(1, 0, 100, 0).encode()),
                    B256::from(data_read_with_params(2, 100, 100, 0).encode()),
                ],
            },
            AccessListItem {
                address: PD_PRECOMPILE_ADDRESS,
                storage_keys: vec![B256::from(data_read_with_params(3, 200, 100, 0).encode())],
            },
        ]);
        // All PD entries should be summed: 1 + 1 + 1 = 3 chunks
        assert_eq!(sum_pd_chunks_in_access_list(&access_list, &config), 3);
    }

    #[test]
    fn test_sum_pd_chunks_no_deduplication() {
        let config = test_chunk_config();
        // Same key appears multiple times - should be counted multiple times
        let duplicate_key = B256::from(data_read_with_params(5, 42, 100, 0).encode());
        let access_list = AccessList::from(vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![duplicate_key, duplicate_key, duplicate_key],
        }]);
        // 1 chunk * 3 = 3
        assert_eq!(sum_pd_chunks_in_access_list(&access_list, &config), 3);
    }

    #[test]
    fn test_sum_pd_chunks_large_read() {
        let config = test_chunk_config();
        // len = 262_144 * 10 = 2_621_440 bytes = 10 chunks
        let access_list = build_pd_access_list(&[data_read(262_144 * 10)]);
        assert_eq!(sum_pd_chunks_in_access_list(&access_list, &config), 10);
    }

    // ========== parse_pd_transaction tests ==========

    #[test]
    fn test_parse_not_pd_empty_access_list() {
        assert!(matches!(
            parse_pd_transaction(&AccessList::default(), &test_chunk_config()),
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
            parse_pd_transaction(&access_list, &test_chunk_config()),
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
            parse_pd_transaction(&access_list, &test_chunk_config()),
            PdParseResult::NotPd
        ));
    }

    #[test]
    fn test_parse_valid_minimal() {
        let config = test_chunk_config();
        let spec = data_read(262_144); // exactly 1 chunk
        let access_list = build_pd_access_list_with_fees(
            &[spec],
            U256::from(1_000_000_u64),
            U256::from(5_000_000_u64),
        )
        .expect("valid fees");
        match parse_pd_transaction(&access_list, &config) {
            PdParseResult::ValidPd(meta) => {
                assert_eq!(meta.data_reads.len(), 1);
                assert_eq!(meta.total_chunks, 1);
                assert_eq!(meta.priority_fee_per_chunk, U256::from(1_000_000_u64));
                assert_eq!(meta.base_fee_cap_per_chunk, U256::from(5_000_000_u64));
            }
            other => panic!("expected ValidPd, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_valid_multiple_reads() {
        let config = test_chunk_config();
        let reads = [
            data_read_with_params(0, 0, 100, 0), // 1 chunk
            data_read_with_params(1, 0, 100, 0), // 1 chunk
            data_read_with_params(2, 0, 100, 0), // 1 chunk
        ];
        let access_list =
            build_pd_access_list_with_fees(&reads, U256::from(100_u64), U256::from(200_u64))
                .expect("valid fees");
        match parse_pd_transaction(&access_list, &config) {
            PdParseResult::ValidPd(meta) => {
                assert_eq!(meta.data_reads.len(), 3);
                assert_eq!(meta.total_chunks, 3);
            }
            other => panic!("expected ValidPd, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_valid_total_chunks_with_byte_offset() {
        let config = test_chunk_config();
        // byte_off=262_100, len=100 => total_bytes=262_200, ceil(262_200/262_144) = 2 chunks
        let reads = [data_read_with_params(0, 0, 100, 262_100)];
        let access_list =
            build_pd_access_list_with_fees(&reads, U256::from(100_u64), U256::from(200_u64))
                .expect("valid fees");
        match parse_pd_transaction(&access_list, &config) {
            PdParseResult::ValidPd(meta) => {
                assert_eq!(meta.total_chunks, 2);
            }
            other => panic!("expected ValidPd, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_invalid_multiple_pd_items() {
        let config = test_chunk_config();
        let access_list = AccessList::from(vec![
            AccessListItem {
                address: PD_PRECOMPILE_ADDRESS,
                storage_keys: vec![B256::from(data_read(100).encode())],
            },
            AccessListItem {
                address: PD_PRECOMPILE_ADDRESS,
                storage_keys: vec![B256::from(data_read(200).encode())],
            },
        ]);
        assert!(matches!(
            parse_pd_transaction(&access_list, &config),
            PdParseResult::InvalidPd(PdValidationError::MultiplePdItems)
        ));
    }

    #[test]
    fn test_parse_invalid_missing_priority_fee() {
        let config = test_chunk_config();
        let mut storage_keys: Vec<B256> = vec![B256::from(data_read(100).encode())];
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
            parse_pd_transaction(&access_list, &config),
            PdParseResult::InvalidPd(PdValidationError::MissingPriorityFee)
        ));
    }

    #[test]
    fn test_parse_invalid_missing_base_fee() {
        let config = test_chunk_config();
        let mut storage_keys: Vec<B256> = vec![B256::from(data_read(100).encode())];
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
            parse_pd_transaction(&access_list, &config),
            PdParseResult::InvalidPd(PdValidationError::MissingBaseFeeCap)
        ));
    }

    #[test]
    fn test_parse_invalid_no_data_reads() {
        let config = test_chunk_config();
        // Fee keys only, no DataRead
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
            parse_pd_transaction(&access_list, &config),
            PdParseResult::InvalidPd(PdValidationError::NoDataReads)
        ));
    }

    #[test]
    fn test_parse_invalid_unknown_type_byte() {
        let config = test_chunk_config();
        let mut bad_key = [0_u8; 32];
        bad_key[0] = 0xFF; // Unknown type
        let mut storage_keys: Vec<B256> = vec![B256::from(data_read(100).encode())];
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
            parse_pd_transaction(&access_list, &config),
            PdParseResult::InvalidPd(PdValidationError::UnknownTypeByte(0xFF))
        ));
    }

    #[test]
    fn test_parse_invalid_duplicate_priority_fee() {
        let config = test_chunk_config();
        let mut storage_keys: Vec<B256> = vec![B256::from(data_read(100).encode())];
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
            parse_pd_transaction(&access_list, &config),
            PdParseResult::InvalidPd(PdValidationError::DuplicatePriorityFee)
        ));
    }

    #[test]
    fn test_parse_invalid_duplicate_data_read() {
        let config = test_chunk_config();
        let spec = data_read(100);
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
            parse_pd_transaction(&access_list, &config),
            PdParseResult::InvalidPd(PdValidationError::DuplicateDataReadKey)
        ));
    }

    #[test]
    fn test_parse_invalid_data_read_encoding() {
        let config = test_chunk_config();
        // Type byte = 0x01 (DataRead) but len = 0 which is invalid
        let mut bad_data_read = [0_u8; 32];
        bad_data_read[0] = 0x01; // DataRead type byte
        // partition_index, start, len all zero; len=0 is invalid
        let mut storage_keys: Vec<B256> = vec![B256::from(bad_data_read)];
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
        match parse_pd_transaction(&access_list, &config) {
            PdParseResult::InvalidPd(PdValidationError::InvalidDataReadEncoding { reason }) => {
                assert!(
                    reason.contains("len must be > 0"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected InvalidPd(InvalidDataReadEncoding), got {other:?}"),
        }
    }

    #[test]
    fn test_parse_order_independent() {
        let config = test_chunk_config();
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
            B256::from(data_read(100).encode()), // 1 chunk
        ];
        let access_list = AccessList::from(vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys,
        }]);
        match parse_pd_transaction(&access_list, &config) {
            PdParseResult::ValidPd(meta) => {
                assert_eq!(meta.total_chunks, 1);
                assert_eq!(meta.priority_fee_per_chunk, U256::from(100_u64));
                assert_eq!(meta.base_fee_cap_per_chunk, U256::from(500_u64));
            }
            other => panic!("expected ValidPd, got {other:?}"),
        }
    }

    #[test]
    fn test_build_and_parse_roundtrip() {
        let config = test_chunk_config();
        let prio = U256::from(42_000_u64);
        let base = U256::from(99_000_u64);
        let reads = [
            data_read_with_params(0, 0, 100, 0), // 1 chunk
            data_read_with_params(1, 0, 100, 0), // 1 chunk
        ];
        let access_list = build_pd_access_list_with_fees(&reads, prio, base).expect("valid fees");
        match parse_pd_transaction(&access_list, &config) {
            PdParseResult::ValidPd(meta) => {
                assert_eq!(meta.total_chunks, 2);
                assert_eq!(meta.priority_fee_per_chunk, prio);
                assert_eq!(meta.base_fee_cap_per_chunk, base);
                assert_eq!(meta.data_reads.len(), 2);
            }
            other => panic!("expected ValidPd, got {other:?}"),
        }
    }

    // ========== extract_pd_chunk_specs tests ==========

    #[test]
    fn test_extract_pd_chunk_specs_basic() {
        let config = test_chunk_config();
        let reads = [
            data_read_with_params(0, 0, 100, 0),
            data_read_with_params(1, 10, 200, 50),
        ];
        let access_list =
            build_pd_access_list_with_fees(&reads, U256::from(100_u64), U256::from(200_u64))
                .expect("valid fees");
        let extracted = extract_pd_chunk_specs(&access_list, &config);
        assert_eq!(extracted.len(), 2);
        assert_eq!(extracted[0], reads[0]);
        assert_eq!(extracted[1], reads[1]);
    }

    #[test]
    fn test_extract_pd_chunk_specs_empty() {
        let config = test_chunk_config();
        let access_list = AccessList::default();
        let extracted = extract_pd_chunk_specs(&access_list, &config);
        assert!(extracted.is_empty());
    }

    #[test]
    fn test_extract_pd_chunk_specs_skips_non_pd() {
        let config = test_chunk_config();
        let access_list = AccessList::from(vec![AccessListItem {
            address: other_address(),
            storage_keys: vec![B256::from(data_read(100).encode())],
        }]);
        let extracted = extract_pd_chunk_specs(&access_list, &config);
        assert!(extracted.is_empty());
    }

    // ========== build_pd_access_list tests ==========

    #[test]
    fn test_build_pd_access_list_basic() {
        let reads = [
            data_read_with_params(0, 0, 100, 0),
            data_read_with_params(1, 5, 200, 10),
        ];
        let access_list = build_pd_access_list(&reads);
        assert_eq!(access_list.0.len(), 1);
        assert_eq!(access_list.0[0].address, PD_PRECOMPILE_ADDRESS);
        assert_eq!(access_list.0[0].storage_keys.len(), 2);
    }

    #[test]
    fn test_build_pd_access_list_with_fees_structure() {
        let reads = [data_read(100)];
        let access_list =
            build_pd_access_list_with_fees(&reads, U256::from(42_u64), U256::from(99_u64))
                .expect("valid fees");
        assert_eq!(access_list.0.len(), 1);
        assert_eq!(access_list.0[0].address, PD_PRECOMPILE_ADDRESS);
        // 1 data read + 2 fee keys = 3
        assert_eq!(access_list.0[0].storage_keys.len(), 3);
    }
}
