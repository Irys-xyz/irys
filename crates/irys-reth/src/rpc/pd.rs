//! Programmable Data (PD) pricing RPC namespace for Reth
//!
//! Namespace: `pd`
//! Methods:
//! - `pd_quote` → returns base PD fee quote for a given number of chunks.

use jsonrpsee::{
    core::RpcResult,
    proc_macros::rpc,
    types::error::{ErrorObjectOwned, INTERNAL_ERROR_CODE},
};

use crate::pd_pricing::quote_base_fee_tokens;
use crate::pd_pricing::state::PdPricingState;
use alloy_primitives::U256 as AlloyU256;

/// Re-export pricing state types for backward compatibility
pub use crate::pd_pricing::state::PdPricingStateInner;

/// The PD RPC API exposed under the `pd` namespace.
#[cfg_attr(not(test), rpc(server, namespace = "pd"))]
#[cfg_attr(test, rpc(server, client, namespace = "pd"))]
pub trait PdApi {
    /// Returns base PD fee quote for a given number of chunks.
    ///
    /// - Uses a base USD rate per MB with floor enforced by `min_base_fee_tokens`.
    /// - Converts USD to tokens using the shared state price.
    #[method(name = "quote")]
    fn quote(&self, num_chunks: AlloyU256) -> RpcResult<AlloyU256>;
}

/// The RPC implementation that holds the consensus config snapshot.
pub struct PdRpc {
    /// Shared pricing state
    state: PdPricingState,
}

impl PdRpc {
    pub fn new_with_state(state: PdPricingState) -> Self {
        Self { state }
    }

    fn err<E: core::fmt::Display>(e: E) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(INTERNAL_ERROR_CODE, e.to_string(), None::<()>)
    }
}

impl PdApiServer for PdRpc {
    fn quote(&self, num_chunks: AlloyU256) -> RpcResult<AlloyU256> {
        // Snapshot pricing state then compute via shared helper
        let (price_scaled, min_base_fee_tokens, base_rate_usd_per_mb_scaled, chunk_size) = {
            let guard = self
                .state
                .0
                .read()
                .map_err(|_| Self::err("pricing state poisoned"))?;
            (
                guard.token_price_usd_per_token_scaled,
                guard.min_base_fee_tokens,
                guard.base_rate_usd_per_mb_scaled,
                guard.chunk_size,
            )
        };

        let fee = quote_base_fee_tokens(
            num_chunks,
            price_scaled,
            min_base_fee_tokens,
            base_rate_usd_per_mb_scaled,
            chunk_size,
        );
        Ok(fee)
    }
}

// no helpers
