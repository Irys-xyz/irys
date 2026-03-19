//! Range offsets are used by PD to figure out what chunks/bytes are required to fulfill a precompile call.

use std::ops::Div as _;

use alloy_primitives::{aliases::U200, B256, U256};
use eyre::{eyre, OptionExt as _};
use ruint::Uint;
use serde::{Deserialize, Serialize};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PdAccessListArgsTypeId {
    ChunkRead = 0,
    ByteRead = 1,
    PdPriorityFee = 2,
    PdBaseFeeCap = 3,
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
            0 => Ok(Self::ChunkRead),
            1 => Ok(Self::ByteRead),
            2 => Ok(Self::PdPriorityFee),
            3 => Ok(Self::PdBaseFeeCap),
            _ => Err(PdAccessListArgsTypeIdDecodeError::UnknownPdAccessListArgsTypeId(id)),
        }
    }
}

pub enum PdAccessListArg {
    ChunkRead(ChunkRangeSpecifier),
    ByteRead(ByteRangeSpecifier),
    PdPriorityFee(U256),
    PdBaseFeeCap(U256),
}

impl PdAccessListArg {
    pub fn type_id(&self) -> PdAccessListArgsTypeId {
        match *self {
            Self::ChunkRead(_) => PdAccessListArgsTypeId::ChunkRead,
            Self::ByteRead(_) => PdAccessListArgsTypeId::ByteRead,
            Self::PdPriorityFee(_) => PdAccessListArgsTypeId::PdPriorityFee,
            Self::PdBaseFeeCap(_) => PdAccessListArgsTypeId::PdBaseFeeCap,
        }
    }

    pub fn encode(&self) -> [u8; 32] {
        match self {
            Self::ChunkRead(range_specifier) => range_specifier.encode(),
            Self::ByteRead(bytes_range_specifier) => bytes_range_specifier.encode(),
            Self::PdPriorityFee(fee) => {
                encode_pd_fee(PdAccessListArgsTypeId::PdPriorityFee as u8, *fee)
                    .expect("PdPriorityFee encode: fee must be > 0 and <= MAX_PD_FEE")
            }
            Self::PdBaseFeeCap(fee) => {
                encode_pd_fee(PdAccessListArgsTypeId::PdBaseFeeCap as u8, *fee)
                    .expect("PdBaseFeeCap encode: fee must be > 0 and <= MAX_PD_FEE")
            }
        }
    }

