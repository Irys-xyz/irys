use crate::{error::ApiError, utils::address::parse_address, ApiState};
use actix_web::web::{self, Json, Path, Query};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::FixedBytes;
use irys_types::{Address, U256};
use reth::{
    providers::{BlockNumReader as _, StateProviderFactory as _},
    rpc::types::RpcBlockHash,
};
use serde::{Deserialize, Serialize};
use tracing::error;

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BalanceResponse {
    pub address: Address,
    pub balance: U256,
    pub block_height: u64,
    pub block_parameter: String,
}

#[derive(Deserialize, Debug)]
pub struct BalanceQuery {
    #[serde(default = "default_block_param")]
    pub block: String,
}

#[must_use]
fn default_block_param() -> String {
    "latest".to_string()
}

/// Query account balance at specified block.
/// Supports hex (0x...) and Base58 addresses.
pub async fn get_balance(
    state: web::Data<ApiState>,
    path: Path<String>,
    query: Query<BalanceQuery>,
) -> Result<Json<BalanceResponse>, ApiError> {
    let address = parse_address(&path)?;
    let BalanceQuery { block } = query.into_inner();
    let block_id = parse_block_parameter(&block)?;
    let provider = &state.reth_provider.provider;

    let (balance, block_height) = match block_id {
        BlockId::Number(block_number_or_tag) => {
            let resolved_block_num = match block_number_or_tag {
                BlockNumberOrTag::Latest => provider.last_block_number().map_err(|e| {
                    error!("Failed to get latest block number: {}", e);
                    ApiError::BalanceUnavailable {
                        reason: "Unable to retrieve latest block".to_string(),
                    }
                })?,
                BlockNumberOrTag::Pending => provider.best_block_number().map_err(|e| {
                    error!("Failed to get pending block number: {}", e);
                    ApiError::BalanceUnavailable {
                        reason: "Unable to retrieve pending block".to_string(),
                    }
                })?,
                BlockNumberOrTag::Earliest => 0,
                BlockNumberOrTag::Number(num) => num,
                _ => {
                    return Err(ApiError::NotImplemented {
                        feature: format!("Block parameter '{}' not supported", block),
                    });
                }
            };

            let state_provider = provider
                .history_by_block_number(resolved_block_num)
                .map_err(|e| {
                    error!("Failed to get state at block {}: {}", resolved_block_num, e);
                    ApiError::BalanceUnavailable {
                        reason: "Unable to retrieve state at requested block".to_string(),
                    }
                })?;

            let balance = query_balance(&state_provider, &address)?;

            (balance, resolved_block_num)
        }
        BlockId::Hash(rpc_block_hash) => {
            let state_provider = provider
                .state_by_block_hash(rpc_block_hash.block_hash)
                .map_err(|e| {
                    error!(
                        "Failed to get state at block hash {:?}: {}",
                        rpc_block_hash.block_hash, e
                    );
                    ApiError::BalanceUnavailable {
                        reason: "Unable to retrieve state at requested block hash".to_string(),
                    }
                })?;

            let balance = query_balance(&state_provider, &address)?;

            let block_height = provider
                .block_number(rpc_block_hash.block_hash)
                .map_err(|e| {
                    error!(
                        "Failed to get block number for hash {:?}: {}",
                        rpc_block_hash.block_hash, e
                    );
                    ApiError::BalanceUnavailable {
                        reason: "Unable to retrieve block number".to_string(),
                    }
                })?
                .ok_or_else(|| {
                    error!("Block not found for hash: {:?}", rpc_block_hash.block_hash);
                    ApiError::BalanceUnavailable {
                        reason: "Block not found".to_string(),
                    }
                })?;

            (balance, block_height)
        }
    };

    Ok(Json(BalanceResponse {
        address,
        balance,
        block_height,
        block_parameter: block,
    }))
}

fn query_balance(
    state_provider: &impl reth::providers::StateProvider,
    address: &Address,
) -> Result<U256, ApiError> {
    Ok(state_provider
        .account_balance(address)
        .map_err(|e| {
            error!("Failed to query balance for address {}: {}", address, e);
            ApiError::BalanceUnavailable {
                reason: "Unable to retrieve account balance".to_string(),
            }
        })?
        .map(std::convert::Into::into)
        .unwrap_or(U256::zero()))
}

fn parse_block_parameter(block: &str) -> Result<BlockId, ApiError> {
    match block {
        "latest" => Ok(BlockId::Number(BlockNumberOrTag::Latest)),
        "pending" => Ok(BlockId::Number(BlockNumberOrTag::Pending)),
        "earliest" => Ok(BlockId::Number(BlockNumberOrTag::Earliest)),
        "safe" => Ok(BlockId::Number(BlockNumberOrTag::Safe)),
        "finalized" => Ok(BlockId::Number(BlockNumberOrTag::Finalized)),
        _ => {
            if let Ok(num) = block.parse::<u64>() {
                return Ok(BlockId::Number(BlockNumberOrTag::Number(num)));
            }

            if let Some(hex_str) = block.strip_prefix("0x") {
                if hex_str.len() == 64 {
                    let hash_bytes =
                        hex::decode(hex_str).map_err(|_| ApiError::InvalidBlockParameter {
                            parameter: block.to_string(),
                        })?;

                    let block_hash = FixedBytes::<32>::from_slice(&hash_bytes);
                    return Ok(BlockId::Hash(RpcBlockHash {
                        block_hash,
                        require_canonical: Some(true),
                    }));
                }

                if let Ok(num) = u64::from_str_radix(hex_str, 16) {
                    return Ok(BlockId::Number(BlockNumberOrTag::Number(num)));
                }
            }

            Err(ApiError::InvalidBlockParameter {
                parameter: block.to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("12345", 12345)]
    #[case("0x64", 100)]
    #[case("0x123", 0x123)]
    #[case("0", 0)]
    #[case("0xFFFF", 65535)]
    fn test_parse_block_number(#[case] input: &str, #[case] expected: u64) {
        let result = parse_block_parameter(input);
        assert!(result.is_ok());

        if let Ok(BlockId::Number(BlockNumberOrTag::Number(num))) = result {
            assert_eq!(num, expected);
        } else {
            panic!("Expected BlockNumberOrTag::Number({})", expected);
        }
    }

    #[rstest]
    #[case("latest", BlockNumberOrTag::Latest)]
    #[case("pending", BlockNumberOrTag::Pending)]
    #[case("earliest", BlockNumberOrTag::Earliest)]
    #[case("safe", BlockNumberOrTag::Safe)]
    #[case("finalized", BlockNumberOrTag::Finalized)]
    fn test_parse_block_tag(#[case] input: &str, #[case] expected: BlockNumberOrTag) {
        let result = parse_block_parameter(input);
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), BlockId::Number(tag) if tag == expected));
    }

    #[rstest]
    #[case("0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef")]
    #[case("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890")]
    #[case("0x0000000000000000000000000000000000000000000000000000000000000000")]
    fn test_parse_block_hash(#[case] input: &str) {
        let result = parse_block_parameter(input);
        assert!(result.is_ok());

        if let Ok(BlockId::Hash(rpc_hash)) = result {
            assert_eq!(rpc_hash.require_canonical, Some(true));
        } else {
            panic!("Expected BlockId::Hash for input: {}", input);
        }
    }

    #[rstest]
    #[case("invalid")]
    #[case("0xZZZZ567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef")]
    #[case("0xGGGG")]
    #[case("not_a_number")]
    fn test_parse_invalid_block_parameter(#[case] input: &str) {
        let result = parse_block_parameter(input);
        assert!(result.is_err());
    }
}
