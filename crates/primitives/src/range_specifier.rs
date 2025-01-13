//! Range offsets are used by PD to figure out what chunks/bytes are required to fufill a precompile call.

use alloy_primitives::{aliases::U208, B256};
use ruint::Uint;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
// 26 + 4 + 2 bytes
pub struct RangeSpecifier {
    pub partition_index: U208, // 3 64-bit words + 1 16 bit word, 26 bytes
    pub offset: u32,
    pub chunk_count: u16,
}

impl RangeSpecifier {
    pub fn encode(&self) -> [u8; 32] {
        let mut buf: [u8; 32] = [0; 32];
        buf[..26].copy_from_slice(&self.partition_index.to_le_bytes::<26>());
        buf[26..30].copy_from_slice(&self.offset.to_le_bytes());
        buf[30..32].copy_from_slice(&self.chunk_count.to_le_bytes());

        buf
    }

    pub fn decode(bytes: &[u8; 32]) -> eyre::Result<Self> {
        Ok(RangeSpecifier {
            partition_index: U208::from_le_bytes::<26>(bytes[..26].try_into()?),
            offset: u32::from_le_bytes(bytes[26..30].try_into()?),
            chunk_count: u16::from_le_bytes(bytes[30..32].try_into()?),
        })
    }
}

impl From<&[u8; 32]> for RangeSpecifier {
    fn from(value: &[u8; 32]) -> Self {
        RangeSpecifier::decode(value).unwrap()
    }
}

impl From<RangeSpecifier> for [u8; 32] {
    fn from(value: RangeSpecifier) -> Self {
        value.encode()
    }
}

impl From<RangeSpecifier> for B256 {
    fn from(value: RangeSpecifier) -> Self {
        B256::from(<RangeSpecifier as Into<[u8; 32]>>::into(value))
    }
}

impl From<B256> for RangeSpecifier {
    fn from(value: B256) -> Self {
        RangeSpecifier::decode(&value.0).unwrap()
    }
}

impl RangeSpecifier {
    pub fn to_slice(self) -> [u8; 32] {
        self.into()
    }

    pub fn from_slice(slice: &[u8; 32]) -> Self {
        slice.into()
    }
}

#[cfg(test)]
mod range_specifier_tests {

    use std::{u16, u32};

    use crate::range_specifier::RangeSpecifier;
    use alloy_primitives::aliases::U208;

    #[test]
    fn test_encode_decode_roundtrip() -> eyre::Result<()> {
        let range_spec = RangeSpecifier {
            partition_index: U208::from(42_u16),
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
            partition_index: U208::MAX,
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
    pub index: u8,          // index of the corresponding PD chunk range request
    pub chunk_offset: u16, // the PD chunk range request start relative chunk offset (matches chunk_count in RangeSpecifier)
    pub byte_offset: U18,  // the chunk offset relative byte offset to start reading from
    pub len: U34,          // the number of bytes to read
    pub reserved: [u8; 22], // 22 bytes, unused (for now) - above fields use 9.5 bytes (76 bits)
}

impl BytesRangeSpecifier {
    /// Encode the BytesRangeSpecifier into a 32-byte array
    pub fn encode(&self) -> [u8; 32] {
        let mut bytes = [0u8; 32];

        // Write index (byte 0)
        bytes[0] = self.index;

        // Write chunk_offset (bytes 1-2)
        bytes[1..3].copy_from_slice(&self.chunk_offset.to_be_bytes());

        // Write byte_offset (18 bits across bytes 3-5)
        let byte_offset_val = self.byte_offset.as_limbs()[0] as u32;
        bytes[3] = (byte_offset_val >> 10) as u8;
        bytes[4] = (byte_offset_val >> 2) as u8;
        bytes[5] = ((byte_offset_val & 0b11) << 6) as u8;

        // Write len (34 bits across bytes 5-9)
        let len_val = self.len.as_limbs()[0] as u64;
        bytes[5] |= (len_val >> 28) as u8; // Top 6 bits go into remaining bits of byte 5
        bytes[6] = (len_val >> 20) as u8;
        bytes[7] = (len_val >> 12) as u8;
        bytes[8] = (len_val >> 4) as u8;
        bytes[9] = ((len_val & 0b1111) << 4) as u8; // Bottom 4 bits

        // Write reserved bytes (bytes 10-31)
        bytes[10..32].copy_from_slice(&self.reserved);

        bytes
    }

    /// Decode a BytesRangeSpecifier from a 32-byte array
    pub fn decode(bytes: &[u8; 32]) -> eyre::Result<Self> {
        // Read index (byte 0)
        let index = bytes[0];

        // Read chunk_offset (bytes 1-2)
        let chunk_offset = u16::from_be_bytes([bytes[1], bytes[2]]);

        // Read byte_offset (18 bits across bytes 3-5)
        let byte_offset_val = ((bytes[3] as u32) << 10)
            | ((bytes[4] as u32) << 2)
            | ((bytes[5] & 0b11000000) >> 6) as u32;

        // Create U18 from raw bits
        let byte_offset = U18::from(byte_offset_val);

        // Read len (34 bits across bytes 5-9)
        let len_val = ((bytes[5] & 0b00111111) as u64) << 28
            | (bytes[6] as u64) << 20
            | (bytes[7] as u64) << 12
            | (bytes[8] as u64) << 4
            | (bytes[9] >> 4) as u64;

        // Create U34 from raw bits
        let len = U34::from(len_val);

        // Read reserved bytes (bytes 10-31)
        let mut reserved = [0u8; 22];
        reserved.copy_from_slice(&bytes[10..32]);

        Ok(BytesRangeSpecifier {
            index,
            chunk_offset,
            byte_offset,
            len,
            reserved,
        })
    }
}

impl From<[u8; 32]> for BytesRangeSpecifier {
    fn from(value: [u8; 32]) -> Self {
        BytesRangeSpecifier::decode(&value).unwrap()
    }
}

impl From<BytesRangeSpecifier> for [u8; 32] {
    fn from(value: BytesRangeSpecifier) -> Self {
        value.encode()
    }
}

impl From<BytesRangeSpecifier> for B256 {
    fn from(value: BytesRangeSpecifier) -> Self {
        B256::from(<BytesRangeSpecifier as Into<[u8; 32]>>::into(value))
    }
}

impl From<B256> for BytesRangeSpecifier {
    fn from(value: B256) -> Self {
        BytesRangeSpecifier::decode(&value.0).unwrap()
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
            reserved: [0u8; 22],
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
            reserved: [255u8; 22],
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
