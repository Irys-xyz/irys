use irys_actors::block_validation::{
    PreValidationError, poa_is_valid, poa_ledger_matches_assignment,
    poa_partition_is_owned_by_miner, previous_solution_hash_is_valid, solution_hash_link_is_valid,
};
use irys_database::{IrysDatabaseArgs as _, open_or_create_db};
use irys_domain::{BlockIndex, BlockIndexReadGuard, BlockTree, BlockTreeReadGuard, EpochSnapshot};
use irys_packing::capacity_single::compute_entropy_chunk;
use irys_testing_utils::new_mock_signed_header;
use irys_types::{
    Base64, BlockIndexItem, ConsensusConfig, DataLedger, DatabaseProvider, H256, IrysAddress,
    IrysBlockHeader, LedgerIndexItem, PoaData, compute_solution_hash,
    partition::PartitionAssignment,
};
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

    let block_index_items = [
        BlockIndexItem {
            block_hash: H256::zero(),
            num_ledgers: 2,
            ledgers: vec![
                LedgerIndexItem {
                    total_chunks: 0,
                    tx_root: H256::zero(),
                    ledger: DataLedger::Publish,
                },
                LedgerIndexItem {
                    total_chunks: 0,
                    tx_root: H256::zero(),
                    ledger: DataLedger::Submit,
                },
            ],
        },
        BlockIndexItem {
            block_hash: H256::zero(),
            num_ledgers: 2,
            ledgers: vec![
                LedgerIndexItem {
                    total_chunks: 10,
                    tx_root: H256::zero(),
                    ledger: DataLedger::Publish,
                },
                LedgerIndexItem {
                    total_chunks: 10,
                    tx_root: H256::zero(),
                    ledger: DataLedger::Submit,
                },
            ],
        },
        BlockIndexItem {
            block_hash: H256::zero(),
            num_ledgers: 2,
            ledgers: vec![
                LedgerIndexItem {
                    total_chunks: 20,
                    tx_root: H256::zero(),
                    ledger: DataLedger::Publish,
                },
                LedgerIndexItem {
                    total_chunks: 20,
                    tx_root: H256::zero(),
                    ledger: DataLedger::Submit,
                },
            ],
        },
    ];

    let _tmp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
    let db_env = open_or_create_db(
        _tmp_dir.path(),
        irys_database::tables::IrysTables::ALL,
        reth_db::mdbx::DatabaseArguments::irys_testing().unwrap(),
    )
    .unwrap();
    let db = DatabaseProvider(Arc::new(db_env));
    let block_index = BlockIndex::new_for_testing(db.clone());
    for (height, item) in block_index_items.iter().enumerate() {
        block_index.push_item(item, height as u64).unwrap();
    }
    let block_index_guard = BlockIndexReadGuard::new(block_index);

    let mut epoch_snapshot = EpochSnapshot::default();
    let partition_hash = H256::zero();
    epoch_snapshot.partition_assignments.data_partitions.insert(
        partition_hash,
        PartitionAssignment {
            partition_hash,
            miner_address: IrysAddress::ZERO,
            ledger_id: Some(DataLedger::Publish as u32),
            slot_index: Some(0),
        },
    );

    let mut poa = PoaData {
        partition_chunk_offset: 5,
        partition_hash,
        chunk: Some(Base64(vec![0; config.chunk_size as usize])),
        ledger_id: Some(DataLedger::Publish as u32),
        tx_path: Some(Base64(vec![])),
        data_path: Some(Base64(vec![])),
    };

    // The test indexes three blocks at heights 0/1/2 with Publish
    // total_chunks of [0, 10, 20]. With offset=5 (inside the 10-chunk
    // testing partition), anchoring on the top block (height=2) keeps the
    // ledger offset in range (5 < 20) so the pre-checks pass and the
    // merkle proof is the failure source.
    //
    // The fast-path block_index lookup is exercised here; block_tree is
    // populated with a signed mock genesis solely to satisfy the
    // `poa_is_valid` signature (the fallback path is unreachable when
    // parent_height is in the index). `BlockTree::new` validates the
    // genesis signature, hence the signed mock header.
    let dummy_genesis = new_mock_signed_header();
    let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(BlockTree::new(
        &dummy_genesis,
        config.clone(),
    ))));
    let res = poa_is_valid(
        &poa,
        &block_index_guard,
        &block_tree_guard,
        &db,
        H256::zero(),
        2,
        &epoch_snapshot,
        &config,
        &IrysAddress::ZERO,
    );

    assert!(matches!(
        res,
        Err(PreValidationError::MerkleProofInvalid(_))
    ));

    // An offset at or past the active partition size is rejected before any
    // ledger or merkle work.
    poa.partition_chunk_offset = config.num_chunks_in_partition as u32;
    let res = poa_is_valid(
        &poa,
        &block_index_guard,
        &block_tree_guard,
        &db,
        H256::zero(),
        2,
        &epoch_snapshot,
        &config,
        &IrysAddress::ZERO,
    );
    assert!(matches!(
        res,
        Err(PreValidationError::PoAChunkOffsetOutOfBlockBounds)
    ));
}

