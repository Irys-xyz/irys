use irys_actors::block_validation::previous_solution_hash_is_valid;
use irys_types::{IrysBlockHeader, H256};

#[test_log::test(test)]
/// test that a parent blocks solution_hash must equal the current blocks previous_solution_hash
fn invalid_previous_solution_hash_rejected() {
    let mut parent = IrysBlockHeader::new_mock_header();
    parent.solution_hash = H256::zero();

    let mut block = IrysBlockHeader::new_mock_header();
    block.previous_solution_hash = {
        let mut bytes = H256::zero().to_fixed_bytes();
        bytes[1] ^= 0x01; // flip second bit so it will not match in the later test
        H256::from(bytes)
    };

    assert_ne!(block.previous_solution_hash, parent.solution_hash);
    assert!(previous_solution_hash_is_valid(&block, &parent).is_err());
}
