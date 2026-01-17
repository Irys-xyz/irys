use irys_types::{
    CommitmentTransaction, CommitmentTransactionV1, ConsensusConfig, DataTransactionHeader,
    DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata, DataTransactionMetadata,
    IrysBlockHeader, IrysBlockHeaderV1, VersionDiscriminant as _,
};

#[test]
fn data_tx_v1_construction_from_inner() {
    // Test that we can construct a versioned type from an inner V1 type
    let header = DataTransactionHeaderV1::default();
    let versioned = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
        tx: header,
        metadata: DataTransactionMetadata::new(),
    });
    assert_eq!(versioned.version(), 1);
}

#[test]
fn data_tx_uses_new_constructor() {
    // Prefer using the public constructor for normal usage
    let config = ConsensusConfig::testing();
    let versioned = DataTransactionHeader::new(&config);
    assert_eq!(versioned.version(), 1);
}

#[test]
fn block_header_v1_construction_from_inner() {
    // Test that we can construct a versioned type from an inner V1 type
    let header = IrysBlockHeaderV1::default();

    let versioned = IrysBlockHeader::V1(header);
    assert_eq!(versioned.version(), 1);
}

#[test]
fn block_header_uses_new_constructor() {
    // Prefer using the public constructor for normal usage
    let versioned = IrysBlockHeader::new_mock_header();
    assert_eq!(versioned.version(), 1);
}

#[test]
fn commitment_tx_v1_construction_from_inner() {
    // Test that we can construct a versioned type from an inner V1 type
    let tx = CommitmentTransactionV1::default();
    let versioned = CommitmentTransaction::V1(irys_types::CommitmentV1WithMetadata {
        tx,
        metadata: Default::default(),
    });
    assert_eq!(versioned.version(), 1);
}

#[test]
fn commitment_tx_uses_new_constructor() {
    // Prefer using the public constructor for normal usage
    let config = ConsensusConfig::testing();
    let versioned = CommitmentTransaction::new_stake(&config, irys_types::H256::zero());
    assert_eq!(versioned.version(), 2);
}

#[test]
fn data_tx_metadata_does_not_affect_equality() {
    // Test that metadata does not affect equality comparison
    let header = DataTransactionHeaderV1::default();

    let mut tx1 = DataTransactionHeaderV1WithMetadata {
        tx: header.clone(),
        metadata: DataTransactionMetadata::new(),
    };

    let mut tx2 = DataTransactionHeaderV1WithMetadata {
        tx: header,
        metadata: DataTransactionMetadata::new(),
    };

    // Initially equal
    assert_eq!(tx1, tx2);
    assert_eq!(tx1.cmp(&tx2), std::cmp::Ordering::Equal);

    // Mutate metadata on one
    tx1.metadata.included_height = Some(100);
    tx2.metadata.included_height = Some(200);
    tx1.metadata.promoted_height = Some(50);
    tx2.metadata.promoted_height = None;

    // Still equal despite different metadata
    assert_eq!(tx1, tx2);
    assert_eq!(tx1.cmp(&tx2), std::cmp::Ordering::Equal);
}
