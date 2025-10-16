use std::sync::Arc;

use crate::pd_pricing::state::{PdPricingHandle, PdPricingState, PdPricingStateInner};
use reth_transaction_pool::EthPooledTransaction;

/// Trait for deciding whether a PD transaction should be included given current pricing.
///
/// This is intentionally minimal; policies can inspect the transaction and number of PD chunks,
/// and consult the current pricing snapshot. Implementations must be cheap and deterministic.
pub trait PdPolicy: std::fmt::Debug + Send + Sync {
    fn accept_pd_tx(
        &self,
        tx: &EthPooledTransaction,
        pd_chunks: u64,
        pricing: &PdPricingStateInner,
    ) -> bool;
}

/// A permissive default policy: accept all PD transactions. Useful as a placeholder
/// until a concrete policy is configured.
#[derive(Debug, Default)]
pub struct AcceptAllPdPolicy;

impl PdPolicy for AcceptAllPdPolicy {
    fn accept_pd_tx(
        &self,
        _tx: &EthPooledTransaction,
        _pd_chunks: u64,
        _pricing: &PdPricingStateInner,
    ) -> bool {
        true
    }
}

/// Convenience constructor for a default policy.
pub fn default_pd_policy() -> Arc<dyn PdPolicy> {
    Arc::new(AcceptAllPdPolicy)
}

/// Helper to get a read-only snapshot of pricing state.
pub fn snapshot_pricing(pricing: &PdPricingState) -> PdPricingStateInner {
    pricing
        .0
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|_| PdPricingStateInner {
            token_price_usd_per_token_scaled: crate::constants::ONE_TOKEN_SCALE_ALLOY,
            min_base_fee_tokens: crate::constants::USD_CENT_SCALED_ALLOY,
            base_rate_usd_per_mb_scaled: crate::constants::USD_CENT_SCALED_ALLOY,
            chunk_size: 32,
        })
}

/// Type alias re-export so callers can hold and update pricing.
pub type PdPricingShared = PdPricingState;
pub type PdPricingUpdater = PdPricingHandle;
