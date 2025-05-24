// #![allow(missing_docs)]
// use alloy_primitives::bytes;
// use bytes::Buf;
// use reth_codecs::Compact;
// use serde::{Deserialize, Serialize};
// use std::{
//     fmt::Debug,
//     ops::{Deref, DerefMut},
// };

// use alloy_rlp::{
//     Decodable, Encodable, Error as RlpError, RlpDecodable, RlpDecodableWrapper, RlpEncodable,
//     RlpEncodableWrapper,
// };

// pub use alloy_primitives::{Address, U256};

// use crate::commitment::IrysTxId;

// use super::new_account::NewAccountState;

// #[derive(
//     Clone,
//     Debug,
//     Default,
//     PartialEq,
//     Eq,
//     Hash,
//     RlpEncodable,
//     RlpDecodable,
//     Compact,
//     serde::Serialize,
//     serde::Deserialize,
//     arbitrary::Arbitrary,
// )]

// pub struct ShadowTx {
//     pub tx_id: IrysTxId,
//     pub fee: U256,
//     // address the tx is from
//     pub address: Address,
//     pub tx: ShadowTxType,
// }

// #[derive(
//     Debug,
//     Clone,
//     Eq,
//     PartialEq,
//     Hash,
//     Compact,
//     serde::Serialize,
//     serde::Deserialize,
//     arbitrary::Arbitrary,
//     Default,
// )]
// pub enum ShadowTxType {
//     #[default]
//     Null, // because default is a required derive TODO: replace with a null TransferShadow or some other no-op
//     Transfer(TransferShadow),
//     Data(DataShadow),
//     MiningAddressStake(MiningAddressStakeShadow),
//     PartitionPledge(PartitionPledgeShadow),
//     PartitionUnPledge(PartitionUnPledgeShadow),
//     Unstake(UnstakeShadow),
//     Slash(SlashShadow),
//     BlockReward(BlockRewardShadow),
//     Diff(DiffShadow),
// }

// // encode/decode boundary type IDs and related casting impls

// #[derive(Debug)]
// pub enum ShadowTxTypeId {
//     Null = 0,
//     Transfer = 1,
//     Data = 2,
//     MiningAddressPledge = 3,
//     PartitionPledge = 4,
//     PartitionUnPledge = 5,
//     Unstake = 6,
//     Slash = 7,
//     BlockReward = 8,
//     Diff = 9,
// }

// #[derive(thiserror::Error, Debug)]
// pub enum ShadowTxTypeIdDecodeError {
//     #[error("unknown reserved Shadow Tx type ID: {0}")]
//     UnknownShadowTypeId(u8),
// }

// impl TryFrom<u8> for ShadowTxTypeId {
//     type Error = ShadowTxTypeIdDecodeError;
//     fn try_from(id: u8) -> Result<Self, Self::Error> {
//         match id {
//             0 => Ok(ShadowTxTypeId::Null),
//             1 => Ok(ShadowTxTypeId::Transfer),
//             2 => Ok(ShadowTxTypeId::Data),
//             3 => Ok(ShadowTxTypeId::MiningAddressPledge),
//             4 => Ok(ShadowTxTypeId::PartitionPledge),
//             5 => Ok(ShadowTxTypeId::PartitionUnPledge),
//             6 => Ok(ShadowTxTypeId::Unstake),
//             7 => Ok(ShadowTxTypeId::Slash),
//             8 => Ok(ShadowTxTypeId::BlockReward),
//             9 => Ok(ShadowTxTypeId::Diff),
//             _ => Err(ShadowTxTypeIdDecodeError::UnknownShadowTypeId(id)),
//         }
//     }
// }
// impl ShadowTxType {
//     pub fn type_id(&self) -> ShadowTxTypeId {
//         match self {
//             ShadowTxType::Null => ShadowTxTypeId::Null,
//             ShadowTxType::Transfer(_) => ShadowTxTypeId::Transfer,
//             ShadowTxType::Data(_) => ShadowTxTypeId::Data,
//             ShadowTxType::MiningAddressStake(_) => ShadowTxTypeId::MiningAddressPledge,
//             ShadowTxType::PartitionPledge(_) => ShadowTxTypeId::PartitionPledge,
//             ShadowTxType::PartitionUnPledge(_) => ShadowTxTypeId::PartitionUnPledge,
//             ShadowTxType::Unstake(_) => ShadowTxTypeId::Unstake,
//             ShadowTxType::Slash(_) => ShadowTxTypeId::Slash,
//             ShadowTxType::BlockReward(_) => ShadowTxTypeId::BlockReward,
//             ShadowTxType::Diff(_) => ShadowTxTypeId::Diff,
//         }
//     }
// }

