// pub use alloy_eips::eip4895::Withdrawal;
// use alloy_primitives::{Address, B256};
// use serde::{Deserialize, Serialize};

// use crate::shadow::Shadows;

// #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
// #[serde(rename_all = "camelCase")]
// pub struct PayloadAttributes {
//     /// Value for the `timestamp` field of the new payload
//     #[serde(with = "alloy_serde::quantity")]
//     pub timestamp: u64,
//     /// Value for the `prevRandao` field of the new payload
//     pub prev_randao: B256,
//     /// Suggested value for the `feeRecipient` field of the new payload
//     pub suggested_fee_recipient: Address,
//     /// Array of [`Withdrawal`] enabled with V2
//     /// See <https://github.com/ethereum/execution-apis/blob/6452a6b194d7db269bf1dbd087a267251d3cc7f8/src/engine/shanghai.md#payloadattributesv2>
//     #[serde(skip_serializing_if = "Option::is_none")]
//     pub withdrawals: Option<Vec<Withdrawal>>,
//     /// Root of the parent beacon block enabled with V3.
//     ///
//     /// See also <https://github.com/ethereum/execution-apis/blob/main/src/engine/cancun.md#payloadattributesv3>
//     #[serde(skip_serializing_if = "Option::is_none")]
//     pub parent_beacon_block_root: Option<B256>,
//     #[serde(skip_serializing_if = "Option::is_none")]
//     pub shadows: Option<Shadows>,
// }
