use crate::constants::{ONE_TOKEN_SCALE_ALLOY, USD_CENT_SCALED_ALLOY};
use alloy_primitives::U256 as AlloyU256;
use std::sync::{Arc, RwLock};

/// Shared, atomically-updated pricing parameters for PD quoting and dynamic adjustments.
#[derive(Debug, Clone)]
pub struct PdPricingStateInner {
    /// Current token price expressed in USD per IRYS token (scaled 1e18)
    pub token_price_usd_per_token_scaled: AlloyU256,
    /// Minimum base fee to charge per transaction (tokens, 1e18 scale)
    pub min_base_fee_tokens: AlloyU256,
    /// Base rate in USD per MB used in PD base fee calculation (scaled 1e18)
    pub base_rate_usd_per_mb_scaled: AlloyU256,
    /// Chunk size in bytes from consensus config
    pub chunk_size: u64,
}

#[derive(Clone)]
pub struct PdPricingState(pub Arc<RwLock<PdPricingStateInner>>);

impl core::fmt::Debug for PdPricingState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "PdPricingState(..)")
    }
}

#[derive(Clone)]
pub struct PdPricingHandle(pub Arc<RwLock<PdPricingStateInner>>);

impl core::fmt::Debug for PdPricingHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "PdPricingHandle(..)")
    }
}

impl PdPricingState {
    /// Construct pricing state with sensible defaults from parameters.
    /// - `chunk_size`: consensus chunk size
    /// - `price_scaled`: USD per token scaled 1e18
    pub fn new_from_params(chunk_size: u64, price_scaled: AlloyU256) -> (Self, PdPricingHandle) {
        let base_rate_usd_per_mb_scaled = USD_CENT_SCALED_ALLOY; // $0.01
        let min_base_fee_usd_scaled = USD_CENT_SCALED_ALLOY; // $0.01

        // tokens = (usd_scaled * 1e18) / price_scaled
        let token_scale = ONE_TOKEN_SCALE_ALLOY;
        let min_base_fee_tokens = if price_scaled.is_zero() {
            AlloyU256::ZERO
        } else {
            (min_base_fee_usd_scaled.saturating_mul(token_scale)) / price_scaled
        };

        let inner = PdPricingStateInner {
            token_price_usd_per_token_scaled: price_scaled,
            min_base_fee_tokens,
            base_rate_usd_per_mb_scaled,
            chunk_size,
        };
        let arc = Arc::new(RwLock::new(inner));
        (PdPricingState(arc.clone()), PdPricingHandle(arc))
    }
}

impl PdPricingHandle {
    pub fn set_price_and_min_base(&self, price_scaled: AlloyU256, min_base_fee_tokens: AlloyU256) {
        if let Ok(mut guard) = self.0.write() {
            guard.token_price_usd_per_token_scaled = price_scaled;
            guard.min_base_fee_tokens = min_base_fee_tokens;
        }
    }

    pub fn set_base_rate_usd_per_mb(&self, rate_scaled: AlloyU256) {
        if let Ok(mut guard) = self.0.write() {
            guard.base_rate_usd_per_mb_scaled = rate_scaled;
        }
    }
}