    pub fn decode(bytes: &[u8; 32]) -> eyre::Result<Self> {
        let type_id = PdAccessListArgsTypeId::try_from(bytes[0])
            .map_err(|e| eyre!("failed to decode type ID: {}", e))?;

        match type_id {
            PdAccessListArgsTypeId::ChunkRead => {
                Ok(Self::ChunkRead(ChunkRangeSpecifier::decode(bytes)?))
            }
            PdAccessListArgsTypeId::ByteRead => {
                Ok(Self::ByteRead(ByteRangeSpecifier::decode(bytes)?))
            }
            PdAccessListArgsTypeId::PdPriorityFee => {
                let fee = decode_pd_fee(bytes, PdAccessListArgsTypeId::PdPriorityFee as u8)?;
                Ok(Self::PdPriorityFee(fee))
            }
            PdAccessListArgsTypeId::PdBaseFeeCap => {
                let fee = decode_pd_fee(bytes, PdAccessListArgsTypeId::PdBaseFeeCap as u8)?;
                Ok(Self::PdBaseFeeCap(fee))
            }
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
        let mut bytes = [0_u8; 32];
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
pub struct ChunkRangeSpecifier {
    pub partition_index: U200, // 3 64-bit words + 1 8 bit word, 25 bytes
    pub offset: u32,           // offset within the partition (chunks)
    pub chunk_count: u16,      // number of chunks in the range
}

impl PdAccessListArgSerde for ChunkRangeSpecifier {
    fn get_type() -> PdAccessListArgsTypeId {
        PdAccessListArgsTypeId::ChunkRead
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
        Ok(Self {
            partition_index: U200::try_from_le_slice(bytes[0..=24].try_into()?)
                .ok_or_eyre("U200 out of bounds")?,
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
        Self(T::decode(bytes).expect("Invalid byte encoding"))
    }
}

impl<T: PdAccessListArgSerde> From<PdArgsEncWrapper<T>> for [u8; 32] {
    fn from(wrapper: PdArgsEncWrapper<T>) -> [u8; 32] {
        wrapper.0.encode()
    }
}

impl<T: PdAccessListArgSerde> From<PdArgsEncWrapper<T>> for B256 {
    fn from(wrapper: PdArgsEncWrapper<T>) -> Self {
        Self::from(wrapper.0.encode())
    }
}

impl<T: PdAccessListArgSerde> From<&B256> for PdArgsEncWrapper<T> {
    fn from(bytes: &B256) -> Self {
        Self(T::decode(&bytes.0).expect("Invalid byte encoding"))
    }
}

#[cfg(test)]
mod range_specifier_tests {

    use crate::range_specifier::{
        decode_pd_fee, encode_pd_fee, ChunkRangeSpecifier, PdAccessListArg,
        PdAccessListArgSerde as _, PdAccessListArgsTypeId, MAX_PD_FEE,
    };
    use alloy_primitives::{aliases::U200, U256};

    #[test]
    fn test_encode_decode_roundtrip() -> eyre::Result<()> {
        let range_spec = ChunkRangeSpecifier {
            partition_index: U200::from(42_u16),
            offset: 12_u32,
            chunk_count: 11_u16,
        };

        let enc = range_spec.encode();
        let dec = ChunkRangeSpecifier::decode(&enc).unwrap();
        assert_eq!(dec, range_spec);
        Ok(())
    }

    #[test]
    fn test_byte_boundaries() {
        // Test maximum values
        let max_values = ChunkRangeSpecifier {
            partition_index: U200::MAX,
            offset: u32::MAX,
            chunk_count: u16::MAX,
        };

        let encoded = max_values.encode();
        let decoded = ChunkRangeSpecifier::decode(&encoded).unwrap();

        assert_eq!(max_values, decoded);
    }

    #[test]
    fn test_pd_fee_encode_decode_roundtrip() -> eyre::Result<()> {
        let priority_fee = U256::from(1_000_000_u64);
        let type_byte = PdAccessListArgsTypeId::PdPriorityFee as u8;
        let encoded = encode_pd_fee(type_byte, priority_fee)?;
        let decoded = decode_pd_fee(&encoded, type_byte)?;
        assert_eq!(decoded, priority_fee);

        let base_fee = U256::from(500_000_u64);
        let type_byte = PdAccessListArgsTypeId::PdBaseFeeCap as u8;
        let encoded = encode_pd_fee(type_byte, base_fee)?;
        let decoded = decode_pd_fee(&encoded, type_byte)?;
        assert_eq!(decoded, base_fee);

        Ok(())
    }

    #[test]
    fn test_pd_fee_reject_zero() {
        let result = encode_pd_fee(PdAccessListArgsTypeId::PdPriorityFee as u8, U256::ZERO);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("fee must be > 0"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_pd_fee_reject_overflow() {
        let over_max = MAX_PD_FEE.checked_add(U256::from(1)).expect("no overflow");
        let result = encode_pd_fee(PdAccessListArgsTypeId::PdPriorityFee as u8, over_max);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("fee exceeds u248 max"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_pd_fee_reject_wrong_type_byte() -> eyre::Result<()> {
        let fee = U256::from(42_u64);
        let type_byte = PdAccessListArgsTypeId::PdPriorityFee as u8;
        let encoded = encode_pd_fee(type_byte, fee)?;

        // Try decoding with the wrong expected type byte
        let wrong_type = PdAccessListArgsTypeId::PdBaseFeeCap as u8;
        let result = decode_pd_fee(&encoded, wrong_type);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("unexpected type byte"),
            "unexpected error: {err}"
        );

        Ok(())
    }

    #[test]
    fn test_pd_fee_max_value() -> eyre::Result<()> {
        let type_byte = PdAccessListArgsTypeId::PdPriorityFee as u8;
        let encoded = encode_pd_fee(type_byte, MAX_PD_FEE)?;
        let decoded = decode_pd_fee(&encoded, type_byte)?;
        assert_eq!(decoded, MAX_PD_FEE);
        Ok(())
    }

    #[test]
    fn test_pd_fee_small_value() -> eyre::Result<()> {
        let fee = U256::from(1);
        let type_byte = PdAccessListArgsTypeId::PdBaseFeeCap as u8;
        let encoded = encode_pd_fee(type_byte, fee)?;

        // The encoded form should be: type_byte, 30 zero bytes, then 0x01
        assert_eq!(encoded[0], type_byte);
        for &b in &encoded[1..31] {
            assert_eq!(b, 0x00, "expected zero byte in padding");
        }
        assert_eq!(encoded[31], 0x01);

        let decoded = decode_pd_fee(&encoded, type_byte)?;
        assert_eq!(decoded, fee);
        Ok(())
    }

    #[test]
    fn test_pd_access_list_arg_fee_roundtrip() -> eyre::Result<()> {
        // PdPriorityFee variant
        let priority_fee = U256::from(999_999_u64);
        let arg = PdAccessListArg::PdPriorityFee(priority_fee);
        let encoded = arg.encode();
        let decoded = PdAccessListArg::decode(&encoded)?;
        match decoded {
            PdAccessListArg::PdPriorityFee(f) => assert_eq!(f, priority_fee),
            other => panic!("expected PdPriorityFee, got {:?}", other.type_id()),
        }

        // PdBaseFeeCap variant
        let base_fee = U256::from(123_456_789_u64);
        let arg = PdAccessListArg::PdBaseFeeCap(base_fee);
        let encoded = arg.encode();
        let decoded = PdAccessListArg::decode(&encoded)?;
        match decoded {
            PdAccessListArg::PdBaseFeeCap(f) => assert_eq!(f, base_fee),
            other => panic!("expected PdBaseFeeCap, got {:?}", other.type_id()),
        }

        Ok(())
    }
}

/// Maximum PD fee value: 2^248 - 1. Fits in 31 bytes.
pub const MAX_PD_FEE: U256 = U256::from_be_bytes([
    0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
]);

/// Encode a U256 fee into a 32-byte access list key with type prefix.
/// Returns Err if fee is zero or exceeds `MAX_PD_FEE`.
pub fn encode_pd_fee(type_byte: u8, fee: U256) -> eyre::Result<[u8; 32]> {
    eyre::ensure!(fee > U256::ZERO, "fee must be > 0");
    eyre::ensure!(fee <= MAX_PD_FEE, "fee exceeds u248 max");
    let mut buf = [0_u8; 32];
    buf[0] = type_byte;
    let be_bytes = fee.to_be_bytes::<32>();
    buf[1..32].copy_from_slice(&be_bytes[1..32]);
    Ok(buf)
}

/// Decode a U256 fee from a 32-byte access list key.
/// The top byte of the U256 is forced to 0 (u248 bound by construction).
pub fn decode_pd_fee(bytes: &[u8; 32], expected_type: u8) -> eyre::Result<U256> {
    eyre::ensure!(bytes[0] == expected_type, "unexpected type byte");
    let mut be_bytes = [0_u8; 32];
    be_bytes[1..32].copy_from_slice(&bytes[1..32]);
    let fee = U256::from_be_bytes(be_bytes);
    eyre::ensure!(fee > U256::ZERO, "fee must be > 0");
    Ok(fee)
}

pub type U34 = Uint<34, 1>;
pub type U18 = Uint<18, 1>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct ByteRangeSpecifier {
    pub index: u8,         // index of the corresponding PD chunk range request
    pub chunk_offset: u16, // the PD chunk range request start relative chunk offset (matches chunk_count in RangeSpecifier)
    pub byte_offset: U18,  // the chunk offset relative byte offset to start reading from
    pub length: U34,       // the number of bytes to read - this is optional
                           // pub reserved: [u8; 20], // 20 bytes, unused (for now)
}

impl PdAccessListArgSerde for ByteRangeSpecifier {
    fn get_type() -> PdAccessListArgsTypeId {
        PdAccessListArgsTypeId::ByteRead
    }

    fn encode_inner(&self) -> [u8; 31] {
        let mut buf: [u8; 31] = [0; 31];
        buf[0] = self.index;
        buf[1..=2].copy_from_slice(&self.chunk_offset.to_le_bytes());
        buf[3..=5].copy_from_slice(&self.byte_offset.to_le_bytes::<3>());
        buf[6..=10].copy_from_slice(&self.length.to_le_bytes::<5>());
        // 20 unused bytes
        // buf[11..=30].copy_from_slice(&self.reserved);
        buf
    }

    fn decode_inner(bytes: &[u8; 31]) -> eyre::Result<Self>
    where
        Self: Sized,
    {
        Ok(Self {
            index: bytes[0],
            chunk_offset: u16::from_le_bytes(bytes[1..=2].try_into()?),
            byte_offset: U18::try_from_le_slice(bytes[3..=5].try_into()?)
                .ok_or_eyre("U18 out of bounds")?,
            length: U34::try_from_le_slice(bytes[6..=10].try_into()?)
                .ok_or_eyre("U34 out of bounds")?,
        })
    }
}

impl ByteRangeSpecifier {
    pub fn translate_offset(&mut self, chunk_size: u64, offset: u64) -> eyre::Result<()> {
        let full_offset: u64 = offset
            .checked_add(u64::try_from(self.byte_offset)?)
            .ok_or_else(|| eyre!("Offset addition overflow"))?;

        let additional_chunks = full_offset.div(chunk_size);
        let new_chunk_offset = self
            .chunk_offset
            .checked_add(
                u16::try_from(additional_chunks).map_err(|_| eyre!("Chunk offset overflow"))?,
            )
            .ok_or_else(|| eyre!("Chunk offset addition overflow"))?;

        let new_byte_offset = U18::try_from(full_offset % chunk_size)
            .map_err(|_| eyre!("Byte offset conversion error"))?;

        self.chunk_offset = new_chunk_offset;
        self.byte_offset = new_byte_offset;
        Ok(())
    }
}

#[cfg(test)]
mod bytes_range_specifier_tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let original = ByteRangeSpecifier {
            index: 42,
            chunk_offset: 12345,
            byte_offset: U18::from(123456),
            length: U34::from(12345678),
        };

        let encoded = original.encode();
        let decoded = ByteRangeSpecifier::decode(&encoded).unwrap();

        assert_eq!(original, decoded);
    }

    #[test]
    fn test_bit_boundaries() {
        // Test maximum values
        let max_values = ByteRangeSpecifier {
            index: 255,
            chunk_offset: 65535,
            byte_offset: U18::from((1 << 18) - 1),
            length: U34::from((1_u64 << 34) - 1),
        };

        let encoded = max_values.encode();
        let decoded = ByteRangeSpecifier::decode(&encoded).unwrap();

        assert_eq!(max_values, decoded);
    }

    #[test]
    fn test_validation() {
        U18::try_from(1_u32 << 18).expect_err("value should overflow");
        U34::try_from(1_u64 << 34).expect_err("value should overflow");
    }

    #[test]
    fn test_translate_offset_within_chunk() -> eyre::Result<()> {
        let mut specifier = ByteRangeSpecifier {
            index: 0,
            chunk_offset: 5,
            byte_offset: U18::from(100),
            length: U34::from(50),
        };

        let chunk_size = 1000;
        let offset = 200;
        specifier.translate_offset(chunk_size, offset)?;

        assert_eq!(specifier.chunk_offset, 5);
        assert_eq!(u64::try_from(specifier.byte_offset)?, 300);
        assert_eq!(specifier.length, specifier.length);
        Ok(())
    }

    #[test]
    fn test_translate_offset_cross_chunk() -> eyre::Result<()> {
        let mut specifier = ByteRangeSpecifier {
            index: 1,
            chunk_offset: 10,
            byte_offset: U18::from(800),
            length: U34::from(100),
        };

        let chunk_size = 1000;
        let offset = 300;
        specifier.translate_offset(chunk_size, offset)?;

        assert_eq!(specifier.chunk_offset, 11);
        assert_eq!(u64::try_from(specifier.byte_offset)?, 100);
        assert_eq!(specifier.length, specifier.length);
        Ok(())
    }

    #[test]
    fn test_translate_offset_multiple_chunks() -> eyre::Result<()> {
        let mut specifier = ByteRangeSpecifier {
            index: 2,
            chunk_offset: 20,
            byte_offset: U18::from(500),
            length: U34::from(200),
        };

        let chunk_size = 1000;
        let offset = 2500;
        specifier.translate_offset(chunk_size, offset)?;

        assert_eq!(specifier.chunk_offset, 23);
        assert_eq!(u64::try_from(specifier.byte_offset)?, 0);
        assert_eq!(specifier.length, specifier.length);
        Ok(())
    }

    #[test]
    fn test_translate_offset_zero() -> eyre::Result<()> {
        let mut specifier = ByteRangeSpecifier {
            index: 3,
            chunk_offset: 15,
            byte_offset: U18::from(250),
            length: U34::from(75),
        };

        let chunk_size = 1000;
        let offset = 0;
        specifier.translate_offset(chunk_size, offset)?;

        assert_eq!(specifier.chunk_offset, 15);
        assert_eq!(u64::try_from(specifier.byte_offset)?, 250);
        assert_eq!(specifier.length, specifier.length);
        Ok(())
    }

    #[test]
    fn test_translate_offset_exact_chunk_boundary() -> eyre::Result<()> {
        let mut specifier = ByteRangeSpecifier {
            index: 4,
            chunk_offset: 30,
            byte_offset: U18::from(900),
            length: U34::from(150),
        };

        let chunk_size = 1000;
        let offset = 100;
        specifier.translate_offset(chunk_size, offset)?;

        assert_eq!(specifier.chunk_offset, 31);
        assert_eq!(u64::try_from(specifier.byte_offset)?, 0);
        assert_eq!(specifier.length, specifier.length);
        Ok(())
    }

    #[test]
    fn test_translate_offset_overflow_handling() -> eyre::Result<()> {
        let mut specifier = ByteRangeSpecifier {
            index: 5,
            chunk_offset: u16::MAX,
            byte_offset: U18::from(500),
            length: U34::from(100),
        };

        let chunk_size = 1000;
        let offset = 1000;

        // Should return an error instead of panicking
        let result = specifier.translate_offset(chunk_size, offset);
        assert!(result.is_err());

        // Verify error message
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("overflow"),
            "Expected overflow error, got: {err}"
        );

        Ok(())
    }

    #[test]
    fn test_translate_offset_huge_offset() -> eyre::Result<()> {
        let mut specifier = ByteRangeSpecifier {
            index: 6,
            chunk_offset: 0,
            byte_offset: U18::from(0),
            length: U34::from(100),
        };

        let chunk_size = 1000;
        let offset = u64::MAX;

        // Should return an error for impossibly large offset
        let result = specifier.translate_offset(chunk_size, offset);
        assert!(result.is_err());

        Ok(())
    }
}
