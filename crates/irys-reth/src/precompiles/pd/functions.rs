//! PD precompile function identifiers.

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PdFunctionId {
    ReadFullByteRange = 0,
    ReadPartialByteRange = 1,
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
            0 => Ok(Self::ReadFullByteRange),
            1 => Ok(Self::ReadPartialByteRange),
            _ => Err(PdFunctionIdDecodeError::UnknownPdFunctionId(id)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case(2, 2)]
    #[case(99, 99)]
    #[case(255, 255)]
    fn test_invalid_function_id_returns_error(#[case] id: u8, #[case] expected_id: u8) {
        let err =
            PdFunctionId::try_from(id).expect_err(&format!("Should fail for function ID {}", id));

        assert!(
            matches!(err, PdFunctionIdDecodeError::UnknownPdFunctionId(_)),
            "Expected UnknownPdFunctionId error"
        );

        let PdFunctionIdDecodeError::UnknownPdFunctionId(unknown_id) = err;
        assert_eq!(unknown_id, expected_id);
    }
}
