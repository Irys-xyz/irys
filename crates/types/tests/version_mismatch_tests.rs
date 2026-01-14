use irys_types::{
    CommitmentTransaction, ConsensusConfig, DataTransactionHeader, VersionDiscriminant as _,
};

#[test]
fn data_tx_version_is_1() {
    let config = ConsensusConfig::testing();
    let header = DataTransactionHeader::new(&config);
    assert_eq!(header.version(), 1, "new() should create V1 with version=1");
}

#[test]
fn commitment_tx_version_is_2() {
    let config = ConsensusConfig::testing();
    let tx = CommitmentTransaction::new_stake(&config, irys_types::H256::zero());
    assert_eq!(
        tx.version(),
        2,
        "new_stake() should create V2 with version=2"
    );
}
