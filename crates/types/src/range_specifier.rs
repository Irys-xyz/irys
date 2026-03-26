//! PD access list encoding for data reads and fee parameters.
//!
//! Each access list storage key under the PD precompile address is a 32-byte
//! value whose first byte is a type discriminant:
//!
//! | Type byte | Meaning          | Struct              |
//! |-----------|------------------|---------------------|
//! | `0x01`    | Data read        | [`PdDataRead`]      |
//! | `0x02`    | Priority fee     | (fee U256)          |
//! | `0x03`    | Base fee cap     | (fee U256)          |
//!
//! `0x00` is intentionally unused so that `B256::ZERO` always fails validation.

use alloy_primitives::U256;
use eyre::eyre;

use crate::chunk_provider::ChunkConfig;

// ---------------------------------------------------------------------------
// Type discriminant enum
// ---------------------------------------------------------------------------

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PdAccessListArgsTypeId {
    DataRead = 1,
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
            1 => Ok(Self::DataRead),
            2 => Ok(Self::PdPriorityFee),
            3 => Ok(Self::PdBaseFeeCap),
            _ => Err(PdAccessListArgsTypeIdDecodeError::UnknownPdAccessListArgsTypeId(id)),
        }
    }
}

// ---------------------------------------------------------------------------
// PdDataRead
// ---------------------------------------------------------------------------

/// Unified PD access list specifier. Encodes a data read from a partition's
/// chunk range.
///
/// Binary layout (32 bytes, all big-endian):
///
/// | Offset   | Size  | Field           | Description                             |
/// |----------|-------|-----------------|-----------------------------------------|
/// | `[0]`    | 1     | type byte       | Must be `0x01`                          |
/// | `[1..9]` | 8     | partition_index | Partition to read from                  |
/// | `[9..13]`| 4     | start           | First chunk offset within partition     |
/// | `[13..17]`| 4    | len             | Number of bytes to read                 |
/// | `[17..20]`| 3    | byte_off        | Byte offset within the first chunk (u24)|
/// | `[20..32]`| 12   | reserved        | Must be zero                            |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PdDataRead {
    pub partition_index: u64,
    pub start: u32,
    pub len: u32,
    /// Byte offset within the first chunk. Stored as u32 but the top byte is
    /// always zero (u24 range: 0..16_777_215).
    pub byte_off: u32,
}

/// Maximum value for the `byte_off` field (2^24 - 1).
const BYTE_OFF_MAX: u32 = (1 << 24) - 1;

impl PdDataRead {
    /// Decode a `PdDataRead` from a 32-byte access list storage key.
    ///
    /// # Validity rules
    /// - Type byte (`[0]`) must be `0x01`.
    /// - `len` must be > 0.
    /// - `byte_off` must be < `chunk_config.chunk_size` (as u32).
    /// - Reserved bytes `[20..32]` must all be zero.
    /// - `start + chunks_needed` must not exceed `num_chunks_in_partition`.
    pub fn decode(bytes: &[u8; 32], chunk_config: &ChunkConfig) -> eyre::Result<Self> {
        // Type discriminant
        if bytes[0] != PdAccessListArgsTypeId::DataRead as u8 {
            return Err(eyre!(
                "invalid PdDataRead type byte: expected 0x01, got 0x{:02X}",
                bytes[0]
            ));
        }

        // Reserved bytes must be zero
        if bytes[20..32].iter().any(|&b| b != 0) {
            return Err(eyre!("non-zero reserved bytes in PdDataRead"));
        }

        let partition_index = u64::from_be_bytes(bytes[1..9].try_into()?);
        let start = u32::from_be_bytes(bytes[9..13].try_into()?);
        let len = u32::from_be_bytes(bytes[13..17].try_into()?);

        // byte_off is u24 big-endian in bytes [17..20]
        let byte_off = ((bytes[17] as u32) << 16) | ((bytes[18] as u32) << 8) | (bytes[19] as u32);

        if len == 0 {
            return Err(eyre!("PdDataRead len must be > 0"));
        }

        let chunk_size_u32 = u32::try_from(chunk_config.chunk_size)
            .map_err(|_| eyre!("chunk_size does not fit in u32"))?;

        if byte_off >= chunk_size_u32 {
            return Err(eyre!(
                "PdDataRead byte_off ({byte_off}) must be < chunk_size ({chunk_size_u32})"
            ));
        }

        let spec = Self {
            partition_index,
            start,
            len,
            byte_off,
        };

        // Reject partition_index values that would overflow the ledger offset
        // calculation (num_chunks_in_partition * partition_index). Without this
        // check, an attacker can craft a PD tx that the PdService marks as Ready
        // (empty key set from overflow → no missing chunks) without any real data.
        if chunk_config
            .num_chunks_in_partition
            .checked_mul(partition_index)
            .is_none()
        {
            return Err(eyre!(
                "PdDataRead partition_index ({partition_index}) causes ledger offset overflow \
                 with num_chunks_in_partition ({})",
                chunk_config.num_chunks_in_partition
            ));
        }

        let chunks = spec.chunks_needed(chunk_config.chunk_size);
        let end = u64::from(start)
            .checked_add(chunks)
            .ok_or_else(|| eyre!("PdDataRead start + chunks_needed overflows u64"))?;

        if end > chunk_config.num_chunks_in_partition {
            return Err(eyre!(
                "PdDataRead exceeds partition boundary: start({start}) + chunks_needed({chunks}) = {end} > num_chunks({})",
                chunk_config.num_chunks_in_partition
            ));
        }

        Ok(spec)
    }

