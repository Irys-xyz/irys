use irys_actors::block_validation::{previous_solution_hash_is_valid, solution_hash_link_is_valid};
use irys_types::{IrysBlockHeader, H256};
use openssl::sha;

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
#[test_log::test(test)]
fn solution_hash_link_valid_ok() {
    let mut block = IrysBlockHeader::new_mock_header();
    // choose deterministic inputs
    block.poa.partition_chunk_offset = 7;
    block.vdf_limiter_info.output = H256::from([1u8; 32]);

    let poa_chunk: Vec<u8> = vec![0xAA, 0xBB, 0xCC, 0xDD];

    // compute expected solution_hash = sha256(poa_chunk || offset_le || seed)
    let mut hasher = sha::Sha256::new();
    hasher.update(&poa_chunk);
    hasher.update(&block.poa.partition_chunk_offset.to_le_bytes());
    hasher.update(block.vdf_limiter_info.output.as_bytes());
    let expected = H256::from(hasher.finish());

    block.solution_hash = expected;

    assert!(solution_hash_link_is_valid(&block, &poa_chunk).is_ok());
}

#[test_log::test(test)]
fn solution_hash_link_invalid_when_inputs_tampered() {
    let mut block = IrysBlockHeader::new_mock_header();
    block.poa.partition_chunk_offset = 7;
    block.vdf_limiter_info.output = H256::from([1u8; 32]);

    let poa_chunk: Vec<u8> = vec![0xAA, 0xBB, 0xCC, 0xDD];

    // set correct solution hash first
    let mut hasher = sha::Sha256::new();
    hasher.update(&poa_chunk);
    hasher.update(&block.poa.partition_chunk_offset.to_le_bytes());
    hasher.update(block.vdf_limiter_info.output.as_bytes());
    let expected = H256::from(hasher.finish());
    block.solution_hash = expected;

    // now tamper the inputs (e.g., change poa_chunk by one byte) to trigger mismatch
    let mut tampered_chunk = poa_chunk.clone();
    tampered_chunk[0] ^= 0x01;

    assert!(solution_hash_link_is_valid(&block, &tampered_chunk).is_err());
}
