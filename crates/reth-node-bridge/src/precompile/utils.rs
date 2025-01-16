use alloy_rpc_types::AccessListItem;
use irys_primitives::range_specifier::{BytesRangeSpecifier, PdAccessListArg, RangeSpecifier};
use tracing::warn;

use super::entrypoint::PRECOMPILE_ADDRESS;

#[derive(Debug, Clone)]
pub struct ParsedAccessLists {
    pub chunk_reads: Vec<RangeSpecifier>,
    pub byte_reads: Vec<BytesRangeSpecifier>,
}

pub fn parse_access_list(access_list: &Vec<AccessListItem>) -> eyre::Result<ParsedAccessLists> {
    // parse the access list into buckets
    let mut parsed = ParsedAccessLists {
        chunk_reads: vec![],
        byte_reads: vec![],
    };
    for ali in access_list {
        if ali.address != PRECOMPILE_ADDRESS {
            warn!("received unfiltered access list item {:?}", &ali);
            continue;
        }
        for key in &ali.storage_keys {
            match PdAccessListArg::decode(key) {
                Ok(dec) => match dec {
                    PdAccessListArg::ChunksRead(range_specifier) => {
                        parsed.chunk_reads.push(range_specifier)
                    }
                    PdAccessListArg::BytesRead(bytes_range_specifier) => {
                        parsed.byte_reads.push(bytes_range_specifier)
                    }
                },
                Err(_) => continue,
            }
        }
    }

    Ok(parsed)
}