#[test_log::test(test)]
fn poa_chunk_longer_than_chunk_size_is_too_long() {
    let config = ConsensusConfig::testing();
    let chunk_size = usize::try_from(config.chunk_size).unwrap();
    let _tmp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
    let db_env = open_or_create_db(
        _tmp_dir.path(),
        irys_database::tables::IrysTables::ALL,
        reth_db::mdbx::DatabaseArguments::irys_testing().unwrap(),
    )
    .unwrap();
    let db = DatabaseProvider(Arc::new(db_env));
    let block_index_guard = BlockIndexReadGuard::new(BlockIndex::new_for_testing(db.clone()));
    let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(BlockTree::new(
        &new_mock_signed_header(),
        config.clone(),
    ))));
    let poa = PoaData {
        partition_chunk_offset: 0,
        partition_hash: H256::zero(),
        chunk: Some(Base64(vec![0; chunk_size + 1])),
        ledger_id: Some(DataLedger::Publish.into()),
        tx_path: Some(Base64(vec![])),
        data_path: Some(Base64(vec![])),
    };

    let res = poa_is_valid(
        &poa,
        &block_index_guard,
        &block_tree_guard,
        &db,
        H256::zero(),
        0,
        &EpochSnapshot::default(),
        &config,
        &IrysAddress::ZERO,
    );

    assert!(
        matches!(
            res,
            Err(PreValidationError::PoAChunkTooLong {
                got,
                limit,
                ..
            }) if got == chunk_size + 1 && limit == chunk_size
        ),
        "oversized chunk must be rejected before Merkle validation, got {res:?}"
    );
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

fn capacity_assignment(partition_hash: H256, miner_address: IrysAddress) -> PartitionAssignment {
    PartitionAssignment {
        partition_hash,
        miner_address,
        ledger_id: None,
        slot_index: None,
    }
}

fn data_assignment(
    partition_hash: H256,
    miner_address: IrysAddress,
    ledger: DataLedger,
) -> PartitionAssignment {
    PartitionAssignment {
        partition_hash,
        miner_address,
        ledger_id: Some(ledger as u32),
        slot_index: Some(0),
    }
}

/// Snapshot with one capacity and one Submit data partition owned by `owner`.
fn ownership_snapshot(owner: IrysAddress) -> (EpochSnapshot, H256, H256) {
    let capacity_hash = H256::random();
    let data_hash = H256::random();
    let mut epoch_snapshot = EpochSnapshot::default();
    let assignments = &mut epoch_snapshot.partition_assignments;
    assignments
        .capacity_partitions
        .insert(capacity_hash, capacity_assignment(capacity_hash, owner));
    assignments.data_partitions.insert(
        data_hash,
        data_assignment(data_hash, owner, DataLedger::Submit),
    );
    (epoch_snapshot, capacity_hash, data_hash)
}

fn entropy_poa(partition_hash: H256, ledger_id: Option<u32>) -> PoaData {
    PoaData {
        partition_chunk_offset: 0,
        partition_hash,
        chunk: Some(Base64(vec![0; 32])),
        ledger_id,
        tx_path: None,
        data_path: None,
    }
}

fn data_poa(partition_hash: H256, ledger: DataLedger) -> PoaData {
    PoaData {
        partition_chunk_offset: 0,
        partition_hash,
        chunk: Some(Base64(vec![0; 32])),
        ledger_id: Some(ledger as u32),
        tx_path: Some(Base64(vec![])),
        data_path: Some(Base64(vec![])),
    }
}

#[derive(Debug, Clone, Copy)]
enum Partition {
    Capacity,
    Data,
}

#[rstest::rstest]
#[case::capacity(Partition::Capacity)]
#[case::data(Partition::Data)]
fn poa_partition_owned_by_miner_is_accepted(#[case] partition: Partition) {
    let miner = IrysAddress::random();
    let (epoch_snapshot, capacity_hash, data_hash) = ownership_snapshot(miner);
    // Honest miners also submit entropy-chunk PoAs (no data_path) from data partitions.
    let partition_hash = match partition {
        Partition::Capacity => capacity_hash,
        Partition::Data => data_hash,
    };

    assert!(poa_partition_is_owned_by_miner(partition_hash, miner, &epoch_snapshot).is_ok());
}

