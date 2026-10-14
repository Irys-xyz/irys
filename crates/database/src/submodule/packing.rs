//! Packing identity stored in the RocksDB v1 `Metadata` family.
//!
//! The value is raw. It is not a hash and not SCALE. A later hardfork can
//! accept an older iteration count in [`packing_params_valid_for_module`]
//! without changing these bytes.

use std::fmt;

use irys_types::IrysAddress;

/// First byte of [`PackingRecord::encode`].
pub const PACKING_RECORD_VERSION: u8 = 1;

/// `version` + address + iterations + chunk size + chain id.
pub const PACKING_RECORD_LEN: usize = 41;

/// What one storage module was packed with.
///
/// Layout: `version u8 = 1 | packing address 20 bytes | iterations u32 BE |
/// chunk size u64 BE | chain id u64 BE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackingRecord {
    pub packing_address: [u8; 20],
    pub iterations: u32,
    pub chunk_size: u64,
    pub chain_id: u64,
}

impl PackingRecord {
    pub fn encode(self) -> [u8; PACKING_RECORD_LEN] {
        let mut out = [0_u8; PACKING_RECORD_LEN];
        out[0] = PACKING_RECORD_VERSION;
        out[1..21].copy_from_slice(&self.packing_address);
        out[21..25].copy_from_slice(&self.iterations.to_be_bytes());
        out[25..33].copy_from_slice(&self.chunk_size.to_be_bytes());
        out[33..41].copy_from_slice(&self.chain_id.to_be_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> eyre::Result<Self> {
        eyre::ensure!(
            bytes.len() == PACKING_RECORD_LEN,
            "packing record is {} bytes, expected {PACKING_RECORD_LEN}",
            bytes.len()
        );
        eyre::ensure!(
            bytes[0] == PACKING_RECORD_VERSION,
            "packing record version is {}, expected {PACKING_RECORD_VERSION}",
            bytes[0]
        );
        Ok(Self {
            packing_address: bytes[1..21].try_into().expect("length checked"),
            iterations: u32::from_be_bytes(bytes[21..25].try_into().expect("length checked")),
            chunk_size: u64::from_be_bytes(bytes[25..33].try_into().expect("length checked")),
            chain_id: u64::from_be_bytes(bytes[33..41].try_into().expect("length checked")),
        })
    }
}

impl fmt::Display for PackingRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "version={PACKING_RECORD_VERSION} address={} iterations={} chunk_size={} chain_id={}",
            IrysAddress::from(self.packing_address),
            self.iterations,
            self.chunk_size,
            self.chain_id
        )
    }
}

/// Whether `recorded` is the packing this module may open.
///
/// Today `recorded` must equal `expected`. `expected` is the miner address
/// and the consensus iterations, chunk size, and chain id. A later hardfork
/// can accept an older iteration count for one partition here. The record
/// layout stays the same. A refusal names both values.
pub fn packing_params_valid_for_module(
    recorded: &PackingRecord,
    expected: &PackingRecord,
) -> eyre::Result<()> {
    if recorded == expected {
        return Ok(());
    }
    eyre::bail!(
        "packing identity does not match this storage module. recorded: {recorded} expected: {expected}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PackingRecord {
        PackingRecord {
            packing_address: [0xab; 20],
            iterations: 0x0102_0304,
            chunk_size: 0x1112_1314_1516_1718,
            chain_id: 0x2122_2324_2526_2728,
        }
    }

    #[test]
    fn packing_record_round_trip() {
        let record = sample();
        let bytes = record.encode();
        assert_eq!(bytes.len(), PACKING_RECORD_LEN);
        assert_eq!(bytes[0], PACKING_RECORD_VERSION);
        assert_eq!(&bytes[1..21], &[0xab; 20]);
        assert_eq!(&bytes[21..25], &0x0102_0304_u32.to_be_bytes());
        assert_eq!(&bytes[25..33], &0x1112_1314_1516_1718_u64.to_be_bytes());
        assert_eq!(&bytes[33..41], &0x2122_2324_2526_2728_u64.to_be_bytes());
        assert_eq!(PackingRecord::decode(&bytes).unwrap(), record);

        let mut other_version = bytes;
        other_version[0] = 2;
        assert!(PackingRecord::decode(&other_version).is_err());
        assert!(PackingRecord::decode(&bytes[..40]).is_err());
    }

    #[test]
    fn packing_params_mismatch_prints_recorded_and_expected() {
        let recorded = sample();
        let expected = PackingRecord {
            iterations: 9,
            chunk_size: 64,
            ..recorded
        };
        let err = packing_params_valid_for_module(&recorded, &expected)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not match"), "{err}");
        assert!(err.contains("recorded:"), "{err}");
        assert!(err.contains("expected:"), "{err}");
        assert!(
            err.contains(&format!("iterations={}", recorded.iterations)),
            "{err}"
        );
        assert!(
            err.contains(&format!("iterations={}", expected.iterations)),
            "{err}"
        );
        assert!(
            err.contains(&format!("chunk_size={}", recorded.chunk_size)),
            "{err}"
        );
        assert!(
            err.contains(&format!("chunk_size={}", expected.chunk_size)),
            "{err}"
        );
        assert!(packing_params_valid_for_module(&recorded, &recorded).is_ok());
    }
}
