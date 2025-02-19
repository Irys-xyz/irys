use crate::reth_db::{
    table::{Decode, Encode},
    DatabaseError,
};
use serde::{Deserialize, Serialize};

#[repr(u32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub enum MetadataKey {
    DBSchemaVersion = 1,
}

impl Encode for MetadataKey {
    type Encoded = [u8; 4];

    fn encode(self) -> Self::Encoded {
        (self as u32).to_le_bytes()
    }
}

impl Decode for MetadataKey {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        let value = u32::decode(value)?;
        match value {
            1 => Ok(MetadataKey::DBSchemaVersion),
            _ => Err(DatabaseError::Decode),
        }
    }
}
