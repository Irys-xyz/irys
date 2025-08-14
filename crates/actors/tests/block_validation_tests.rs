use irys_actors::block_validation::{
    poa_is_valid, previous_solution_hash_is_valid, PreValidationError,
};
use irys_domain::{BlockIndex, BlockIndexReadGuard, EpochSnapshot};
use irys_types::{
    partition::PartitionAssignment, Address, Base64, BlockIndexItem, ConsensusConfig, DataLedger,
    IrysBlockHeader, LedgerIndexItem, PoaData, H256,
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
