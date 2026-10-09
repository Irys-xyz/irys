use crate::reth_db::{
    DatabaseError,
    table::{Decode, Encode},
};
use serde::{Deserialize, Serialize};

#[repr(u32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub enum MetadataKey {
    DBSchemaVersion = 1,
    /// Set before a storage-module reset clears the index. Open finishes the
    /// clear while this row is present. It is not a schema version.
    SubmoduleResetInProgress = 2,
    /// Partition this schema-v3 index was bound to. Absent until the module
    /// is assigned. A later different hash refuses to open.
    PartitionHash = 3,
    /// Hash of the packing address, iterations, chunk size, and chain id.
    /// A mismatch refuses to open a schema-v3 index.
    PackingParamsHash = 4,
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
        // LE decoding to match LE encode — do NOT delegate to reth's
        // u32::decode which uses BE, as our keys are stored in LE.
        let arr: [u8; 4] = value.try_into().map_err(|_| DatabaseError::Decode)?;
        match u32::from_le_bytes(arr) {
            1 => Ok(Self::DBSchemaVersion),
            2 => Ok(Self::SubmoduleResetInProgress),
            3 => Ok(Self::PartitionHash),
            4 => Ok(Self::PackingParamsHash),
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
            prop_assume!(val != 1 && val != 2 && val != 3 && val != 4);
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
