use alloy_primitives::aliases::U208;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
// 26 + 4 + 2 bytes
pub struct RangeSpecifier {
    partition_index: U208, // 3 64-bit words + 1 16 bit word
    offset: u32,
    chunk_count: u16,
}

impl From<[u8; 32]> for RangeSpecifier {
    fn from(value: [u8; 32]) -> Self {
        let (partition_index, rest) = value.split_first_chunk::<26>().unwrap();
        let (offset, rest) = rest.split_first_chunk().unwrap();
        let (chunk_count, _rest) = rest.split_first_chunk().unwrap();

        RangeSpecifier {
            partition_index: U208::from_le_bytes(*partition_index),
            offset: u32::from_le_bytes(*offset),
            chunk_count: u16::from_le_bytes(*chunk_count),
        }
    }
}

impl From<RangeSpecifier> for [u8; 32] {
    fn from(value: RangeSpecifier) -> Self {
        let mut buf: [u8; 32] = [0; 32];
        buf[..26].copy_from_slice(&value.partition_index.to_le_bytes::<26>());
        buf[26..30].copy_from_slice(&value.offset.to_le_bytes());
        buf[30..32].copy_from_slice(&value.chunk_count.to_le_bytes());

        buf
    }
}

impl RangeSpecifier {
    pub fn to_slice(self) -> [u8; 32] {
        self.into()
    }

    pub fn from_slice(slice: [u8; 32]) -> Self {
        slice.into()
    }
}

mod tests {
    use super::RangeSpecifier;
    use alloy_primitives::aliases::U208;

    #[test]
    fn rangespec_test() -> eyre::Result<()> {
        let range_spec = RangeSpecifier {
            partition_index: U208::from(42_u16),
            offset: 12_u32,
            chunk_count: 11_u16,
        };

        let enc = range_spec.to_slice();
        let dec = RangeSpecifier::from_slice(enc);
        assert_eq!(dec, range_spec);
        Ok(())
    }
}
