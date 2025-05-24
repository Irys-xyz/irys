use alloy_rlp::{Decodable, Encodable, Error as RlpError};
use bytes::Buf;
use reth_codecs::Compact;

// use super::DestHash;

// #[derive(PartialEq, Debug, Default, Eq, Clone, Copy, Hash)]
// // #[main_codec(no_arbitrary)]
// #[derive(Compact, serde::Serialize, serde::Deserialize, RlpEncodable, RlpDecodable)]
// #[rlp(trailing)]
// #[derive(arbitrary::Arbitrary)]

// pub struct Commitment {
//     pub tx_id: IrysTxId,
//     pub quantity: U256,
//     pub height: u64,
//     pub status: CommitmentStatus,
//     pub tx_type: CommitmentType,
//     pub dest_hash: Option<DestHash>,
// }

// impl Commitment {
//     pub fn update_status(&mut self, status: CommitmentStatus) -> &Self {
//         self.status = status;
//         self
//     }
// }

// // #[derive(PartialEq, Debug, Eq, Clone, Copy, Hash)]
// // // #[derive(Compact, serde::Serialize, serde::Deserialize)]
// // #[derive(PledgeArbitrary, PledgePropTestArbitrary)]
// // #[main_codec(no_arbitrary)]
// // pub enum CommitmentType {
// //     Stake = 2,
// //     Pledge = 3,
// //     Unpledge = 4,
// //     Unstake = 5,
// // }

// #[derive(
//     PartialEq,
//     Debug,
//     Default,
//     Eq,
//     Clone,
//     Copy,
//     Hash,
//     Compact,
//     serde::Serialize,
//     serde::Deserialize,
//     arbitrary::Arbitrary,
// )]

// pub enum CommitmentStatus {
//     #[default]
//     /// Stake is pending epoch activation
//     Pending = 1,
//     /// Stake is active
//     Active = 2,
//     /// Stake is pending epoch removal
//     Inactive = 3,
//     /// Stake is pending slash epoch removal
//     Slashed = 4,
// }

// #[derive(thiserror::Error, Debug)]
// pub enum CommitmentStatusDecodeError {
//     #[error("unknown reserved Commitment status: {0}")]
//     UnknownCommitmentStatus(u8),
// }

// impl TryFrom<u8> for CommitmentStatus {
//     type Error = CommitmentStatusDecodeError;
//     fn try_from(id: u8) -> Result<Self, Self::Error> {
//         match id {
//             1 => Ok(CommitmentStatus::Pending),
//             2 => Ok(CommitmentStatus::Active),
//             3 => Ok(CommitmentStatus::Inactive),
//             4 => Ok(CommitmentStatus::Slashed),
//             _ => Err(CommitmentStatusDecodeError::UnknownCommitmentStatus(id)),
//         }
//     }
// }

// impl Encodable for CommitmentStatus {
//     fn encode(&self, out: &mut dyn bytes::BufMut) {
//         match self {
//             CommitmentStatus::Pending => out.put_u8(CommitmentStatus::Pending as u8),
//             CommitmentStatus::Active => out.put_u8(CommitmentStatus::Active as u8),
//             CommitmentStatus::Inactive => out.put_u8(CommitmentStatus::Inactive as u8),
//             CommitmentStatus::Slashed => out.put_u8(CommitmentStatus::Slashed as u8),
//         };
//     }
//     fn length(&self) -> usize {
//         1
//     }
// }

// impl Decodable for CommitmentStatus {
//     fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
//         let _v = buf.to_vec();
//         let enc_stake_status = u8::decode(&mut &buf[..])?;
//         buf.advance(1);
//         let id = CommitmentStatus::try_from(enc_stake_status)
//             .or(Err(RlpError::Custom("unknown stake status id")))?;
//         let _v2 = buf.to_vec();
//         Ok(id)
//     }
// }

// TODO: these need to be redone!
#[derive(
    PartialEq,
    Debug,
    Default,
    Eq,
    Clone,
    Copy,
    Hash,
    Compact,
    serde::Serialize,
    serde::Deserialize,
    arbitrary::Arbitrary,
)]

pub enum CommitmentType {
    #[default]
    Stake = 2,
    Pledge = 3,
    Unpledge = 4,
    Unstake = 5,
}

// TODO: custom de/serialize (or just make it a u8 field lol) impl so we can use the commitment type id integer

#[derive(thiserror::Error, Debug)]
pub enum CommitmentTypeDecodeError {
    #[error("unknown reserved Commitment type: {0}")]
    UnknownCommitmentType(u8),
}

impl TryFrom<u8> for CommitmentType {
    type Error = CommitmentTypeDecodeError;
    fn try_from(id: u8) -> Result<Self, Self::Error> {
        match id {
            2 => Ok(CommitmentType::Stake),
            3 => Ok(CommitmentType::Pledge),
            4 => Ok(CommitmentType::Unpledge),
            5 => Ok(CommitmentType::Unstake),
            _ => Err(CommitmentTypeDecodeError::UnknownCommitmentType(id)),
        }
    }
}

