use irys_types::{
    Compact, VersionedCommitmentTransaction, ConsensusConfig, VersionedDataTransactionHeader, VersionedIrysBlockHeader,
    H256,
};

#[test]
fn test_versioned_block_header_compact_roundtrip() {
    let versioned = VersionedIrysBlockHeader::new_mock_header();

    // Encode to compact format
    let mut buf = Vec::new();
    versioned.to_compact(&mut buf);

    assert!(!buf.is_empty(), "buffer should contain encoded data");

    // Check that the first byte is the discriminant (1)
    assert_eq!(buf[0], 1, "first byte should be the discriminant");

    // Decode from compact format
    let (decoded_versioned, rest) = VersionedIrysBlockHeader::from_compact(&buf, buf.len());

    assert!(rest.is_empty(), "entire buffer should be consumed");

    // Verify the decoded version matches the original
    assert_eq!(decoded_versioned, versioned);
    assert_eq!(decoded_versioned.version, 1);
}

#[test]
fn test_versioned_data_transaction_header_compact_roundtrip() {
    let config = ConsensusConfig::testing();
    let mut versioned = VersionedDataTransactionHeader::new(&config);
    versioned.id = H256::random();

    // Encode to compact format
    let mut buf = Vec::new();
    versioned.to_compact(&mut buf);

    assert!(!buf.is_empty(), "buffer should contain encoded data");

    // Check that the first byte is the discriminant (1)
    assert_eq!(buf[0], 1, "first byte should be the discriminant");

    // Decode from compact format
    let (decoded_versioned, rest) = VersionedDataTransactionHeader::from_compact(&buf, buf.len());

    assert!(rest.is_empty(), "entire buffer should be consumed");

    // Verify the decoded version matches the original
    assert_eq!(decoded_versioned, versioned);
    assert_eq!(decoded_versioned.version, 1);
}

#[test]
fn test_versioned_commitment_transaction_compact_roundtrip() {
    let config = ConsensusConfig::testing();
    let mut versioned = VersionedCommitmentTransaction::new_stake(&config, H256::random());
    versioned.id = H256::random();

    // Encode to compact format
    let mut buf = Vec::new();
    versioned.to_compact(&mut buf);

    assert!(!buf.is_empty(), "buffer should contain encoded data");

    // Check that the first byte is the discriminant (1)
    assert_eq!(buf[0], 1, "first byte should be the discriminant");

    // Decode from compact format
    let (decoded_versioned, rest) =
        VersionedCommitmentTransaction::from_compact(&buf, buf.len());

    assert!(rest.is_empty(), "entire buffer should be consumed");

    // Verify the decoded version matches the original
    assert_eq!(decoded_versioned, versioned);
    assert_eq!(decoded_versioned.version, 1);
}

#[test]
fn test_versioned_block_header_compact_with_default() {
    // Test with Default implementation which sets version = 1
    let versioned = VersionedIrysBlockHeader::default();
    assert_eq!(versioned.version, 1, "default should set version to 1");

    let mut buf = Vec::new();
    versioned.to_compact(&mut buf);

    let (decoded_versioned, rest) = VersionedIrysBlockHeader::from_compact(&buf, buf.len());

    assert!(rest.is_empty());
    assert_eq!(decoded_versioned, versioned);
}

#[test]
fn test_versioned_data_transaction_header_compact_with_default() {
    // Test with Default implementation which sets version = 1
    let versioned = VersionedDataTransactionHeader::default();
    assert_eq!(versioned.version, 1, "default should set version to 1");

    let mut buf = Vec::new();
    versioned.to_compact(&mut buf);

    let (decoded_versioned, rest) = VersionedDataTransactionHeader::from_compact(&buf, buf.len());

    assert!(rest.is_empty());
    assert_eq!(decoded_versioned, versioned);
}

#[test]
fn test_versioned_commitment_transaction_compact_with_default() {
    // Test with Default implementation which sets version = 1
    let versioned = VersionedCommitmentTransaction::default();
    assert_eq!(
        versioned.version, 1,
        "default should set version to 1"
    );

    let mut buf = Vec::new();
    versioned.to_compact(&mut buf);

    let (decoded_versioned, rest) =
        VersionedCommitmentTransaction::from_compact(&buf, buf.len());

    assert!(rest.is_empty());
    assert_eq!(decoded_versioned, versioned);
}

#[test]
#[should_panic(expected = "UnsupportedVersion")]
fn test_versioned_block_header_from_compact_panics_on_unsupported_version() {
    // Create a buffer with an unsupported discriminant (99)
    let mut buf = vec![99_u8];
    // Add some dummy data (the compact representation expects more than just the discriminant)
    buf.extend_from_slice(&[0u8; 100]);

    // This should panic with UnsupportedVersion error
    let _ = VersionedIrysBlockHeader::from_compact(&buf, buf.len());
}

#[test]
#[should_panic(expected = "UnsupportedVersion")]
fn test_versioned_data_transaction_header_from_compact_panics_on_unsupported_version() {
    // Create a buffer with an unsupported discriminant (99)
    let mut buf = vec![99_u8];
    buf.extend_from_slice(&[0u8; 100]);

    // This should panic with UnsupportedVersion error
    let _ = VersionedDataTransactionHeader::from_compact(&buf, buf.len());
}

#[test]
#[should_panic(expected = "UnsupportedVersion")]
fn test_versioned_commitment_transaction_from_compact_panics_on_unsupported_version() {
    // Create a buffer with an unsupported discriminant (99)
    let mut buf = vec![99_u8];
    buf.extend_from_slice(&[0u8; 100]);

    // This should panic with UnsupportedVersion error
    let _ = VersionedCommitmentTransaction::from_compact(&buf, buf.len());
}
