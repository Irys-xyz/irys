use irys_types::{
    CommitmentTransaction, DataTransactionHeader, DataTransactionHeaderV1, Signable as _,
};

#[test]
fn data_tx_preimage_starts_with_discriminant() {
    let tx = DataTransactionHeaderV1 {
        version: 1,
        ..Default::default()
    };
    let versioned = DataTransactionHeader::V1(tx);
    let mut buf = Vec::new();
    versioned.encode_for_signing(&mut buf);
    assert_eq!(buf.first().copied(), Some(1));
}

#[test]
fn commitment_tx_preimage_starts_with_discriminant() {
    use irys_types::ConsensusConfig;
    let config = ConsensusConfig::testing();
    let tx = CommitmentTransaction::new_stake(&config, irys_types::H256::zero());
    let mut buf = Vec::new();
    tx.encode_for_signing(&mut buf);
    assert_eq!(buf.first().copied(), Some(1));
}
