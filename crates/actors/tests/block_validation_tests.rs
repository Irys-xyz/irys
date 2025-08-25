use irys_actors::block_validation::{
    poa_is_valid, previous_solution_hash_is_valid, solution_hash_link_is_valid, PreValidationError,
};
use irys_domain::{BlockIndex, BlockIndexReadGuard, EpochSnapshot};
use irys_types::{
    compute_solution_hash, partition::PartitionAssignment, Address, Base64, BlockIndexItem,
    ConsensusConfig, DataLedger, IrysBlockHeader, LedgerIndexItem, PoaData, H256,
};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

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
fn poa_chunk_offset_out_of_bounds_returns_error() {
    let config = ConsensusConfig::testing();

    let block_index_items = vec![
        BlockIndexItem {
            block_hash: H256::zero(),
            num_ledgers: 2,
            ledgers: vec![
                LedgerIndexItem {
                    max_chunk_offset: 0,
                    tx_root: H256::zero(),
                },
                LedgerIndexItem {
                    max_chunk_offset: 0,
                    tx_root: H256::zero(),
                },
            ],
        },
        BlockIndexItem {
            block_hash: H256::zero(),
            num_ledgers: 2,
            ledgers: vec![
                LedgerIndexItem {
                    max_chunk_offset: 10,
                    tx_root: H256::zero(),
                },
                LedgerIndexItem {
                    max_chunk_offset: 10,
                    tx_root: H256::zero(),
                },
            ],
        },
        BlockIndexItem {
            block_hash: H256::zero(),
            num_ledgers: 2,
            ledgers: vec![
                LedgerIndexItem {
                    max_chunk_offset: 20,
                    tx_root: H256::zero(),
                },
                LedgerIndexItem {
                    max_chunk_offset: 20,
                    tx_root: H256::zero(),
                },
            ],
        },
    ];

    let block_index = BlockIndex {
        items: block_index_items.into(),
        block_index_file: PathBuf::new(),
    };
    let block_index_guard = BlockIndexReadGuard::new(Arc::new(RwLock::new(block_index)));

    let mut epoch_snapshot = EpochSnapshot::default();
    let partition_hash = H256::zero();
    epoch_snapshot.partition_assignments.data_partitions.insert(
        partition_hash,
        PartitionAssignment {
            partition_hash,
            miner_address: Address::ZERO,
            ledger_id: Some(DataLedger::Publish as u32),
            slot_index: Some(0),
        },
    );

    let poa = PoaData {
        partition_chunk_offset: 10,
        partition_hash,
        chunk: Some(Base64(vec![0; config.chunk_size as usize])),
        ledger_id: Some(DataLedger::Publish as u32),
        tx_path: Some(Base64(vec![])),
        data_path: Some(Base64(vec![])),
    };

    let res = poa_is_valid(
        &poa,
        &block_index_guard,
        &epoch_snapshot,
        &config,
        &Address::ZERO,
    );

    assert!(matches!(
        res,
        Err(PreValidationError::MerkleProofInvalid(_))
    ));
}

#[test_log::test(test)]
fn solution_hash_link_valid_ok() {
    let mut block = IrysBlockHeader::new_mock_header();
    // choose deterministic inputs
    block.poa.partition_chunk_offset = 7;
    block.vdf_limiter_info.output = H256::from([1_u8; 32]);

    let poa_chunk: Vec<u8> = vec![0xAA, 0xBB, 0xCC, 0xDD];

    // compute expected solution_hash = sha256(poa_chunk || offset_le || seed)
    block.solution_hash = compute_solution_hash(
        &poa_chunk,
        block.poa.partition_chunk_offset,
        &block.vdf_limiter_info.output,
    );

    assert!(solution_hash_link_is_valid(&block, &poa_chunk).is_ok());
}

#[test_log::test(test)]
fn solution_hash_link_invalid_when_inputs_tampered() {
    let mut block = IrysBlockHeader::new_mock_header();
    block.poa.partition_chunk_offset = 7;
    block.vdf_limiter_info.output = H256::from([1_u8; 32]);

    let poa_chunk: Vec<u8> = vec![0xAA, 0xBB, 0xCC, 0xDD];

    // set correct solution hash first
    block.solution_hash = compute_solution_hash(
        &poa_chunk,
        block.poa.partition_chunk_offset,
        &block.vdf_limiter_info.output,
    );

    // now tamper the inputs (e.g., change poa_chunk by one byte) to trigger mismatch
    let mut tampered_chunk = poa_chunk;
    tampered_chunk[0] ^= 0x01;

    assert!(solution_hash_link_is_valid(&block, &tampered_chunk).is_err());
}
