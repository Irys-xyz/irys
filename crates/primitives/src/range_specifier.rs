//! Range offsets are used by PD to figure out what chunks/bytes are required to fufill a precompile call.

use alloy_primitives::{aliases::U200, B256};
use eyre::eyre;
use ruint::Uint;
use serde::{Deserialize, Serialize};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PdAccessListArgsTypeId {
    ChunksRead = 0,
    BytesRead = 1,
}

#[derive(thiserror::Error, Debug)]
pub enum PdAccessListArgsTypeIdDecodeError {
    #[error("unknown reserved PD access list args type ID: {0}")]
    UnknownPdAccessListArgsTypeId(u8),
}

impl TryFrom<u8> for PdAccessListArgsTypeId {
    type Error = PdAccessListArgsTypeIdDecodeError;
    fn try_from(id: u8) -> Result<Self, Self::Error> {
        match id {
            0 => Ok(PdAccessListArgsTypeId::ChunksRead),
            1 => Ok(PdAccessListArgsTypeId::BytesRead),
            _ => Err(PdAccessListArgsTypeIdDecodeError::UnknownPdAccessListArgsTypeId(id)),
        }
    }
}

pub enum PdAccessListArg {
    ChunksRead(RangeSpecifier),
    BytesRead(BytesRangeSpecifier),
}

impl PdAccessListArg {
    pub fn type_id(&self) -> PdAccessListArgsTypeId {
        match self {
            &PdAccessListArg::ChunksRead(_) => PdAccessListArgsTypeId::ChunksRead,
            &PdAccessListArg::BytesRead(_) => PdAccessListArgsTypeId::BytesRead,
        }
    }

    pub fn encode(&self) -> [u8; 32] {
        match self {
            PdAccessListArg::ChunksRead(range_specifier) => range_specifier.encode(),
            PdAccessListArg::BytesRead(bytes_range_specifier) => bytes_range_specifier.encode(),
        }
    }

    pub fn decode(bytes: &[u8; 32]) -> eyre::Result<Self> {
        let type_id = PdAccessListArgsTypeId::try_from(bytes[0])
            .map_err(|e| eyre!("failed to decode type ID: {}", e))?;

        match type_id {
            PdAccessListArgsTypeId::ChunksRead => {
                Ok(PdAccessListArg::ChunksRead(RangeSpecifier::decode(bytes)?))
            }
            PdAccessListArgsTypeId::BytesRead => Ok(PdAccessListArg::BytesRead(
                BytesRangeSpecifier::decode(bytes)?,
            )),
        }
    }
}

impl From<PdAccessListArg> for PdAccessListArgsTypeId {
    fn from(value: PdAccessListArg) -> Self {
        value.type_id()
    }
}

pub trait PdAccessListArgSerde {
    fn encode(&self) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[0] = Self::get_type() as u8;
        bytes[1..].copy_from_slice(&self.encode_inner());
        bytes
    }

    fn decode(bytes: &[u8; 32]) -> eyre::Result<Self>
    where
        Self: Sized,
    {
        let type_id = bytes[0];
        if type_id != Self::get_type() as u8 {
            return Err(eyre!("invalid type ID"));
        }
        Self::decode_inner(bytes[1..].try_into()?)
    }

    fn get_type() -> PdAccessListArgsTypeId;

    fn encode_inner(&self) -> [u8; 31];

    fn decode_inner(bytes: &[u8; 31]) -> eyre::Result<Self>
    where
        Self: Sized;
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct RangeSpecifier {
    pub partition_index: U200, // 3 64-bit words + 1 8 bit word, 25 bytes
    pub offset: u32,
    pub chunk_count: u16,
}

impl PdAccessListArgSerde for RangeSpecifier {
    fn get_type() -> PdAccessListArgsTypeId {
        PdAccessListArgsTypeId::ChunksRead
    }

    fn encode_inner(&self) -> [u8; 31] {
        let mut buf: [u8; 31] = [0; 31];
        buf[0..=24].copy_from_slice(&self.partition_index.to_le_bytes::<25>());
        buf[25..=28].copy_from_slice(&self.offset.to_le_bytes());
        buf[29..=30].copy_from_slice(&self.chunk_count.to_le_bytes());
        buf
    }

    fn decode_inner(bytes: &[u8; 31]) -> eyre::Result<Self>
    where
        Self: Sized,
    {
        Ok(RangeSpecifier {
            partition_index: U200::from_le_bytes::<25>(bytes[0..=24].try_into()?),
            offset: u32::from_le_bytes(bytes[25..=28].try_into()?),
            chunk_count: u16::from_le_bytes(bytes[29..=30].try_into()?),
        })
    }
}

