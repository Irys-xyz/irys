use irys_types::{CommitmentTransaction, DataTransactionHeader, Signable};

#[test]
fn data_tx_preimage_starts_with_discriminant() {
    let mut tx = DataTransactionHeader::default();
    tx.version = 1;
    let versioned = tx.try_into_versioned().unwrap();
    let mut buf = Vec::new();
    versioned.encode_for_signing(&mut buf);
    assert_eq!(buf.first().copied(), Some(1));
}

#[test]
fn commitment_tx_preimage_starts_with_discriminant() {
    use irys_types::ConsensusConfig;
    let config = ConsensusConfig::testing();
    let tx = CommitmentTransaction::new_stake(&config, irys_types::H256::zero());
    let mut tx = tx; // need mutable to set version
    tx.version = 1;
    let versioned = tx.try_into_versioned().unwrap();
    let mut buf = Vec::new();
    versioned.encode_for_signing(&mut buf);
    assert_eq!(buf.first().copied(), Some(1));
}