    /// Encode this `PdDataRead` into a 32-byte access list storage key.
    #[must_use]
    pub fn encode(&self) -> [u8; 32] {
        assert!(
            self.byte_off <= BYTE_OFF_MAX,
            "byte_off {:#010X} exceeds u24 max ({:#010X})",
            self.byte_off,
            BYTE_OFF_MAX
        );
        let mut buf = [0_u8; 32];
        buf[0] = PdAccessListArgsTypeId::DataRead as u8;
        buf[1..9].copy_from_slice(&self.partition_index.to_be_bytes());
        buf[9..13].copy_from_slice(&self.start.to_be_bytes());
        buf[13..17].copy_from_slice(&self.len.to_be_bytes());
        // byte_off as u24 big-endian
        buf[17] = (self.byte_off >> 16) as u8;
        buf[18] = (self.byte_off >> 8) as u8;
        buf[19] = self.byte_off as u8;
        // bytes [20..32] are already zero (reserved)
        buf
    }

    /// Number of chunks needed to satisfy this read.
    ///
    /// `ceil((byte_off + len) / chunk_size)`
    #[must_use]
    pub fn chunks_needed(&self, chunk_size: u64) -> u64 {
        let total_bytes = u64::from(self.byte_off) + u64::from(self.len);
        total_bytes.div_ceil(chunk_size)
    }
}

// ---------------------------------------------------------------------------
// Fee encoding (unchanged)
// ---------------------------------------------------------------------------

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

/// Typed error returned by [`decode_pd_fee`].
#[derive(Debug, thiserror::Error)]
pub enum PdFeeDecodeError {
    #[error("unexpected type byte")]
    UnexpectedTypeByte,
    #[error("fee must be > 0")]
    ZeroFee,
    #[error("fee exceeds u248 max")]
    FeeOverflow,
}

