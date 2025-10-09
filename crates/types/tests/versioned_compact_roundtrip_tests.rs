use irys_types::{
    CommitmentTransaction, Compact as _, ConsensusConfig, DataTransactionHeader, IrysBlockHeader,
    VersionDiscriminant as _, H256,
};

#[test]
fn test_versioned_block_header_compact_roundtrip() {
    let versioned = IrysBlockHeader::new_mock_header();

    // Encode to compact format
    let mut buf = Vec::new();
    versioned.to_compact(&mut buf);

    assert!(!buf.is_empty(), "buffer should contain encoded data");

    // Check that the first byte is the discriminant (1)
    assert_eq!(buf[0], 1, "first byte should be the discriminant");

    // Decode from compact format
    let (decoded_versioned, rest) = IrysBlockHeader::from_compact(&buf, buf.len());

    assert!(rest.is_empty(), "entire buffer should be consumed");

    // Verify the decoded version matches the original
    assert_eq!(decoded_versioned, versioned);
    assert_eq!(decoded_versioned.version(), 1);
}

#[test]
fn test_versioned_data_transaction_header_compact_roundtrip() {
    let config = ConsensusConfig::testing();
    let mut versioned = DataTransactionHeader::new(&config);
    versioned.id = H256::random();

    // Encode to compact format
    let mut buf = Vec::new();
    versioned.to_compact(&mut buf);

    assert!(!buf.is_empty(), "buffer should contain encoded data");

    // Check that the first byte is the discriminant (1)
    assert_eq!(buf[0], 1, "first byte should be the discriminant");

    // Decode from compact format
    let (decoded_versioned, rest) = DataTransactionHeader::from_compact(&buf, buf.len());

    assert!(rest.is_empty(), "entire buffer should be consumed");

    // Verify the decoded version matches the original
    assert_eq!(decoded_versioned, versioned);
    assert_eq!(decoded_versioned.version(), 1);
}

#[test]
fn test_versioned_commitment_transaction_compact_roundtrip() {
    let config = ConsensusConfig::testing();
    let mut versioned = CommitmentTransaction::new_stake(&config, H256::random());
    versioned.id = H256::random();

    // Encode to compact format
    let mut buf = Vec::new();
    versioned.to_compact(&mut buf);

    assert!(!buf.is_empty(), "buffer should contain encoded data");

    // Check that the first byte is the discriminant (1)
    assert_eq!(buf[0], 1, "first byte should be the discriminant");

    // Decode from compact format
    let (decoded_versioned, rest) = CommitmentTransaction::from_compact(&buf, buf.len());

    assert!(rest.is_empty(), "entire buffer should be consumed");

    // Verify the decoded version matches the original
    assert_eq!(decoded_versioned, versioned);
    assert_eq!(decoded_versioned.version(), 1);
}

#[test]
fn test_versioned_block_header_compact_with_default() {
    // Test with Default implementation which sets version = 1
    let versioned = IrysBlockHeader::default();
    assert_eq!(versioned.version(), 1, "default should set version to 1");

    let mut buf = Vec::new();
    versioned.to_compact(&mut buf);

    let (decoded_versioned, rest) = IrysBlockHeader::from_compact(&buf, buf.len());

    assert!(rest.is_empty());
    assert_eq!(decoded_versioned, versioned);
}

#[test]
fn test_versioned_data_transaction_header_compact_with_default() {
    // Test with Default implementation which sets version = 1
    let versioned = DataTransactionHeader::default();
    assert_eq!(versioned.version(), 1, "default should set version to 1");

    let mut buf = Vec::new();
    versioned.to_compact(&mut buf);

    let (decoded_versioned, rest) = DataTransactionHeader::from_compact(&buf, buf.len());

    assert!(rest.is_empty());
    assert_eq!(decoded_versioned, versioned);
}

#[test]
fn test_versioned_commitment_transaction_compact_with_default() {
    // Test with Default implementation which sets version = 1
    let versioned = CommitmentTransaction::default();
    assert_eq!(versioned.version(), 1, "default should set version to 1");

    let mut buf = Vec::new();
    versioned.to_compact(&mut buf);

    let (decoded_versioned, rest) = CommitmentTransaction::from_compact(&buf, buf.len());

    assert!(rest.is_empty());
    assert_eq!(decoded_versioned, versioned);
}

#[test]
#[should_panic(expected = "UnsupportedVersion")]
fn test_versioned_block_header_from_compact_panics_on_unsupported_version() {
    // Create a buffer with an unsupported discriminant (99)
    let mut buf = vec![99_u8];
    // Add some dummy data (the compact representation expects more than just the discriminant)
    buf.extend_from_slice(&[0_u8; 100]);

    // This should panic with UnsupportedVersion error
    let _ = IrysBlockHeader::from_compact(&buf, buf.len());
}

#[test]
#[should_panic(expected = "UnsupportedVersion")]
fn test_versioned_data_transaction_header_from_compact_panics_on_unsupported_version() {
    // Create a buffer with an unsupported discriminant (99)
    let mut buf = vec![99_u8];
    buf.extend_from_slice(&[0_u8; 100]);

    // This should panic with UnsupportedVersion error
    let _ = DataTransactionHeader::from_compact(&buf, buf.len());
}

#[test]
#[should_panic(expected = "UnsupportedVersion")]
fn test_versioned_commitment_transaction_from_compact_panics_on_unsupported_version() {
    // Create a buffer with an unsupported discriminant (99)
    let mut buf = vec![99_u8];
    buf.extend_from_slice(&[0_u8; 100]);

    // This should panic with UnsupportedVersion error
    let _ = CommitmentTransaction::from_compact(&buf, buf.len());
}