// wrapper so we can have blanket impls
pub struct PdArgsEncWrapper<T>(T);
impl<T> PdArgsEncWrapper<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: PdAccessListArgSerde> From<&[u8; 32]> for PdArgsEncWrapper<T> {
    fn from(bytes: &[u8; 32]) -> Self {
        PdArgsEncWrapper(T::decode(bytes).expect("Invalid byte encoding"))
    }
}

impl<T: PdAccessListArgSerde> From<PdArgsEncWrapper<T>> for [u8; 32] {
    fn from(wrapper: PdArgsEncWrapper<T>) -> [u8; 32] {
        wrapper.0.encode()
    }
}

impl<T: PdAccessListArgSerde> From<PdArgsEncWrapper<T>> for B256 {
    fn from(wrapper: PdArgsEncWrapper<T>) -> B256 {
        B256::from(wrapper.0.encode())
    }
}

impl<T: PdAccessListArgSerde> From<&B256> for PdArgsEncWrapper<T> {
    fn from(bytes: &B256) -> Self {
        PdArgsEncWrapper(T::decode(&bytes.0).expect("Invalid byte encoding"))
    }
}

#[cfg(test)]
mod range_specifier_tests {

    use crate::range_specifier::{PdAccessListArgSerde as _, RangeSpecifier};
    use alloy_primitives::aliases::U200;
    use std::{u16, u32};

    #[test]
    fn test_encode_decode_roundtrip() -> eyre::Result<()> {
        let range_spec = RangeSpecifier {
            partition_index: U200::from(42_u16),
            offset: 12_u32,
            chunk_count: 11_u16,
        };

        let enc = range_spec.encode();
        let dec = RangeSpecifier::decode(&enc).unwrap();
        assert_eq!(dec, range_spec);
        Ok(())
    }

    #[test]
    fn test_byte_boundaries() {
        // Test maximum values
        let max_values = RangeSpecifier {
            partition_index: U200::MAX,
            offset: u32::MAX,
            chunk_count: u16::MAX,
        };

        let encoded = max_values.encode();
        let decoded = RangeSpecifier::decode(&encoded).unwrap();

        assert_eq!(max_values, decoded);
    }
}

pub type U34 = Uint<34, 1>;
pub type U18 = Uint<18, 1>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct BytesRangeSpecifier {
    pub index: u8,         // index of the corresponding PD chunk range request
    pub chunk_offset: u16, // the PD chunk range request start relative chunk offset (matches chunk_count in RangeSpecifier)
    pub byte_offset: U18,  // the chunk offset relative byte offset to start reading from
    pub len: U34,          // the number of bytes to read - this is optional
                           // pub reserved: [u8; 20], // 20 bytes, unused (for now)
}

impl PdAccessListArgSerde for BytesRangeSpecifier {
    fn get_type() -> PdAccessListArgsTypeId {
        PdAccessListArgsTypeId::BytesRead
    }

    fn encode_inner(&self) -> [u8; 31] {
        let mut buf: [u8; 31] = [0; 31];
        buf[0] = self.index;
        buf[1..=2].copy_from_slice(&self.chunk_offset.to_le_bytes());
        buf[3..=5].copy_from_slice(&self.byte_offset.to_le_bytes::<3>());
        buf[6..=10].copy_from_slice(&self.len.to_le_bytes::<5>());
        // 20 unused bytes
        // buf[11..=30].copy_from_slice(&self.reserved);
        buf
    }

    fn decode_inner(bytes: &[u8; 31]) -> eyre::Result<Self>
    where
        Self: Sized,
    {
        Ok(BytesRangeSpecifier {
            index: bytes[0],
            chunk_offset: u16::from_le_bytes(bytes[1..=2].try_into()?),
            byte_offset: U18::from_le_bytes::<3>(bytes[3..=5].try_into()?),
            len: U34::from_le_bytes::<5>(bytes[6..=10].try_into()?),
        })
    }
}

#[cfg(test)]
mod bytes_range_specifier_tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let original = BytesRangeSpecifier {
            index: 42,
            chunk_offset: 12345,
            byte_offset: U18::from(123456),
            len: U34::from(12345678),
        };

        let encoded = original.encode();
        let decoded = BytesRangeSpecifier::decode(&encoded).unwrap();

        assert_eq!(original, decoded);
    }

    #[test]
    fn test_bit_boundaries() {
        // Test maximum values
        let max_values = BytesRangeSpecifier {
            index: 255,
            chunk_offset: 65535,
            byte_offset: U18::from((1 << 18) - 1),
            len: U34::from((1_u64 << 34) - 1),
        };

        let encoded = max_values.encode();
        let decoded = BytesRangeSpecifier::decode(&encoded).unwrap();

        assert_eq!(max_values, decoded);
    }

    #[test]
    fn test_validation() {
        U18::try_from(1_u32 << 18).expect_err("value should overflow");
        U34::try_from(1_u64 << 34).expect_err("value should overflow");
    }
}
