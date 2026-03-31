//! Utilities for parsing PD precompile access lists.

use alloy_eips::eip2930::AccessListItem;
use irys_types::chunk_provider::ChunkConfig;
use irys_types::precompile::PD_PRECOMPILE_ADDRESS;
use irys_types::range_specifier::PdDataRead;

/// Parse all [`PdDataRead`] specifiers from the access list.
///
/// Only keys addressed to [`PD_PRECOMPILE_ADDRESS`] are inspected. Fee keys
/// (`0x02`, `0x03`) are silently skipped (handled by `parse_pd_transaction`).
/// Any other unrecognised type byte is an error.
pub fn parse_pd_specifiers(
    access_list: &[AccessListItem],
    chunk_config: &ChunkConfig,
) -> eyre::Result<Vec<PdDataRead>> {
    let mut specifiers = Vec::new();
    for ali in access_list {
        if ali.address != PD_PRECOMPILE_ADDRESS {
            continue;
        }
        for key in &ali.storage_keys {
            let type_byte = key[0];
            match type_byte {
                0x01 => {
                    let bytes: &[u8; 32] = key.as_ref();
                    let spec = PdDataRead::decode(bytes, chunk_config)?;
                    specifiers.push(spec);
                }
                // Fee keys — skip, handled by parse_pd_transaction
                0x02 | 0x03 => {}
                _ => eyre::bail!("unknown access list key type byte: {:#04x}", type_byte),
            }
        }
    }
    Ok(specifiers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256};
    use irys_types::range_specifier::{PdAccessListArgsTypeId, encode_pd_fee};

    fn test_chunk_config() -> ChunkConfig {
        ChunkConfig {
            num_chunks_in_partition: 100,
            chunk_size: 256_000,
            entropy_packing_iterations: 0,
            chain_id: 1,
        }
    }

    #[test]
    fn test_empty_access_list_returns_empty() {
        let result = parse_pd_specifiers(&[], &test_chunk_config());
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }

    #[test]
    fn test_non_pd_addresses_are_filtered() {
        let wrong_address_item = AccessListItem {
            address: Address::from([1_u8; 20]),
            storage_keys: vec![B256::ZERO],
        };

        let result = parse_pd_specifiers(&[wrong_address_item], &test_chunk_config());
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }

    #[test]
    fn test_correct_address_with_no_keys_returns_empty() {
        let correct_address_no_keys = AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![],
        };

        let result = parse_pd_specifiers(&[correct_address_no_keys], &test_chunk_config());
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }

    #[test]
    fn test_unknown_type_byte_is_error() {
        let access_list = vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            // type byte 0xFF is unknown
            storage_keys: vec![B256::from([0xFF; 32])],
        }];

        let result = parse_pd_specifiers(&access_list, &test_chunk_config());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown access list key type byte"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_fee_keys_are_skipped() {
        use alloy_primitives::U256;

        let fee = U256::from(1_000_000_u64);
        let priority_fee_key = B256::from(
            encode_pd_fee(PdAccessListArgsTypeId::PdPriorityFee as u8, fee)
                .expect("test fee encoding"),
        );
        let base_fee_key = B256::from(
            encode_pd_fee(PdAccessListArgsTypeId::PdBaseFeeCap as u8, fee)
                .expect("test fee encoding"),
        );

        let access_list = vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![priority_fee_key, base_fee_key],
        }];

        let result = parse_pd_specifiers(&access_list, &test_chunk_config());
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }

    #[test]
    fn test_valid_data_read_key_is_parsed() {
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 100,
            byte_off: 0,
        };
        let key = B256::from(spec.encode());

        let access_list = vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![key],
        }];

        let result = parse_pd_specifiers(&access_list, &test_chunk_config());
        assert!(result.is_ok());
        let specifiers = result.unwrap();
        assert_eq!(specifiers.len(), 1);
        assert_eq!(specifiers[0], spec);
    }

    #[test]
    fn test_multiple_data_read_keys_preserve_order() {
        let spec_a = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 100,
            byte_off: 0,
        };
        let spec_b = PdDataRead {
            partition_index: 0,
            start: 5,
            len: 200,
            byte_off: 50,
        };
        let key_a = B256::from(spec_a.encode());
        let key_b = B256::from(spec_b.encode());

        let access_list = vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![key_a, key_b],
        }];

        let result = parse_pd_specifiers(&access_list, &test_chunk_config());
        assert!(result.is_ok());
        let specifiers = result.unwrap();
        assert_eq!(specifiers.len(), 2);
        assert_eq!(specifiers[0], spec_a);
        assert_eq!(specifiers[1], spec_b);
    }
}
