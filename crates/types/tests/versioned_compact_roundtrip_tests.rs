use irys_types::{
    Compact, CommitmentTransaction, ConsensusConfig, DataTransactionHeader, IrysBlockHeader,
    VersionedCommitmentTransaction, VersionedDataTransactionHeader, VersionedIrysBlockHeader, H256,
};

#[test]
fn test_versioned_block_header_compact_roundtrip() {
    let mut header = IrysBlockHeader::new_mock_header();
    header.version = 1;

    // Convert to versioned wrapper
    let versioned = header.clone().try_into_versioned().expect("version 1 should be supported");

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
    assert_eq!(*decoded_versioned, header);
    assert_eq!(decoded_versioned.version, 1);
}

#[test]
fn test_versioned_data_transaction_header_compact_roundtrip() {
    let config = ConsensusConfig::testing();
    let mut tx_header = DataTransactionHeader::new(&config);
    tx_header.id = H256::random();
    tx_header.version = 1;

    // Convert to versioned wrapper
    let versioned = tx_header
        .clone()
        .try_into_versioned()
        .expect("version 1 should be supported");

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
    assert_eq!(*decoded_versioned, tx_header);
    assert_eq!(decoded_versioned.version, 1);
}

#[test]
fn test_versioned_commitment_transaction_compact_roundtrip() {
    let config = ConsensusConfig::testing();
    let mut commitment_tx = CommitmentTransaction::new_stake(&config, H256::random());
    commitment_tx.id = H256::random();
    commitment_tx.version = 1;

    // Convert to versioned wrapper
    let versioned = commitment_tx
        .clone()
        .try_into_versioned()
        .expect("version 1 should be supported");

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
    assert_eq!(*decoded_versioned, commitment_tx);
    assert_eq!(decoded_versioned.version, 1);
}

#[test]
fn test_versioned_block_header_compact_with_default() {
    // Test with Default implementation which sets version = 1
    let header = IrysBlockHeader::default();
    assert_eq!(header.version, 1, "default should set version to 1");

    let versioned = header
        .clone()
        .try_into_versioned()
        .expect("default version should be supported");

    let mut buf = Vec::new();
    versioned.to_compact(&mut buf);

    let (decoded_versioned, rest) = VersionedIrysBlockHeader::from_compact(&buf, buf.len());

    assert!(rest.is_empty());
    assert_eq!(*decoded_versioned, header);
}

#[test]
fn test_versioned_data_transaction_header_compact_with_default() {
    // Test with Default implementation which sets version = 1
    let tx_header = DataTransactionHeader::default();
    assert_eq!(tx_header.version, 1, "default should set version to 1");

    let versioned = tx_header
        .clone()
        .try_into_versioned()
        .expect("default version should be supported");

    let mut buf = Vec::new();
    versioned.to_compact(&mut buf);

    let (decoded_versioned, rest) = VersionedDataTransactionHeader::from_compact(&buf, buf.len());

    assert!(rest.is_empty());
    assert_eq!(*decoded_versioned, tx_header);
}

#[test]
fn test_versioned_commitment_transaction_compact_with_default() {
    // Test with Default implementation which sets version = 1
    let commitment_tx = CommitmentTransaction::default();
    assert_eq!(
        commitment_tx.version, 1,
        "default should set version to 1"
    );

    let versioned = commitment_tx
        .clone()
        .try_into_versioned()
        .expect("default version should be supported");

    let mut buf = Vec::new();
    versioned.to_compact(&mut buf);

    let (decoded_versioned, rest) =
        VersionedCommitmentTransaction::from_compact(&buf, buf.len());

    assert!(rest.is_empty());
    assert_eq!(*decoded_versioned, commitment_tx);
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
