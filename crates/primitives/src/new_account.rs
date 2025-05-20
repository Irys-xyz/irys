// use alloy_primitives::U256;
// use alloy_rlp::{RlpDecodable, RlpEncodable};

// use reth_codecs::Compact;

// use super::{
//     commitment::{Commitments, Stake},
//     last_tx::LastTx,
// };

// #[derive(
//     Compact,
//     serde::Serialize,
//     serde::Deserialize,
//     Debug,
//     Default,
//     Clone,
//     Eq,
//     PartialEq,
//     Hash,
//     RlpEncodable,
//     RlpDecodable,
// )]
// #[rlp(trailing)]
// #[derive(arbitrary::Arbitrary)]

// pub struct NewAccountState {
//     /// Account balance.
//     pub balance: Option<U256>,
//     /// Account nonce.
//     pub nonce: Option<u64>,
//     /// code hash,
//     // pub code_hash: B256,
//     /// code: if None, `code_by_hash` will be used to fetch it if code needs to be loaded from
//     /// inside of `revm`.
//     // pub code: Option<Bytecode>,
//     /// custom pledge addition
//     pub stake: Option<WrappedStake>,
//     pub commitments: Option<WrappedCommitments>,
//     pub last_tx: Option<WrappedLastTx>,
//     pub mining_permission: Option<bool>,
// }
// // weird how many problems can be solved with wrapper structs
// #[derive(
//     Compact,
//     serde::Serialize,
//     serde::Deserialize,
//     Debug,
//     Clone,
//     Eq,
//     PartialEq,
//     Hash,
//     RlpEncodable,
//     RlpDecodable,
//     Default,
// )]
// #[rlp(trailing)]
// #[derive(arbitrary::Arbitrary)]

// pub struct WrappedStake(pub Option<Stake>);

// #[derive(
//     Compact,
//     serde::Serialize,
//     serde::Deserialize,
//     Debug,
//     Clone,
//     Eq,
//     PartialEq,
//     Hash,
//     RlpEncodable,
//     RlpDecodable,
//     Default,
// )]
// #[rlp(trailing)]
// #[derive(arbitrary::Arbitrary)]

// pub struct WrappedCommitments(pub Option<Commitments>);
// #[derive(
//     Compact,
//     serde::Serialize,
//     serde::Deserialize,
//     Debug,
//     Clone,
//     Eq,
//     PartialEq,
//     Hash,
//     RlpEncodable,
//     RlpDecodable,
//     Default,
// )]
// #[rlp(trailing)]
// #[derive(arbitrary::Arbitrary)]

// pub struct WrappedLastTx(pub Option<LastTx>);
