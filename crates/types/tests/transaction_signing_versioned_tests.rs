use irys_types::irys::IrysSigner;
use irys_types::versioning::Signable as _;
use irys_types::{
    CommitmentTransaction, ConsensusConfig, DataTransaction, DataTransactionHeader, IngressProof,
    IrysBlockHeader,
};

#[test]
fn data_tx_signing_uses_discriminant() {
    let config = ConsensusConfig::testing();
    let signer = IrysSigner::random_signer(&config);
    let header = DataTransactionHeader::new(&config);

    // Test that the preimage used for signing starts with the version discriminant
    let mut buf = Vec::new();
    header.encode_for_signing(&mut buf);
    // needs to be a slice so we can advance past the header
    let buf = &mut &buf[..];
    // decode the header
    let _hdr = alloy_rlp::Header::decode(buf).unwrap();
    // first byte after the header should be the version discriminant
    assert_eq!(buf[0], 1);

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
    let mut tx = CommitmentTransaction::new_stake(&config, irys_types::H256::zero());

    // Test that the preimage used for signing starts with the version discriminant
    let mut buf = Vec::new();
    tx.encode_for_signing(&mut buf);
    // needs to be a slice so we can advance past the header
    let buf = &mut &buf[..];
    // decode the header
    let _hdr = alloy_rlp::Header::decode(buf).unwrap();
    // first byte should be the version
    assert_eq!(buf[0], 2);

    // Actually test signing works with the versioned structure
    signer
        .sign_commitment(&mut tx)
        .expect("signing should succeed");

    // Verify the signature is valid
    let sig_hash = tx.signature_hash();
    assert!(tx
        .signature()
        .validate_signature(sig_hash, signer.address()));
}

#[test]
fn block_header_signing_uses_discriminant() {
    let config = ConsensusConfig::testing();
    let signer = IrysSigner::random_signer(&config);
    let mut block = IrysBlockHeader::new_mock_header();

    // Test that the preimage used for signing starts with the version discriminant
    let mut buf = Vec::new();
    block.encode_for_signing(&mut buf);
    // needs to be a slice so we can advance past the header
    let buf = &mut &buf[..];
    // decode the header
    let _hdr = alloy_rlp::Header::decode(buf).unwrap();
    assert_eq!(buf[0], 1);

    // Actually test signing works with the versioned structure
    signer
        .sign_block_header(&mut block)
        .expect("signing should succeed");

    // Verify the signature is valid
    let sig_hash = block.signature_hash();
    assert!(block
        .signature
        .validate_signature(sig_hash, signer.address()));
}

#[test]
fn ingress_proof_signing_uses_discriminant() {
    let config = ConsensusConfig::testing();
    let signer = IrysSigner::random_signer(&config);
    let mut proof = IngressProof::default();

    // Test that the preimage used for signing starts with the version discriminant
    let mut buf = Vec::new();
    proof.encode_for_signing(&mut buf);
    // needs to be a slice so we can advance past the header
    let buf = &mut &buf[..];
    // decode the header
    let _hdr = alloy_rlp::Header::decode(buf).unwrap();
    assert_eq!(buf[0], 1);

    // Actually test signing works with the versioned structure
    signer
        .sign_ingress_proof(&mut proof)
        .expect("signing should succeed");

    // Verify the signature is valid
    let sig_hash = proof.signature_hash();
    assert!(proof
        .signature
        .validate_signature(sig_hash, signer.address()));
}
