//! Utilities for parsing PD precompile access lists.

use alloy_eips::eip2930::AccessListItem;
use irys_types::precompile::PD_PRECOMPILE_ADDRESS;
use irys_types::range_specifier::{ByteRangeSpecifier, ChunkRangeSpecifier, PdAccessListArg};
use tracing::warn;

/// Parsed PD access list entries categorized by read type.
#[derive(Debug, Clone)]
pub struct ParsedAccessLists {
    pub chunk_reads: Vec<ChunkRangeSpecifier>,
    pub byte_reads: Vec<ByteRangeSpecifier>,
}

pub fn parse_access_list(access_list: &Vec<AccessListItem>) -> eyre::Result<ParsedAccessLists> {
    let mut parsed = ParsedAccessLists {
        chunk_reads: vec![],
        byte_reads: vec![],
    };
    for ali in access_list {
        if ali.address != PD_PRECOMPILE_ADDRESS {
            warn!("received unfiltered access list item {:?}", &ali);
            continue;
        }
        for key in &ali.storage_keys {
            match PdAccessListArg::decode(key) {
                Ok(dec) => match dec {
                    PdAccessListArg::ChunkRead(range_specifier) => {
                        parsed.chunk_reads.push(range_specifier)
                    }
                    PdAccessListArg::ByteRead(bytes_range_specifier) => {
                        parsed.byte_reads.push(bytes_range_specifier)
                    }
                },
                Err(_) => continue,
            }
        }
    }

    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256};

    #[test]
    fn test_empty_access_list_returns_empty_parsed() {
        let result = parse_access_list(&vec![]);
        assert!(result.is_ok());

        let parsed = result.unwrap();
        assert_eq!(parsed.chunk_reads.len(), 0);
        assert_eq!(parsed.byte_reads.len(), 0);
    }

    #[test]
    fn test_non_pd_addresses_are_filtered() {
        let wrong_address_item = AccessListItem {
            address: Address::from([1_u8; 20]),
            storage_keys: vec![B256::ZERO],
        };

        let result = parse_access_list(&vec![wrong_address_item]);
        assert!(result.is_ok());

        let parsed = result.unwrap();
        assert_eq!(parsed.chunk_reads.len(), 0);
        assert_eq!(parsed.byte_reads.len(), 0);
    }

    #[test]
    fn test_correct_address_with_no_valid_keys_returns_empty() {
        let correct_address_no_keys = AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![],
        };

        let result = parse_access_list(&vec![correct_address_no_keys]);
        assert!(result.is_ok());

        let parsed = result.unwrap();
        assert_eq!(parsed.chunk_reads.len(), 0);
        assert_eq!(parsed.byte_reads.len(), 0);
    }

    #[test]
    fn test_invalid_storage_keys_are_skipped() {
        let access_list = vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![B256::ZERO, B256::from([1_u8; 32]), B256::from([255_u8; 32])],
        }];

        let result = parse_access_list(&access_list);
        assert!(result.is_ok());
    }
}
