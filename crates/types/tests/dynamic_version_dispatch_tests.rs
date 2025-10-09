use irys_types::{
    CommitmentTransaction, CommitmentTransactionV1, ConsensusConfig, DataTransactionHeader,
    DataTransactionHeaderV1, IrysBlockHeader, IrysBlockHeaderV1, VersionDiscriminant as _,
};

#[test]
fn data_tx_v1_construction_from_inner() {
    // Test that we can construct a versioned type from an inner V1 type
    let header = DataTransactionHeaderV1::default();
    let versioned = DataTransactionHeader::V1(header);
    assert_eq!(versioned.discriminant(), 1);
}

#[test]
fn data_tx_uses_new_constructor() {
    // Prefer using the public constructor for normal usage
    let config = ConsensusConfig::testing();
    let versioned = DataTransactionHeader::new(&config);
    assert_eq!(versioned.discriminant(), 1);
}

#[test]
fn block_header_v1_construction_from_inner() {
    // Test that we can construct a versioned type from an inner V1 type
    let header = IrysBlockHeaderV1 {
        version: 1,
        ..Default::default()
    };
    let versioned = IrysBlockHeader::V1(header);
    assert_eq!(versioned.version, 1);
}

#[test]
fn block_header_uses_new_constructor() {
    // Prefer using the public constructor for normal usage
    let versioned = IrysBlockHeader::new_mock_header();
    assert_eq!(versioned.version, 1);
}

#[test]
fn commitment_tx_v1_construction_from_inner() {
    // Test that we can construct a versioned type from an inner V1 type
    let tx = CommitmentTransactionV1::default();
    let versioned = CommitmentTransaction::V1(tx);
    assert_eq!(versioned.discriminant(), 1);
}

#[test]
fn commitment_tx_uses_new_constructor() {
    // Prefer using the public constructor for normal usage
    let config = ConsensusConfig::testing();
    let versioned = CommitmentTransaction::new_stake(&config, irys_types::H256::zero());
    assert_eq!(versioned.discriminant(), 1);
}
