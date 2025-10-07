use irys_types::{CommitmentTransaction, ConsensusConfig, DataTransactionHeader};

#[test]
fn data_tx_try_into_versioned_returns_err_on_version_mismatch() {
    let config = ConsensusConfig::testing();
    let mut header = DataTransactionHeader::new(&config);
    header.version = 42; // wrong
    assert!(header.try_into_versioned().is_err());
}

#[test]
fn commitment_tx_try_into_versioned_returns_err_on_version_mismatch() {
    let config = ConsensusConfig::testing();
    let mut tx = CommitmentTransaction::new(&config);
    tx.version = 99; // wrong
    assert!(tx.try_into_versioned().is_err());
}
