use crate::reth_db::{
    DatabaseError,
    table::{Decode, Encode},
};
use serde::{Deserialize, Serialize};

#[repr(u32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub enum MetadataKey {
    /// Raw schema text. The same bytes as the `SCHEMA` file. Not SCALE.
    DBSchemaVersion = 1,
    /// Raw byte `01` while a reset is clearing the index. Absent when the
    /// reset is finished. Not a schema version. Not SCALE.
    SubmoduleResetInProgress = 2,
    /// Raw 32-byte partition hash. Absent until the module is assigned.
    /// A later different hash refuses to open.
    PartitionHash = 3,
    /// Raw packing record, 41 bytes: version `u8` = 1, packing address
    /// 20 bytes, iterations `u32` big-endian, chunk size `u64` big-endian,
    /// chain id `u64` big-endian. A mismatch refuses to open.
    PackingRecord = 4,
    /// Raw byte `01` after a scan finds a checksum on every offset. Absent
    /// means unset. Reset deletes the key. `clear` does not. Not SCALE.
    ChunkCrcComplete = 5,
    /// Feature bits for a RocksDB v1 index.
    ///
    /// Raw 24 bytes: `compat`, `ro_compat`, and `incompat`, each a
    /// little-endian u64. Absent is the same as all zeros. This release
    /// defines no bits. An unknown `incompat` bit refuses open. An unknown
    /// `ro_compat` bit refuses a writable open. An unknown `compat` bit is
    /// ignored.
    FeatureFlags = 6,
}

impl Encode for MetadataKey {
    type Encoded = [u8; 4];

    fn encode(self) -> Self::Encoded {
        // LE encoding for backward compatibility with existing databases.
        // NOT using reth's u32 BE convention because the Metadata table was
        // created with LE keys and changing endianness would make existing
        // schema-version entries invisible (different on-disk key bytes).
        (self as u32).to_le_bytes()
    }
}

impl Decode for MetadataKey {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        // Little-endian, matching [`Encode`]. Reth's `u32` decode is big-endian.
        let arr: [u8; 4] = value.try_into().map_err(|_| DatabaseError::Decode)?;
        match u32::from_le_bytes(arr) {
            1 => Ok(Self::DBSchemaVersion),
            2 => Ok(Self::SubmoduleResetInProgress),
            3 => Ok(Self::PartitionHash),
            4 => Ok(Self::PackingRecord),
            5 => Ok(Self::ChunkCrcComplete),
            6 => Ok(Self::FeatureFlags),
            _ => Err(DatabaseError::Decode),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rstest::rstest;

    #[test]
    fn metadata_key_encode_decode_roundtrip() {
        let key = MetadataKey::DBSchemaVersion;
        let encoded = key.encode();
        let decoded = MetadataKey::decode(&encoded).unwrap();
        assert_eq!(decoded, key);
    }

    #[test]
    fn metadata_key_encoding_is_le_u32() {
        let encoded = MetadataKey::DBSchemaVersion.encode();
        assert_eq!(encoded, 1_u32.to_le_bytes());
        assert_eq!(
            MetadataKey::SubmoduleResetInProgress.encode(),
            2_u32.to_le_bytes()
        );
    }

    #[test]
    fn reset_marker_key_roundtrips() {
        let key = MetadataKey::SubmoduleResetInProgress;
        assert_eq!(MetadataKey::decode(&key.encode()).unwrap(), key);
    }

    proptest! {
        #[test]
        fn metadata_key_decode_rejects_invalid_discriminants(val in any::<u32>()) {
            prop_assume!(
                val != 1 && val != 2 && val != 3 && val != 4 && val != 5 && val != 6
            );
            let bytes = val.to_le_bytes();
            prop_assert!(MetadataKey::decode(&bytes).is_err());
        }
    }

    #[rstest]
    #[case(&[], "empty slice")]
    #[case(&[0], "1 byte")]
    #[case(&[0, 0, 0], "3 bytes")]
    #[case(&[0, 0, 0, 0, 0], "5 bytes")]
    fn metadata_key_decode_rejects_wrong_length(#[case] input: &[u8], #[case] _desc: &str) {
        assert!(MetadataKey::decode(input).is_err());
    }
}
