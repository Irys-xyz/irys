use irys_types::irys::IrysSigner;
use irys_types::versioning::Signable as _;
use irys_types::{
    ConsensusConfig, DataTransaction, VersionedCommitmentTransaction,
    VersionedDataTransactionHeader,
};

#[test]
fn data_tx_signing_uses_discriminant() {
    let config = ConsensusConfig::testing();
    let signer = IrysSigner::random_signer(&config);
    let header = VersionedDataTransactionHeader::new(&config);

    // Test that the preimage used for signing starts with the version discriminant
    let preimage = {
        let mut buf = Vec::new();
        header.encode_for_signing(&mut buf);
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
    let sig_hash = signed_tx.signature_hash();
    assert!(signed_tx
        .header
        .signature
        .validate_signature(sig_hash, signer.address()));
}

#[test]
fn commitment_tx_signing_uses_discriminant() {
    let config = ConsensusConfig::testing();
    let signer = IrysSigner::random_signer(&config);
    let mut tx = VersionedCommitmentTransaction::new_stake(&config, irys_types::H256::zero());

    // Test that the preimage used for signing starts with the version discriminant
    let preimage = {
        let mut buf = Vec::new();
        tx.encode_for_signing(&mut buf);
        buf
    };
    assert_eq!(preimage[0], 1);

    // Actually test signing works with the versioned structure
    signer
        .sign_commitment(&mut tx)
        .expect("signing should succeed");

    // Verify the signature is valid
    let sig_hash = tx.signature_hash();
    assert!(tx.signature.validate_signature(sig_hash, signer.address()));
}