// // todo: macro these.
// impl Encodable for ShadowTxType {
//     fn encode(&self, out: &mut dyn bytes::BufMut) {
//         (self.type_id() as u8).encode(out);
//         match self {
//             ShadowTxType::Null => out.put_u8(0x00),
//             ShadowTxType::Transfer(msg) => msg.encode(out),
//             ShadowTxType::Data(msg) => msg.encode(out),
//             ShadowTxType::MiningAddressStake(msg) => msg.encode(out),
//             ShadowTxType::PartitionPledge(msg) => msg.encode(out),
//             ShadowTxType::PartitionUnPledge(msg) => msg.encode(out),
//             ShadowTxType::Unstake(msg) => msg.encode(out),
//             ShadowTxType::Slash(msg) => msg.encode(out),
//             ShadowTxType::BlockReward(msg) => msg.encode(out),
//             ShadowTxType::Diff(msg) => msg.encode(out),
//         };
//     }
//     fn length(&self) -> usize {
//         let payload_len = match self {
//             ShadowTxType::Null => 1,
//             ShadowTxType::Transfer(msg) => msg.length(),
//             ShadowTxType::Data(msg) => msg.length(),
//             ShadowTxType::MiningAddressStake(msg) => msg.length(),
//             ShadowTxType::PartitionPledge(msg) => msg.length(),
//             ShadowTxType::PartitionUnPledge(msg) => msg.length(),
//             ShadowTxType::Unstake(msg) => msg.length(),
//             ShadowTxType::Slash(msg) => msg.length(),
//             ShadowTxType::BlockReward(msg) => msg.length(),
//             ShadowTxType::Diff(msg) => msg.length(),
//         };
//         payload_len + 1 // +1 for type ID
//     }
// }

// impl Decodable for ShadowTxType {
//     fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
//         let enc_tx_type_id = u8::decode(&mut &buf[..])?;
//         let id = ShadowTxTypeId::try_from(enc_tx_type_id)
//             .or(Err(RlpError::Custom("unknown tx type id")))?;
//         buf.advance(1);
//         match id {
//             ShadowTxTypeId::Null => Ok(ShadowTxType::Null),
//             ShadowTxTypeId::Data => Ok(ShadowTxType::Data(DataShadow::decode(buf)?)),
//             ShadowTxTypeId::Transfer => Ok(ShadowTxType::Transfer(TransferShadow::decode(buf)?)),
//             ShadowTxTypeId::MiningAddressPledge => Ok(ShadowTxType::MiningAddressStake(
//                 MiningAddressStakeShadow::decode(buf)?,
//             )),
//             ShadowTxTypeId::PartitionPledge => Ok(ShadowTxType::PartitionPledge(
//                 PartitionPledgeShadow::decode(buf)?,
//             )),
//             ShadowTxTypeId::PartitionUnPledge => Ok(ShadowTxType::PartitionUnPledge(
//                 PartitionUnPledgeShadow::decode(buf)?,
//             )),
//             ShadowTxTypeId::Unstake => Ok(ShadowTxType::Unstake(UnstakeShadow::decode(buf)?)),
//             ShadowTxTypeId::Slash => Ok(ShadowTxType::Slash(SlashShadow::decode(buf)?)),
//             ShadowTxTypeId::BlockReward => {
//                 Ok(ShadowTxType::BlockReward(BlockRewardShadow::decode(buf)?))
//             }
//             ShadowTxTypeId::Diff => Ok(ShadowTxType::Diff(DiffShadow::decode(buf)?)),
//         }
//     }
// }

