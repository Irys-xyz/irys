#![cfg(debug_assertions)]
use irys_types::{DataTransactionHeader, CommitmentTransaction, ConsensusConfig};

#[test]
#[should_panic]
fn data_tx_into_versioned_panics_on_version_mismatch() {
    let config = ConsensusConfig::testing();
    let mut header = DataTransactionHeader::new(&config);
    header.version = 42; // wrong
    let _ = header.into_versioned();
}

#[test]
#[should_panic]
fn commitment_tx_into_versioned_panics_on_version_mismatch() {
    let config = ConsensusConfig::testing();
    let mut tx = CommitmentTransaction::new(&config);
    tx.version = 99; // wrong
    let _ = tx.into_versioned();
}
