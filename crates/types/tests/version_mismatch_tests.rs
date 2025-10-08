use irys_types::{VersionedCommitmentTransaction, ConsensusConfig, VersionedDataTransactionHeader};

#[test]
fn data_tx_version_is_1() {
    let config = ConsensusConfig::testing();
    let header = VersionedDataTransactionHeader::new(&config);
    assert_eq!(header.version, 1, "new() should create V1 with version=1");
}

#[test]
fn commitment_tx_version_is_1() {
    let config = ConsensusConfig::testing();
    let tx = VersionedCommitmentTransaction::new_stake(&config, irys_types::H256::zero());
    assert_eq!(tx.version, 1, "new_stake() should create V1 with version=1");
}
