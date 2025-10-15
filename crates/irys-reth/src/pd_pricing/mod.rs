use crate::constants::{BYTES_PER_MB, ONE_TOKEN_SCALE_ALLOY};
use alloy_primitives::U256;

/// Convert USD (scaled 1e18) to tokens (scaled 1e18) using a USD-per-token price (scaled 1e18).
/// Floors at zero on zero price.
#[inline]
pub fn usd_to_tokens_floor(usd_scaled: U256, price_usd_per_token_scaled: U256) -> U256 {
    if price_usd_per_token_scaled.is_zero() {
        return U256::from(0u8);
    }
    (usd_scaled.saturating_mul(ONE_TOKEN_SCALE_ALLOY)) / price_usd_per_token_scaled
}

/// Compute base fee in tokens for the given `num_chunks` based on USD/MB base rate and token price.
/// - `price_usd_per_token_scaled`: USD per token (scaled 1e18)
/// - `min_base_fee_tokens`: minimum fee in tokens (scaled 1e18)
/// - `base_rate_usd_per_mb_scaled`: USD per MB (scaled 1e18)
/// - `chunk_size`: bytes per chunk
#[inline]
pub fn quote_base_fee_tokens(
    num_chunks: U256,
    price_usd_per_token_scaled: U256,
    min_base_fee_tokens: U256,
    base_rate_usd_per_mb_scaled: U256,
    chunk_size: u64,
) -> U256 {
    // mb_in_chunks = ceil(1MB / chunk_size)
    let mb_in_chunks = (BYTES_PER_MB + chunk_size.saturating_sub(1)) / chunk_size.max(1);
    if mb_in_chunks == 0 {
        return min_base_fee_tokens;
    }
    let denom = U256::from(mb_in_chunks);
    // proportional USD = (num_chunks * base_rate) / mb_in_chunks
    let proportional_usd_scaled = if denom.is_zero() {
        U256::from(0u8)
    } else {
        num_chunks.saturating_mul(base_rate_usd_per_mb_scaled) / denom
    };
    // convert to tokens
    let tokens = usd_to_tokens_floor(proportional_usd_scaled, price_usd_per_token_scaled);
    // enforce floor
    if tokens < min_base_fee_tokens {
        min_base_fee_tokens
    } else {
        tokens
    }
}

pub mod state;
