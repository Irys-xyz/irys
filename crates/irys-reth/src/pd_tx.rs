//! Utilities to construct and decode Programmable Data (PD) access list entries and transactions.
//!
//! PD storage keys encode: `<slot_index:26><offset:4><chunk_count:2>` (big-endian) into 32 bytes.
//! - `slot_index` identifies the partition slot in the publish ledger (26 bytes)
//! - `offset` is the starting chunk within the slot (4 bytes)
//! - `chunk_count` is the number of sequential chunks from offset (2 bytes)
//!
//! This module helps:
//! - Build PD storage keys
//! - Build access lists for PD precompile
//! - Extract total PD chunk count from a transaction’s access list

use crate::constants::{
    FIXED_POINT_SCALE_1E12, PD_MAX_ADJ_DEN, PD_MAX_CHUNKS_PER_BLOCK, PD_TARGET_UTIL_DEN,
    USD_CENT_SCALED_ALLOY,
};
use alloy_consensus::TxEip1559;
use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_primitives::{Address, B256, U256};
use irys_primitives::precompile::PD_PRECOMPILE_ADDRESS;

/// PD storage key components extracted from a 32-byte access-list key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdKey {
    pub slot_index_be: [u8; 26],
    pub offset: u32,
    pub chunk_count: u16,
}

/// Encode a PD storage key `<slot_index:26><offset:4><chunk_count:2>` into a 32-byte big-endian word.
pub fn encode_pd_storage_key(slot_index_be: [u8; 26], offset: u32, chunk_count: u16) -> B256 {
    let mut buf = [0u8; 32];
    buf[0..26].copy_from_slice(&slot_index_be);
    buf[26..30].copy_from_slice(&offset.to_be_bytes());
    buf[30..32].copy_from_slice(&chunk_count.to_be_bytes());
    B256::from(buf)
}

/// Decode a PD storage key from a 32-byte big-endian word.
pub fn decode_pd_storage_key(key: B256) -> PdKey {
    let bytes = key.0;
    let mut slot = [0u8; 26];
    slot.copy_from_slice(&bytes[0..26]);
    let mut off = [0u8; 4];
    off.copy_from_slice(&bytes[26..30]);
    let mut cnt = [0u8; 2];
    cnt.copy_from_slice(&bytes[30..32]);
    PdKey {
        slot_index_be: slot,
        offset: u32::from_be_bytes(off),
        chunk_count: u16::from_be_bytes(cnt),
    }
}

/// Create a PD access list for a list of PD keys, under the PD precompile address.
pub fn build_pd_access_list(keys: impl IntoIterator<Item = PdKey>) -> AccessList {
    let storage_keys: Vec<B256> = keys
        .into_iter()
        .map(|k| encode_pd_storage_key(k.slot_index_be, k.offset, k.chunk_count))
        .collect();
    AccessList::from(vec![AccessListItem {
        address: PD_PRECOMPILE_ADDRESS,
        storage_keys,
    }])
}

/// Compute total PD chunks referenced in an access list (simple sum, no deduplication).
pub fn sum_pd_chunks_in_access_list(access_list: &AccessList) -> u64 {
    access_list
        .0
        .iter()
        .filter(|item| item.address == PD_PRECOMPILE_ADDRESS)
        .flat_map(|item| item.storage_keys.iter())
        .map(|key| decode_pd_storage_key(*key).chunk_count as u64)
        .sum()
}

/// Compute total PD chunks referenced in a transaction envelope’s access list (all supported types).
// Note: sum helpers for envelopes and RPC transactions can be added on demand when needed.

/// EIP-1559-style step adjustment for base rate, all U256 fixed-point math.
/// - `current_scaled`: base rate in USD/MB scaled by 1e18
/// - Returns next base rate, floored at $0.01 (1e16)
pub fn step_base_rate_u256(current_scaled: U256, chunks_used_in_block: u64) -> U256 {
    use alloy_primitives::U256 as A;
    let current = A::from::<U256>(current_scaled);
    let max_chunks = A::from(PD_MAX_CHUNKS_PER_BLOCK);
    if max_chunks.is_zero() {
        return current_scaled;
    }

    let scale = A::from(FIXED_POINT_SCALE_1E12);
    let util_scaled = A::from(chunks_used_in_block).saturating_mul(scale) / max_chunks;
    let target_scaled = scale / A::from(PD_TARGET_UTIL_DEN); // 0.5
    let max_adj_scaled = scale / A::from(PD_MAX_ADJ_DEN); // 0.125

    let (frac_scaled, sign_positive) = if util_scaled > target_scaled {
        let numerator = util_scaled.saturating_sub(target_scaled);
        let denom = scale.saturating_sub(target_scaled);
        ((numerator.saturating_mul(scale)) / denom, true)
    } else {
        let numerator = target_scaled.saturating_sub(util_scaled);
        let denom = target_scaled.max(A::from(1u8));
        ((numerator.saturating_mul(scale)) / denom, false)
    };

    let adjust_scaled = (frac_scaled.saturating_mul(max_adj_scaled)) / scale;
    let multiplier_scaled = if sign_positive {
        scale.saturating_add(adjust_scaled)
    } else {
        scale.saturating_sub(adjust_scaled)
    };

    let new_scaled = (current.saturating_mul(multiplier_scaled)) / scale;
    let floor_scaled = USD_CENT_SCALED_ALLOY; // $0.01 floor
    A::max(new_scaled, floor_scaled).into()
}

/// Helper to construct a minimal EIP-1559 PD transaction with a provided PD access list.
/// The caller is responsible for filling fees, nonce, chain_id, etc., and signing.
pub fn make_pd_eip1559_tx_with_access_list(
    chain_id: u64,
    to: Address,
    value: U256,
    gas_limit: u64,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    access_list: AccessList,
) -> TxEip1559 {
    TxEip1559 {
        chain_id,
        nonce: 0,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        gas_limit,
        to: to.into(),
        value,
        input: alloy_primitives::Bytes::new(),
        access_list,
    }
}