#[rstest::rstest]
#[case::capacity(Partition::Capacity)]
#[case::data(Partition::Data)]
fn poa_partition_owned_by_other_miner_is_rejected(#[case] partition: Partition) {
    let owner = IrysAddress::random();
    let miner = IrysAddress::random();
    let (epoch_snapshot, capacity_hash, data_hash) = ownership_snapshot(owner);
    let partition_hash = match partition {
        Partition::Capacity => capacity_hash,
        Partition::Data => data_hash,
    };

    let res = poa_partition_is_owned_by_miner(partition_hash, miner, &epoch_snapshot);

    assert!(matches!(
        res,
        Err(PreValidationError::PoAPartitionNotOwned { partition_hash: p, miner: m, owner: Some(o) })
            if p == partition_hash && m == miner && o == owner
    ));
}

#[test_log::test(test)]
fn poa_partition_absent_from_snapshot_is_rejected() {
    let miner = IrysAddress::random();
    let (epoch_snapshot, _, _) = ownership_snapshot(miner);
    let unassigned = H256::random();

    let res = poa_partition_is_owned_by_miner(unassigned, miner, &epoch_snapshot);

    assert!(matches!(
        res,
        Err(PreValidationError::PoAPartitionNotOwned { partition_hash, owner: None, .. })
            if partition_hash == unassigned
    ));
}

fn empty_poa_guards(
    config: &ConsensusConfig,
    db_dir: &std::path::Path,
) -> (DatabaseProvider, BlockIndexReadGuard, BlockTreeReadGuard) {
    let db_env = open_or_create_db(
        db_dir,
        irys_database::tables::IrysTables::ALL,
        reth_db::mdbx::DatabaseArguments::irys_testing().unwrap(),
    )
    .unwrap();
    let db = DatabaseProvider(Arc::new(db_env));
    let block_index_guard = BlockIndexReadGuard::new(BlockIndex::new_for_testing(db.clone()));
    let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(BlockTree::new(
        &new_mock_signed_header(),
        config.clone(),
    ))));
    (db, block_index_guard, block_tree_guard)
}

#[derive(Debug, Clone, Copy)]
enum UnownedPartition {
    OwnedByOther,
    ZeroHashUnassigned,
}

/// `poa_is_valid` enforces ownership on its own, not only via prevalidation.
/// The error names the partition, the block miner, and the assigned owner
/// (`None` when the snapshot has no assignment).
#[test_log::test(rstest::rstest)]
#[case::owned_by_other(UnownedPartition::OwnedByOther)]
#[case::zero_hash_unassigned(UnownedPartition::ZeroHashUnassigned)]
fn poa_is_valid_rejects_unowned_partition(#[case] kind: UnownedPartition) {
    let config = ConsensusConfig::testing();
    let tmp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
    let (db, block_index_guard, block_tree_guard) = empty_poa_guards(&config, tmp_dir.path());
    let miner = IrysAddress::random();
    let (poa, epoch_snapshot, owner) = match kind {
        UnownedPartition::OwnedByOther => {
            let owner = IrysAddress::random();
            let (epoch_snapshot, capacity_hash, _) = ownership_snapshot(owner);
            (
                entropy_poa(capacity_hash, None),
                epoch_snapshot,
                Some(owner),
            )
        }
        UnownedPartition::ZeroHashUnassigned => (
            entropy_poa(H256::zero(), None),
            EpochSnapshot::default(),
            None,
        ),
    };
    let partition_hash = poa.partition_hash;

    let res = poa_is_valid(
        &poa,
        &block_index_guard,
        &block_tree_guard,
        &db,
        H256::zero(),
        0,
        &epoch_snapshot,
        &config,
        &miner,
    );

    assert!(
        matches!(
            &res,
            Err(PreValidationError::PoAPartitionNotOwned {
                partition_hash: got_hash,
                miner: got_miner,
                owner: got_owner,
            }) if *got_hash == partition_hash && *got_miner == miner && *got_owner == owner
        ),
        "{res:?}"
    );
}

