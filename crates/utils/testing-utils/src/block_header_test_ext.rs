use irys_types::{
    ConsensusConfig, H256, H256List, IrysBlockHeader, SystemLedger, SystemTransactionLedger,
};

/// Extension trait for test-only signing helpers on `IrysBlockHeader`.
pub trait IrysBlockHeaderTestExt {
    /// Signs this header with a random test signer, setting `miner_address`,
    /// `signature`, and `block_hash`. Always re-signs, even if already signed.
    fn test_sign(&mut self);

    /// Signs this header only if the current signature is invalid.
    /// Useful when a header may or may not have been modified after signing.
    fn ensure_test_signed(&mut self);
}

impl IrysBlockHeaderTestExt for IrysBlockHeader {
    fn test_sign(&mut self) {
        let config = ConsensusConfig::testing();
        let signer = irys_types::irys::IrysSigner::random_signer(&config);
        signer
            .sign_block_header(self)
            .expect("test signing should never fail");
    }

    fn ensure_test_signed(&mut self) {
        if !self.is_signature_valid() {
            self.test_sign();
        }
    }
}

/// Creates a new mock header that is properly signed.
pub fn new_mock_signed_header() -> IrysBlockHeader {
    let mut header = IrysBlockHeader::new_mock_header();
    header.test_sign();
    header
}

/// Builds an unsigned mock header at `height` whose commitment ledger carries
/// `commitment_tx_ids` (empty is allowed). `block_hash` is randomized (mock
/// headers default to zero, which would collide) and `poa.chunk` is defaulted.
/// Callers that need a signed header layer `test_sign()` on top.
pub fn mock_header_with_commitments(height: u64, commitment_tx_ids: Vec<H256>) -> IrysBlockHeader {
    let mut header = IrysBlockHeader::new_mock_header();
    header.height = height;
    header.block_hash = H256::random();
    header.poa.chunk = Some(Default::default());
    header.system_ledgers = vec![SystemTransactionLedger {
        ledger_id: SystemLedger::Commitment as u32,
        tx_ids: H256List(commitment_tx_ids),
    }];
    header
}
