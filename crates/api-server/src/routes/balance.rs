use crate::{error::ApiError, ApiState};
use actix_web::web::{self, Json, Path, Query};
use alloy_eips::BlockNumberOrTag;
use irys_actors::block_header_lookup;
use irys_types::{u64_stringify, BlockHash, IrysAddress, U256};
use reth::providers::BlockNumReader as _;
use serde::{Deserialize, Serialize};
use tracing::error;

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BalanceResponse {
    pub address: IrysAddress,
    pub balance: U256,
    #[serde(with = "u64_stringify")]
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

#[derive(Debug)]
enum BlockParameter {
    NumberOrTag(BlockNumberOrTag),
    IrysBlockHash(BlockHash),
}

/// Query account balance at specified block.
pub async fn get_balance(
    state: web::Data<ApiState>,
    address: Path<IrysAddress>,
    query: Query<BalanceQuery>,
) -> Result<Json<BalanceResponse>, ApiError> {
    let address = address.into_inner();
    let BalanceQuery { block } = query.into_inner();
    let block_param = parse_block_parameter(&block)?;
    let provider = &state.reth_provider.provider;

    let (balance, block_height) = match block_param {
        BlockParameter::NumberOrTag(block_number_or_tag) => {
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

            let state_provider = get_state_at_block(provider, resolved_block_num)?;
            let balance = query_balance(&state_provider, &address)?;

            (balance, resolved_block_num)
        }
        BlockParameter::IrysBlockHash(irys_block_hash) => {
            let irys_block_header = block_header_lookup::get_block_header(
                &state.block_tree,
                &state.db,
                irys_block_hash,
                false,
            )
            .map_err(|e| {
                error!("Error looking up block header: {}", e);
                ApiError::BalanceUnavailable {
                    reason: "Unable to retrieve block header".to_string(),
                }
            })?
            .ok_or_else(|| {
                error!(block.hash = %irys_block_hash, "Irys block not found in block tree or database");
                ApiError::BalanceUnavailable {
                    reason: "Block not found".to_string(),
                }
            })?;

            let resolved_block_num = irys_block_header.height;

            let state_provider = get_state_at_block(provider, resolved_block_num)?;
            let balance = query_balance(&state_provider, &address)?;

            (balance, resolved_block_num)
        }
    };

    Ok(Json(BalanceResponse {
        address,
        balance,
        block_height,
        block_parameter: block,
    }))
}

fn get_state_at_block(
    provider: &impl reth::providers::StateProviderFactory,
    block_num: u64,
) -> Result<impl reth::providers::StateProvider, ApiError> {
    provider.history_by_block_number(block_num).map_err(|e| {
        error!("Failed to get state at block {}: {}", block_num, e);
        ApiError::BalanceUnavailable {
            reason: "Unable to retrieve state at requested block".to_string(),
        }
    })
}

fn query_balance(
    state_provider: &impl reth::providers::StateProvider,
    address: &IrysAddress,
) -> Result<U256, ApiError> {
    Ok(state_provider
        .account_balance(&(address.into()))
        .map_err(|e| {
            error!("Failed to query balance for address {}: {}", address, e);
            ApiError::BalanceUnavailable {
                reason: "Unable to retrieve account balance".to_string(),
            }
        })?
        .map(std::convert::Into::into)
        .unwrap_or(U256::zero()))
}

fn parse_block_parameter(block: &str) -> Result<BlockParameter, ApiError> {
    match block {
        "latest" => Ok(BlockParameter::NumberOrTag(BlockNumberOrTag::Latest)),
        "pending" => Ok(BlockParameter::NumberOrTag(BlockNumberOrTag::Pending)),
        "earliest" => Ok(BlockParameter::NumberOrTag(BlockNumberOrTag::Earliest)),
        "safe" => Ok(BlockParameter::NumberOrTag(BlockNumberOrTag::Safe)),
        "finalized" => Ok(BlockParameter::NumberOrTag(BlockNumberOrTag::Finalized)),
        _ => {
            if let Ok(num) = block.parse::<u64>() {
                return Ok(BlockParameter::NumberOrTag(BlockNumberOrTag::Number(num)));
            }

            if let Some(hex_str) = block.strip_prefix("0x") {
                if hex_str.len() == 64 {
                    let hash_bytes =
                        hex::decode(hex_str).map_err(|_| ApiError::InvalidBlockParameter {
                            parameter: block.to_string(),
                        })?;

                    let irys_block_hash = BlockHash::from_slice(&hash_bytes);
                    return Ok(BlockParameter::IrysBlockHash(irys_block_hash));
                }

                if let Ok(num) = u64::from_str_radix(hex_str, 16) {
                    return Ok(BlockParameter::NumberOrTag(BlockNumberOrTag::Number(num)));
                }
            }

            if let Ok(decoded) = base58::FromBase58::from_base58(block) {
                if decoded.len() == 32 {
                    let irys_block_hash = BlockHash::from_slice(&decoded);
                    return Ok(BlockParameter::IrysBlockHash(irys_block_hash));
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

        if let Ok(BlockParameter::NumberOrTag(BlockNumberOrTag::Number(num))) = result {
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
        assert!(matches!(result.unwrap(), BlockParameter::NumberOrTag(tag) if tag == expected));
    }

    #[rstest]
    #[case("0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef")]
    #[case("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890")]
    #[case("0x0000000000000000000000000000000000000000000000000000000000000000")]
    fn test_parse_irys_block_hash_hex(#[case] input: &str) {
        let result = parse_block_parameter(input);
        assert!(
            result.is_ok(),
            "Failed to parse hex Irys block hash: {}",
            input
        );

        if let Ok(BlockParameter::IrysBlockHash(_)) = result {
            // Success
        } else {
            panic!(
                "Expected BlockParameter::IrysBlockHash for input: {}",
                input
            );
        }
    }

    #[test]
    fn test_parse_irys_block_hash_base58() {
        let test_hash = BlockHash::from([42_u8; 32]);
        let base58_encoded = base58::ToBase58::to_base58(test_hash.as_bytes());

        let result = parse_block_parameter(&base58_encoded);
        assert!(
            result.is_ok(),
            "Failed to parse base58 Irys block hash: {}",
            base58_encoded
        );

        if let Ok(BlockParameter::IrysBlockHash(parsed_hash)) = result {
            assert_eq!(parsed_hash, test_hash);
        } else {
            panic!(
                "Expected BlockParameter::IrysBlockHash for base58 input: {}",
                base58_encoded
            );
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