/// An owned capacity partition with a real entropy chunk passes `poa_is_valid`.
#[test_log::test(test)]
fn poa_is_valid_accepts_owned_entropy_poa() {
    let mut config = ConsensusConfig::testing();
    config.chunk_size = 32;
    config.entropy_packing_iterations = 1_000;
    let tmp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
    let (db, block_index_guard, block_tree_guard) = empty_poa_guards(&config, tmp_dir.path());
    let miner = IrysAddress::random();
    let (epoch_snapshot, capacity_hash, _) = ownership_snapshot(miner);
    let mut chunk = Vec::new();
    compute_entropy_chunk(
        miner,
        0,
        capacity_hash.into(),
        config.entropy_packing_iterations,
        config.chunk_size as usize,
        &mut chunk,
        config.chain_id,
    );
    let poa = PoaData {
        partition_chunk_offset: 0,
        partition_hash: capacity_hash,
        chunk: Some(Base64(chunk)),
        ledger_id: None,
        tx_path: None,
        data_path: None,
    };

    let res = poa_is_valid(
        &poa,
        &block_index_guard,
        &block_tree_guard,
        &db,
        H256::zero(),
        0,
        &epoch_snapshot,
        &config,
        &miner,
    );

    assert!(matches!(&res, Ok(())), "{res:?}");
}

/// A data PoA must name the ledger its partition is assigned to; the
/// assignment's slot index means nothing in any other ledger.
#[test_log::test(test)]
fn data_poa_ledger_must_match_partition_assignment() {
    let config = ConsensusConfig::testing();
    let tmp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
    let (db, block_index_guard, block_tree_guard) = empty_poa_guards(&config, tmp_dir.path());
    let miner = IrysAddress::random();
    let (epoch_snapshot, _, data_hash) = ownership_snapshot(miner);
    let poa = PoaData {
        partition_chunk_offset: 0,
        partition_hash: data_hash,
        chunk: Some(Base64(vec![0; 32])),
        ledger_id: Some(DataLedger::Publish as u32),
        tx_path: Some(Base64(vec![])),
        data_path: Some(Base64(vec![])),
    };

    let res = poa_is_valid(
        &poa,
        &block_index_guard,
        &block_tree_guard,
        &db,
        H256::zero(),
        0,
        &epoch_snapshot,
        &config,
        &miner,
    );

    assert!(matches!(
        res,
        Err(PreValidationError::PoALedgerMismatch { partition_hash, poa_ledger_id, assigned_ledger_id })
            if partition_hash == data_hash
                && poa_ledger_id == DataLedger::Publish as u32
                && assigned_ledger_id == Some(DataLedger::Submit as u32)
    ));
}

#[derive(Debug, Clone, Copy)]
enum LedgerAssignment {
    PublishOnSubmit,
    DataOnCapacity,
    EntropyStrayLedger,
    AssignedLedger,
}

/// `poa_ledger_matches_assignment` accepts a data PoA only when `ledger_id` is
/// the ledger the partition is assigned to. An entropy PoA is not bound to a
/// ledger, even when it carries a stray `ledger_id`.
#[test_log::test(rstest::rstest)]
#[case::publish_on_submit(LedgerAssignment::PublishOnSubmit)]
#[case::data_on_capacity(LedgerAssignment::DataOnCapacity)]
#[case::entropy_stray_ledger(LedgerAssignment::EntropyStrayLedger)]
#[case::assigned_ledger(LedgerAssignment::AssignedLedger)]
fn poa_ledger_matches_assignment_cases(#[case] kind: LedgerAssignment) {
    let miner = IrysAddress::random();
    let (epoch_snapshot, capacity_hash, data_hash) = ownership_snapshot(miner);
    let (poa, expected): (PoaData, Result<(), Option<u32>>) = match kind {
        LedgerAssignment::PublishOnSubmit => (
            data_poa(data_hash, DataLedger::Publish),
            Err(Some(DataLedger::Submit as u32)),
        ),
        LedgerAssignment::DataOnCapacity => {
            (data_poa(capacity_hash, DataLedger::Publish), Err(None))
        }
        LedgerAssignment::EntropyStrayLedger => (
            entropy_poa(capacity_hash, Some(DataLedger::Publish as u32)),
            Ok(()),
        ),
        LedgerAssignment::AssignedLedger => (data_poa(data_hash, DataLedger::Submit), Ok(())),
    };

    let res = poa_ledger_matches_assignment(&poa, &epoch_snapshot);
    match expected {
        Ok(()) => assert!(matches!(&res, Ok(())), "{res:?}"),
        Err(assigned) => assert!(
            matches!(
                &res,
                Err(PreValidationError::PoALedgerMismatch {
                    partition_hash,
                    poa_ledger_id,
                    assigned_ledger_id,
                }) if *partition_hash == poa.partition_hash
                    && *poa_ledger_id == DataLedger::Publish as u32
                    && *assigned_ledger_id == assigned
            ),
            "{res:?}"
        ),
    }
}
