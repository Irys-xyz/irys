use irys_types::{
    serialization::Base64, IrysAddress, IrysSignature, Signature, UnixTimestampMs, H256,
};

pub(crate) fn test_h256(byte: u8) -> H256 {
    H256::from([byte; 32])
}

pub(crate) fn test_address(byte: u8) -> IrysAddress {
    IrysAddress::from_slice(&[byte; 20])
}

pub(crate) fn test_signature() -> IrysSignature {
    IrysSignature::new(Signature::new(
        reth::revm::primitives::U256::from(1_u64),
        reth::revm::primitives::U256::from(2_u64),
        false,
    ))
}

pub(crate) fn test_base64(bytes: &[u8]) -> Base64 {
    Base64(bytes.to_vec())
}

pub(crate) fn test_unix_timestamp() -> UnixTimestampMs {
    UnixTimestampMs::from(1_700_000_000_000_u128)
}
