// use alloy_rlp::{Decodable, Encodable, Error as RlpError};
// use bytes::Buf;
// use reth_codecs::Compact;

// use super::commitment::{IrysBlockHash, IrysTxId};

// #[derive(
//     PartialEq,
//     Debug,
//     Eq,
//     Clone,
//     Copy,
//     Hash,
//     serde::Serialize,
//     serde::Deserialize,
//     Compact,
//     arbitrary::Arbitrary,
// )]
// pub enum LastTx {
//     BlockHash(IrysBlockHash),
//     TxId(IrysTxId),
// }

// impl Default for LastTx {
//     fn default() -> Self {
//         Self::BlockHash(IrysBlockHash::default())
//     }
// }

// #[derive(PartialEq, Debug, Eq, Clone, Copy)]

// pub enum LastTxId {
//     BlockHash = 0,
//     TxId = 1,
// }

// #[derive(thiserror::Error, Debug)]
// pub enum LastTxIdDecodeError {
//     #[error("unknown reserved LastTx type ID: {0}")]
//     UnknownLastTxTypeId(u8),
// }

// impl TryFrom<u8> for LastTxId {
//     type Error = LastTxIdDecodeError;
//     fn try_from(id: u8) -> Result<Self, Self::Error> {
//         match id {
//             0 => Ok(LastTxId::BlockHash),
//             1 => Ok(LastTxId::TxId),
//             _ => Err(LastTxIdDecodeError::UnknownLastTxTypeId(id)),
//         }
//     }
// }

// impl LastTx {
//     pub fn type_id(&self) -> LastTxId {
//         match self {
//             LastTx::BlockHash(_) => LastTxId::BlockHash,
//             LastTx::TxId(_) => LastTxId::TxId,
//         }
//     }
// }

// impl Encodable for LastTx {
//     fn encode(&self, out: &mut dyn bytes::BufMut) {
//         (self.type_id() as u8).encode(out);
//         match self {
//             LastTx::BlockHash(msg) => msg.encode(out),
//             LastTx::TxId(msg) => msg.encode(out),
//         };
//     }
//     fn length(&self) -> usize {
//         let payload_len = match self {
//             LastTx::BlockHash(msg) => msg.length(),
//             LastTx::TxId(msg) => msg.length(),
//         };
//         payload_len + 1 // +1 for type ID
//     }
// }

// impl Decodable for LastTx {
//     fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
//         let enc_stake_status_id = u8::decode(&mut &buf[..])?;
//         let id = LastTxId::try_from(enc_stake_status_id)
//             .or(Err(RlpError::Custom("unknown last tx id")))?;
//         buf.advance(1);
//         match id {
//             LastTxId::BlockHash => Ok(LastTx::BlockHash(IrysBlockHash::decode(buf)?)),
//             LastTxId::TxId => Ok(LastTx::TxId(IrysTxId::decode(buf)?)),
//         }
//     }
// }
