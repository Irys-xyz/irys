use irys_types::irys::IrysSigner;
use irys_types::{
    CommitmentTransaction, ConsensusConfig, DataTransactionHeader, Signable,
    VersionedCommitmentTransaction, VersionedDataTransactionHeader,
};

#[test]
fn data_tx_signing_uses_discriminant() {
    let config = ConsensusConfig::testing();
    let signer = IrysSigner::random_signer(&config);
    let header = DataTransactionHeader::new(&config);
    let preimage = {
        let v = VersionedDataTransactionHeader::V1(header.clone());
        let mut buf = Vec::new();
        v.encode_for_signing(&mut buf);
        buf
    };
    assert_eq!(preimage[0], 1);
}

#[test]
fn commitment_tx_signing_uses_discriminant() {
    let config = ConsensusConfig::testing();
    let tx = CommitmentTransaction::new_stake(&config, irys_types::H256::zero());
    let preimage = {
        let v = VersionedCommitmentTransaction::V1(tx.clone());
        let mut buf = Vec::new();
        v.encode_for_signing(&mut buf);
        buf
    };
    assert_eq!(preimage[0], 1);
}