// #[derive(
//     Debug,
//     Copy,
//     Clone,
//     Eq,
//     PartialEq,
//     Hash,
//     RlpEncodable,
//     RlpDecodable,
//     Default,
//     Compact,
//     serde::Serialize,
//     serde::Deserialize,
//     arbitrary::Arbitrary,
// )]

// pub struct TransferShadow {
//     pub to: Address,
//     pub amount: U256,
// }

// #[derive(
//     Compact,
//     serde::Serialize,
//     serde::Deserialize,
//     Debug,
//     Copy,
//     Clone,
//     Eq,
//     PartialEq,
//     Hash,
//     RlpEncodable,
//     RlpDecodable,
//     Default,
//     arbitrary::Arbitrary,
// )]

// pub struct DataShadow {
//     pub fee: U256,
// }
// #[derive(
//     Compact,
//     serde::Serialize,
//     serde::Deserialize,
//     Debug,
//     Copy,
//     Clone,
//     Eq,
//     PartialEq,
//     Hash,
//     RlpEncodable,
//     RlpDecodable,
//     Default,
//     arbitrary::Arbitrary,
// )]

// pub struct MiningAddressStakeShadow {
//     pub value: U256,
//     pub height: u64,
// }
// #[derive(
//     Compact,
//     serde::Serialize,
//     serde::Deserialize,
//     Debug,
//     Copy,
//     Clone,
//     Eq,
//     PartialEq,
//     Hash,
//     RlpEncodable,
//     RlpDecodable,
//     Default,
//     arbitrary::Arbitrary,
// )]

// pub struct PartitionPledgeShadow {
//     pub quantity: U256,
//     pub part_hash: IrysTxId,
//     pub height: u64,
// }

// // todo: below are NOT FINAL
// #[derive(
//     Compact,
//     serde::Serialize,
//     serde::Deserialize,
//     Debug,
//     Copy,
//     Clone,
//     Eq,
//     PartialEq,
//     Hash,
//     RlpEncodable,
//     RlpDecodable,
//     Default,
//     arbitrary::Arbitrary,
// )]

// pub struct PartitionUnPledgeShadow {
//     pub part_hash: IrysTxId,
// }
// #[derive(
//     Compact,
//     serde::Serialize,
//     serde::Deserialize,
//     Debug,
//     Copy,
//     Clone,
//     Eq,
//     PartialEq,
//     Hash,
//     RlpEncodable,
//     RlpDecodable,
//     Default,
//     arbitrary::Arbitrary,
// )]

// pub struct UnstakeShadow {}

// #[derive(
//     Compact,
//     serde::Serialize,
//     serde::Deserialize,
//     Debug,
//     Copy,
//     Clone,
//     Eq,
//     PartialEq,
//     Hash,
//     RlpEncodable,
//     RlpDecodable,
//     Default,
//     arbitrary::Arbitrary,
// )]

// pub struct SlashShadow {
//     pub slashed_addr: Address,
// }

// #[derive(
//     Compact,
//     serde::Serialize,
//     serde::Deserialize,
//     Debug,
//     Copy,
//     Clone,
//     Eq,
//     PartialEq,
//     Hash,
//     RlpEncodable,
//     RlpDecodable,
//     Default,
//     arbitrary::Arbitrary,
// )]

// pub struct BlockRewardShadow {
//     pub reward: U256,
// }

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
//     arbitrary::Arbitrary,
// )]

// pub struct DiffShadow {
//     pub new_state: NewAccountState,
// }