/// Decode a U256 fee from a 32-byte access list key.
/// The top byte of the U256 is forced to 0 (u248 bound by construction).
pub fn decode_pd_fee(bytes: &[u8; 32], expected_type: u8) -> Result<U256, PdFeeDecodeError> {
    if bytes[0] != expected_type {
        return Err(PdFeeDecodeError::UnexpectedTypeByte);
    }
    let mut be_bytes = [0_u8; 32];
    be_bytes[1..32].copy_from_slice(&bytes[1..32]);
    let fee = U256::from_be_bytes(be_bytes);
    if fee > MAX_PD_FEE {
        return Err(PdFeeDecodeError::FeeOverflow);
    }
    if fee == U256::ZERO {
        return Err(PdFeeDecodeError::ZeroFee);
    }
    Ok(fee)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_chunk_config() -> ChunkConfig {
        ChunkConfig {
            chunk_size: 262_144, // 256 KB
            num_chunks_in_partition: 51_872_000,
            entropy_packing_iterations: 0,
            chain_id: 1,
        }
    }

    // === PdDataRead tests ===

    #[test]
    fn roundtrip_basic() {
        let spec = PdDataRead {
            partition_index: 42,
            start: 100,
            len: 1000,
            byte_off: 500,
        };
        let encoded = spec.encode();
        let decoded = PdDataRead::decode(&encoded, &test_chunk_config()).unwrap();
        assert_eq!(spec, decoded);
    }

    #[test]
    fn zero_type_byte_rejected() {
        let bytes = [0_u8; 32]; // type byte 0x00
        assert!(PdDataRead::decode(&bytes, &test_chunk_config()).is_err());
    }

    #[test]
    fn zero_len_rejected() {
        let mut encoded = [0_u8; 32];
        encoded[0] = 0x01; // valid type byte but len=0
        assert!(PdDataRead::decode(&encoded, &test_chunk_config()).is_err());
    }

    #[test]
    fn byte_off_exceeds_chunk_size_rejected() {
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 100,
            byte_off: 262_144,
        };
        let encoded = spec.encode();
        assert!(PdDataRead::decode(&encoded, &test_chunk_config()).is_err());
    }

    #[test]
    fn nonzero_reserved_bytes_rejected() {
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 100,
            byte_off: 0,
        };
        let mut encoded = spec.encode();
        encoded[25] = 0xFF; // taint reserved byte
        assert!(PdDataRead::decode(&encoded, &test_chunk_config()).is_err());
    }

    #[test]
    fn partition_boundary_overflow_rejected() {
        let config = test_chunk_config();
        // start + chunks_needed would exceed num_chunks_in_partition
        let spec = PdDataRead {
            partition_index: 0,
            start: config.num_chunks_in_partition as u32, // at boundary
            len: 100,
            byte_off: 0,
        };
        let encoded = spec.encode();
        assert!(PdDataRead::decode(&encoded, &config).is_err());
    }

    #[test]
    fn chunks_needed_calculation() {
        let chunk_size = 262_144_u64;
        // Exactly one chunk
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 100,
            byte_off: 0,
        };
        assert_eq!(spec.chunks_needed(chunk_size), 1);

        // Spans two chunks due to byte_off
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 100,
            byte_off: 262_100,
        };
        assert_eq!(spec.chunks_needed(chunk_size), 2);
    }

    #[test]
    fn big_endian_encoding() {
        let spec = PdDataRead {
            partition_index: 0x0102030405060708,
            start: 0x090A0B0C,
            len: 0x0D0E0F10,
            byte_off: 0x111213,
        };
        let encoded = spec.encode();
        assert_eq!(encoded[0], 0x01); // type byte
        assert_eq!(
            &encoded[1..9],
            &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]
        );
        assert_eq!(&encoded[9..13], &[0x09, 0x0A, 0x0B, 0x0C]);
        assert_eq!(&encoded[13..17], &[0x0D, 0x0E, 0x0F, 0x10]);
        assert_eq!(&encoded[17..20], &[0x11, 0x12, 0x13]);
        assert_eq!(&encoded[20..32], &[0_u8; 12]); // reserved
    }

    #[test]
    fn byte_off_max_u24_accepted() {
        // byte_off at maximum u24 value should still be accepted if < chunk_size
        // With our default chunk_size of 262_144 (0x40000), byte_off of 262_143 is valid
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 1,
            byte_off: 262_143, // chunk_size - 1
        };
        let encoded = spec.encode();
        let decoded = PdDataRead::decode(&encoded, &test_chunk_config()).unwrap();
        assert_eq!(spec, decoded);
    }

    #[test]
    fn byte_off_u24_max_roundtrip() {
        // Verify that the maximum valid byte_off (0xFFFFFF) roundtrips correctly.
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 100,
            byte_off: BYTE_OFF_MAX,
        };
        let encoded = spec.encode();
        // Decode with a config that has chunk_size > BYTE_OFF_MAX
        let large_config = ChunkConfig {
            chunk_size: (BYTE_OFF_MAX as u64) + 1,
            num_chunks_in_partition: 51_872_000,
            entropy_packing_iterations: 0,
            chain_id: 1,
        };
        let decoded = PdDataRead::decode(&encoded, &large_config).unwrap();
        assert_eq!(decoded.byte_off, BYTE_OFF_MAX);
    }

    #[test]
    fn chunks_needed_exact_boundary() {
        let chunk_size = 262_144_u64;
        // byte_off + len = exactly chunk_size => 1 chunk
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 262_144,
            byte_off: 0,
        };
        assert_eq!(spec.chunks_needed(chunk_size), 1);

        // byte_off + len = chunk_size + 1 => 2 chunks
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 262_144 - 99,
            byte_off: 100,
        };
        assert_eq!(spec.chunks_needed(chunk_size), 2);
    }

    // === Fee encoding tests (preserved from original) ===

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
        assert!(matches!(result, Err(PdFeeDecodeError::UnexpectedTypeByte)));

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
}
