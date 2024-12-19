use alloy_primitives::{aliases::U208, B256};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
// 26 + 4 + 2 bytes
pub struct RangeSpecifier {
    pub partition_index: U208, // 3 64-bit words + 1 16 bit word, 26 bytes
    pub offset: u32,
    pub chunk_count: u16,
}

impl From<[u8; 32]> for RangeSpecifier {
    fn from(value: [u8; 32]) -> Self {
        RangeSpecifier {
            partition_index: U208::from_le_bytes::<26>(value[..26].try_into().unwrap()),
            offset: u32::from_le_bytes(value[26..30].try_into().unwrap()),
            chunk_count: u16::from_le_bytes(value[30..32].try_into().unwrap()),
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

impl From<RangeSpecifier> for B256 {
    fn from(value: RangeSpecifier) -> Self {
        B256::from(&value.to_slice())
    }
}

impl From<B256> for RangeSpecifier {
    fn from(value: B256) -> Self {
        RangeSpecifier::from_slice(value.0)
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