// #[derive(
//     Compact,
//     serde::Serialize,
//     serde::Deserialize,
//     Debug,
//     Clone,
//     PartialEq,
//     Eq,
//     Default,
//     Hash,
//     RlpEncodableWrapper,
//     RlpDecodableWrapper,
//     arbitrary::Arbitrary,
// )]

// pub struct Shadows(Vec<ShadowTx>);

// impl Shadows {
//     /// Create a new Shadows instance.
//     pub fn new(shadows: Vec<ShadowTx>) -> Self {
//         Self(shadows)
//     }

//     /// Calculate the total size, including capacity, of the Shadows.
//     #[inline]
//     pub fn total_size(&self) -> usize {
//         self.capacity() * std::mem::size_of::<ShadowTx>()
//     }

//     /// Calculate a heuristic for the in-memory size of the [Shadows].
//     #[inline]
//     pub fn size(&self) -> usize {
//         self.len() * std::mem::size_of::<ShadowTx>()
//     }

//     /// Get an iterator over the Shadows.
//     pub fn iter(&self) -> std::slice::Iter<'_, ShadowTx> {
//         self.0.iter()
//     }

//     /// Get a mutable iterator over the Shadows.
//     pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, ShadowTx> {
//         self.0.iter_mut()
//     }

//     /// Convert [Self] into raw vec of withdrawals.
//     pub fn into_inner(self) -> Vec<ShadowTx> {
//         self.0
//     }
// }

// impl IntoIterator for Shadows {
//     type Item = ShadowTx;
//     type IntoIter = std::vec::IntoIter<ShadowTx>;

//     fn into_iter(self) -> Self::IntoIter {
//         self.0.into_iter()
//     }
// }

// impl AsRef<[ShadowTx]> for Shadows {
//     fn as_ref(&self) -> &[ShadowTx] {
//         &self.0
//     }
// }

// impl Deref for Shadows {
//     type Target = Vec<ShadowTx>;

//     fn deref(&self) -> &Self::Target {
//         &self.0
//     }
// }

// impl DerefMut for Shadows {
//     fn deref_mut(&mut self) -> &mut Self::Target {
//         &mut self.0
//     }
// }

// impl From<Vec<ShadowTx>> for Shadows {
//     fn from(shadows: Vec<ShadowTx>) -> Self {
//         Self(shadows)
//     }
// }

// // mod test {

// //     #[test]
// //     fn enc_dec_test() {
// //         let shadow1 = ShadowTx {
// //             tx_id: TxId::random(),
// //             address: Address::random(),
// //             tx: ShadowTxType::Data(DataShadow { fee: U256::from(8) }),
// //         };
// //         let shadows = Shadows::new(vec![shadow1]);
// //         let mut buf = vec![];
// //         let enc = shadows.encode(&mut buf);
// //         dbg!(buf);
// //         ()
// //     }
// // }

// // #[cfg_attr(feature = "zstd-codec", main_codec(no_arbitrary, zstd))]
// // #[cfg_attr(not(feature = "zstd-codec"), main_codec(no_arbitrary))]
// // #[add_arbitrary_tests]
// // #[derive(Clone, Debug, PartialEq, Eq, Default, RlpEncodable, RlpDecodable)]
// // #[rlp(trailing)]
// // pub struct ShadowReceipt {
// //     pub tx_type: ShadowTxType,
// //     pub success: bool,
// // }

// #[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
// // #[rlp(trailing)]
// pub struct ShadowReceipt {
//     pub tx_id: IrysTxId,
//     pub tx_type: ShadowTxType,
//     pub result: ShadowResult,
// }

// #[derive(Clone, Debug, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
// pub enum ShadowResult {
//     #[default]
//     Success,
//     OutOfFunds,
//     OverflowPayment,
//     Failure,
//     AlreadyStaked,
//     NoPledges,
//     NoMatchingPledge,
//     AlreadyPledged,
// }
