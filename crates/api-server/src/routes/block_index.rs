use crate::{ApiState, error::ApiError};
use actix_web::web::{self, Json};
use irys_types::{BlockIndexItem, BlockIndexQuery, H256, LedgerIndexItem};
use serde::Serialize;

/// Maximum number of blocks that can be requested in a single query.
///
/// A hard limit protects the API from requests that would otherwise
/// require the server to iterate over a very large range of blocks,
/// potentially leading to excessive memory usage or denial of service.
const MAX_BLOCK_INDEX_QUERY_LIMIT: usize = 1_000;
const DEFAULT_BLOCK_INDEX_QUERY_LIMIT: usize = 100;

/// API response wrapper that preserves the `num_ledgers` field for
/// backward compatibility with external clients.
#[derive(Serialize)]
pub(crate) struct BlockIndexItemResponse {
    block_hash: H256,
    num_ledgers: u8,
    ledgers: Vec<LedgerIndexItem>,
}

impl From<BlockIndexItem> for BlockIndexItemResponse {
    fn from(item: BlockIndexItem) -> Self {
        Self {
            block_hash: item.block_hash,
            num_ledgers: u8::try_from(item.ledgers.len()).unwrap_or(u8::MAX),
            ledgers: item.ledgers,
        }
    }
}

fn resolve_limit(limit: usize) -> Result<usize, ApiError> {
    let effective = if limit == 0 {
        DEFAULT_BLOCK_INDEX_QUERY_LIMIT
    } else {
        limit
    };

    if effective > MAX_BLOCK_INDEX_QUERY_LIMIT {
        return Err(ApiError::Custom(format!(
            "limit exceeds maximum allowed value of {MAX_BLOCK_INDEX_QUERY_LIMIT}"
        )));
    }

    Ok(effective)
}

pub(crate) async fn block_index_route(
    state: web::Data<ApiState>,
    query: web::Query<BlockIndexQuery>,
) -> Result<Json<Vec<BlockIndexItemResponse>>, ApiError> {
    let limit = resolve_limit(query.limit)?;
    let height = query.height;

    let requested_blocks = state.block_index.read().get_range(height as u64, limit);

    Ok(Json(requested_blocks.into_iter().map(Into::into).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case(0, Ok(DEFAULT_BLOCK_INDEX_QUERY_LIMIT))]
    #[case(1, Ok(1))]
    #[case(999, Ok(999))]
    #[case(1_000, Ok(1_000))]
    #[case(1_001, Err(()))]
    #[case(usize::MAX, Err(()))]
    fn resolve_limit_boundary_cases(#[case] input: usize, #[case] expected: Result<usize, ()>) {
        let result = resolve_limit(input);
        match expected {
            Ok(v) => assert_eq!(result.unwrap(), v),
            Err(()) => assert!(result.is_err()),
        }
    }

    mod resolve_limit_proptest {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn result_in_valid_range(limit in 0_usize..=MAX_BLOCK_INDEX_QUERY_LIMIT) {
                let result = resolve_limit(limit).unwrap();
                prop_assert!(result >= 1);
                prop_assert!(result <= MAX_BLOCK_INDEX_QUERY_LIMIT);
            }
        }
    }
}
