#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[repr(u8)]
/// Decoded from the first byte of calldata to determine which function is being called
pub enum PdFunctionId {
    ReadFullByteRange = 0,
    ReadPartialByteRange,
}

#[derive(thiserror::Error, Debug)]
pub enum PdFunctionIdDecodeError {
    #[error("unknown reserved PD function ID: {0}")]
    UnknownPdFunctionId(u8),
}

impl TryFrom<u8> for PdFunctionId {
    type Error = PdFunctionIdDecodeError;
    fn try_from(id: u8) -> Result<Self, Self::Error> {
        // the EVM is BE
        let id = u8::from_be(id);
        match id {
            0 => Ok(PdFunctionId::ReadFullByteRange),
            1 => Ok(PdFunctionId::ReadPartialByteRange),
            _ => Err(PdFunctionIdDecodeError::UnknownPdFunctionId(id)),
        }
    }
}