impl Encodable for CommitmentType {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        match self {
            CommitmentType::Stake => out.put_u8(CommitmentType::Stake as u8),
            CommitmentType::Pledge => out.put_u8(CommitmentType::Pledge as u8),
            CommitmentType::Unpledge => out.put_u8(CommitmentType::Unpledge as u8),
            CommitmentType::Unstake => out.put_u8(CommitmentType::Unstake as u8),
        };
    }
    fn length(&self) -> usize {
        1
    }
}

impl Decodable for CommitmentType {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let _v = buf.to_vec();
        let enc_commitment_type = u8::decode(&mut &buf[..])?;
        let commitment_type = CommitmentType::try_from(enc_commitment_type)
            .or(Err(RlpError::Custom("unknown commitment status")))?;

        buf.advance(1);
        Ok(commitment_type)
    }
}

// #[derive(
//     Debug,
//     Clone,
//     PartialEq,
//     Hash,
//     Eq,
//     Default,
//     RlpEncodableWrapper,
//     RlpDecodableWrapper,
//     Compact,
//     serde::Serialize,
//     serde::Deserialize,
//     arbitrary::Arbitrary,
// )]

// pub struct Commitments(pub Vec<Commitment>);

// // impl Commitments {
// //     /// Create a new pledges instance.
// //     pub fn new(pledges: Vec<Commitment>) -> Self {
// //         Self(pledges)
// //     }

// //     /// Calculate the total size, including capacity, of the pledges.
// //     #[inline]
// //     pub fn total_size(&self) -> usize {
// //         self.capacity() * std::mem::size_of::<Commitment>()
// //     }

// //     /// Calculate a heuristic for the in-memory size of the [pledges].
// //     #[inline]
// //     pub fn size(&self) -> usize {
// //         self.len() * std::mem::size_of::<Commitment>()
// //     }

// //     /// Get an iterator over the Shadows.
// //     pub fn iter(&self) -> std::slice::Iter<'_, Commitment> {
// //         self.0.iter()
// //     }

// //     /// Get a mutable iterator over the pledges.
// //     pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, Commitment> {
// //         self.0.iter_mut()
// //     }

// //     /// Convert [Self] into raw vec of pledges.
// //     pub fn into_inner(self) -> Vec<Commitment> {
// //         self.0
// //     }
// // }

// // impl IntoIterator for Commitments {
// //     type Item = Commitment;
// //     type IntoIter = std::vec::IntoIter<Commitment>;

// //     fn into_iter(self) -> Self::IntoIter {
// //         self.0.into_iter()
// //     }
// // }

// // impl AsRef<[Commitment]> for Commitments {
// //     fn as_ref(&self) -> &[Commitment] {
// //         &self.0
// //     }
// // }

// // impl Deref for Commitments {
// //     type Target = Vec<Commitment>;

// //     fn deref(&self) -> &Self::Target {
// //         &self.0
// //     }
// // }

// // impl DerefMut for Commitments {
// //     fn deref_mut(&mut self) -> &mut Self::Target {
// //         &mut self.0
// //     }
// // }

// impl From<Vec<Commitment>> for Commitments {
//     fn from(pledges: Vec<Commitment>) -> Self {
//         Self(pledges)
//     }
// }

// #[derive(PartialEq, Debug, Default, Eq, Clone, Copy, Hash)]
// // #[main_codec(no_arbitrary)]
// // #[derive(PledgeArbitrary, PledgePropTestArbitrary, RlpEncodable, RlpDecodable)]
// #[derive(
//     Compact, serde::Serialize, serde::Deserialize, RlpEncodable, RlpDecodable, arbitrary::Arbitrary,
// )]
// pub struct Stake {
//     pub tx_id: IrysTxId,
//     pub quantity: U256,
//     pub height: u64,
//     pub status: CommitmentStatus,
// }

// impl Stake {
//     pub fn update_status(&mut self, status: CommitmentStatus) -> &Self {
//         self.status = status;
//         self
//     }
// }

// wrap_fixed_bytes!(
//     extra_derives: [],
//     pub struct IrysBlockHash<48>;
// );

// impl Compact for IrysBlockHash {
//     #[inline]
//     fn to_compact<B>(&self, buf: &mut B) -> usize
//     where
//         B: bytes::BufMut + AsMut<[u8]>,
//     {
//         self.0.to_compact(buf)
//     }

//     #[inline]
//     fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
//         let (v, buf) = <[u8; core::mem::size_of::<IrysBlockHash>()]>::from_compact(buf, len);
//         (Self::from(v), buf)
//     }
// }

// wrap_fixed_bytes!(
//     extra_derives: [],
//     pub struct IrysTxId<32>;
// );

// // from storage/codecs/src/lib.rs, line 328, impl_compact_for_wrapped_bytes! macro
// // this is done "manually" here to prevent acyclic deps, which is why these structs are defined here in the first place...
// impl Compact for IrysTxId {
//     #[inline]
//     fn to_compact<B>(&self, buf: &mut B) -> usize
//     where
//         B: bytes::BufMut + AsMut<[u8]>,
//     {
//         self.0.to_compact(buf)
//     }

//     #[inline]
//     fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
//         let (v, buf) = <[u8; core::mem::size_of::<IrysTxId>()]>::from_compact(buf, len);
//         (Self::from(v), buf)
//     }
// }
