use irys_types::{IrysBlockHeader, VersionedIrysBlockHeader, DataTransactionHeader, VersionedDataTransactionHeader, CommitmentTransaction, VersionedCommitmentTransaction, Signable};

#[test]
fn block_header_preimage_starts_with_discriminant() {
    let header = IrysBlockHeader::new_mock_header();
    let versioned = header.into_versioned();
    let mut buf = Vec::new();
    versioned.encode_for_signing(&mut buf);
    assert_eq!(buf.first().copied(), Some(1), "discriminant must be first byte");
    assert!(buf.len() > 1, "preimage should contain more than discriminant");
}

#[test]
fn data_tx_preimage_starts_with_discriminant() {
    let mut tx = DataTransactionHeader::default();
    tx.version = 1;
    let versioned = tx.into_versioned();
    let mut buf = Vec::new();
    versioned.encode_for_signing(&mut buf);
    assert_eq!(buf.first().copied(), Some(1));
}

#[test]
fn commitment_tx_preimage_starts_with_discriminant() {
    use irys_types::ConsensusConfig;
    let config = ConsensusConfig::testing();
    let tx = CommitmentTransaction::new_stake(&config, irys_types::H256::zero());
    let versioned = tx.into_versioned();
    let mut buf = Vec::new();
    versioned.encode_for_signing(&mut buf);
    assert_eq!(buf.first().copied(), Some(1));
}
