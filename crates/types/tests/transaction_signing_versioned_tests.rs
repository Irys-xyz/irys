use irys_types::irys::IrysSigner;
use irys_types::{
    CommitmentTransaction, ConsensusConfig, DataTransaction, DataTransactionHeader, Signable as _,
    VersionedCommitmentTransaction, VersionedDataTransactionHeader,
};

#[test]
fn data_tx_signing_uses_discriminant() {
    let config = ConsensusConfig::testing();
    let signer = IrysSigner::random_signer(&config);
    let header = DataTransactionHeader::new(&config);

    // Test that the preimage used for signing starts with the version discriminant
    let preimage = {
        let v = VersionedDataTransactionHeader::V1(header.clone());
        let mut buf = Vec::new();
        v.encode_for_signing(&mut buf);
        buf
    };
    assert_eq!(preimage[0], 1);

    // Actually test signing works with the versioned structure
    let tx = DataTransaction {
        header,
        ..Default::default()
    };
    let signed_tx = signer.sign_transaction(tx).expect("signing should succeed");

    // Verify the signature is valid
    let sig_hash = signed_tx.signature_hash().expect("hash should succeed");
    assert!(signed_tx
        .header
        .signature
        .validate_signature(sig_hash, signer.address()));
}

#[test]
fn commitment_tx_signing_uses_discriminant() {
    let config = ConsensusConfig::testing();
    let signer = IrysSigner::random_signer(&config);
    let tx = CommitmentTransaction::new_stake(&config, irys_types::H256::zero());

    // Test that the preimage used for signing starts with the version discriminant
    let preimage = {
        let v = VersionedCommitmentTransaction::V1(tx.clone());
        let mut buf = Vec::new();
        v.encode_for_signing(&mut buf);
        buf
    };
    assert_eq!(preimage[0], 1);

    // Actually test signing works with the versioned structure
    let signed_tx = signer.sign_commitment(tx).expect("signing should succeed");

    // Verify the signature is valid
    let sig_hash = signed_tx.signature_hash().expect("hash should succeed");
    assert!(signed_tx
        .signature
        .validate_signature(sig_hash, signer.address()));
}
