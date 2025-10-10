use irys_types::{CommitmentTransaction, ConsensusConfig, DataTransactionHeader, Signable as _};

#[test]
fn data_tx_preimage_starts_with_discriminant() {
    let config = ConsensusConfig::testing();
    let tx = DataTransactionHeader::new(&config);
    let mut buf = Vec::new();
    tx.encode_for_signing(&mut buf);
    assert_eq!(buf.first().copied(), Some(1));
}

#[test]
fn commitment_tx_preimage_starts_with_discriminant() {
    let config = ConsensusConfig::testing();
    let tx = CommitmentTransaction::new_stake(&config, irys_types::H256::zero());
    let mut buf = Vec::new();
    tx.encode_for_signing(&mut buf);
    assert_eq!(buf.first().copied(), Some(1));
}
