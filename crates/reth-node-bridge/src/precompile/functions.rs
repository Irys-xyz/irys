#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[repr(u8)]
/// Decoded from the first byte of calldata to determine which function is being called
pub enum PdFunctionId {
    ReadBytesFirstRange = 0,
    ReadBytesFirstRangeWithArgs,
}

#[derive(thiserror::Error, Debug)]
pub enum PdFunctionIdDecodeError {
    #[error("unknown reserved PD function ID: {0}")]
    UnknownPdFunctionId(u8),
}

impl TryFrom<u8> for PdFunctionId {
    type Error = PdFunctionIdDecodeError;
    fn try_from(id: u8) -> Result<Self, Self::Error> {
        match id {
            0 => Ok(PdFunctionId::ReadBytesFirstRange),
            1 => Ok(PdFunctionId::ReadBytesFirstRangeWithArgs),
            _ => Err(PdFunctionIdDecodeError::UnknownPdFunctionId(id)),
        }
    }
}
