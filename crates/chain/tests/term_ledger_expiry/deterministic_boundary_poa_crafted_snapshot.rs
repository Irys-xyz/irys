use irys_actors::block_index_service::{BlockIndexService, BlockIndexServiceMessage};
use irys_actors::block_validation::{poa_is_valid, PreValidationError};
use irys_config::StorageSubmodulesConfig;
use irys_database::add_genesis_commitments;
use irys_domain::{BlockIndex, BlockIndexReadGuard, EpochSnapshot};
use irys_packing::{capacity_single::compute_entropy_chunk, xor_vec_u8_arrays_in_place};
use irys_testing_utils::tempfile::TempDir;
use irys_testing_utils::utils::temporary_directory;
use irys_types::{
    irys::IrysSigner, Address, Base64, DataLedger, DataTransactionHeader, DataTransactionLedger,
    H256List, IrysBlockHeader, NodeConfig, Signature, H256, U256,
};
use std::sync::{Arc, RwLock};
use tracing::info;

/// Deterministic crafted-boundary PoA test:
/// - Builds a synthetic block with a single Submit tx and a PoA chunk that is VALID when
///   using the PARENT epoch snapshot's slot assignment for the target partition.
/// - Mutates a CLONED epoch snapshot to simulate the CHILD snapshot with a different slot index
///   for the same partition so that PoA becomes INVALID (e.g., merkle proof mismatch or out-of-bounds).
///
/// This avoids any miner randomness and does not rely on the live validation path.
/// It directly exercises `poa_is_valid` with controlled parent/child snapshots. On the buggy
/// commit that used the child snapshot at the boundary, this PoA would be rejected; the fixed
/// commit (parent snapshot) would accept it.
///
/// Note: This test does not depend on the node's current epoch transition logic; it constructs
/// a minimal, self-contained scenario to validate the invariant deterministically.
#[test_log::test(actix_web::test)]
async fn deterministic_boundary_poa_crafted_snapshot() -> eyre::Result<()> {
    // 1) Minimal test config
    const CHUNK_SIZE: u64 = 32;

    // Make the test stable and light.
    // - small chunk size
    // - small partition size
    // - tiny epoch so boundary behavior is easy to reason about (not strictly used here)
    let mut node_config = NodeConfig::testing();
    node_config.consensus.get_mut().block_migration_depth = 1;
    node_config.consensus.get_mut().chunk_size = CHUNK_SIZE;
    node_config.consensus.get_mut().num_chunks_in_partition = 8;
    node_config.consensus.get_mut().num_partitions_per_slot = 1;
    node_config.consensus.get_mut().epoch.num_blocks_in_epoch = 3;

    // 2) Build a minimal genesis context with BlockIndex and EpochSnapshot
    let (
        tmp_dir,
        block_index,
        block_index_tx,
        parent_epoch_snapshot,
        miner_address,
        consensus,
        genesis_hash,
        _block_index_handle,
    ) = init_min_context(&node_config).await;

    // Choose a partition_hash deterministically: first submit ledger slot's first partition
    let partition_hash = parent_epoch_snapshot
        .ledgers
        .get_slots(DataLedger::Submit)
        .first()
        .and_then(|slot| slot.partitions.first().copied())
        .expect("submit ledger slot 0 should exist with a partition");

    // 3) Craft a synthetic transaction and merkle proofs to populate a single Submit ledger
    //    using a 1-tx block. We'll place the tx at chunk offset 0 in the block bounds.
    let signer = IrysSigner::random_signer(&consensus);
    let data = vec![0xAB; CHUNK_SIZE as usize]; // single 32-byte chunk
    let tx = {
        let tx_unsigned = signer
            .create_transaction(data.clone(), H256::zero())
            .expect("create tx");
        signer.sign_transaction(tx_unsigned).expect("sign tx")
    };
    let tx_header: DataTransactionHeader = tx.header.clone();

    // Merklize a single transaction header to get tx_root and proofs
    let tx_headers = vec![tx_header.clone()];
    let (tx_root, tx_paths) = DataTransactionLedger::merklize_tx_root(&tx_headers);

    // Take the proof for the single tx (index 0)
    let tx_path = tx_paths[0].proof.clone();

    // The tx has one chunk of 32 bytes; build a fake "chunk proof" for that chunk
    // For deterministic test we reuse the tx's data proof if available; otherwise,
    // emulate a leaf proof of appropriate length for the single chunk.
    let data_path = if let Some(p) = tx.proofs.first() {
        p.proof.clone()
    } else {
        // Fallback: emulate a leaf proof structure for a single-chunk tx.
        // The exact encoding doesn't matter as long as validate_path() on the leaf works
        // consistently with our chunk size and left/right bounds. Here we simply copy
        // the tx_path bytes (since it's also a Merkle proof to a leaf) to demonstrate intent.
        tx_path.clone()
    };

    // PoA chunk and partition_chunk_offset will be computed after we migrate the prelude
    // once we can query actual block bounds from the BlockIndex.
    let mut partition_chunk_offset: u32 = 0;

    // 5) Build two synthetic blocks:
    //    - Prelude block at height=1 adds exactly `slot_start` chunks so that the next
    //      block's PoA chunk is at block-relative offset 0 (ensuring tx_path alignment).
    //    - Boundary-like block at height=2 contains our crafted PoA chunk targeting partition_chunk_offset=0.
    //
    // Compute ledger_chunk_offset using parent slot assignment
    let parent_slot_index = parent_epoch_snapshot
        .partition_assignments
        .get_assignment(partition_hash)
        .and_then(|pa| pa.slot_index)
        .unwrap_or(0) as u64;

    let ledger_chunk_offset =
        parent_slot_index * consensus.num_partitions_per_slot * consensus.num_chunks_in_partition
            + partition_chunk_offset as u64;

    // Prelude tx: size = ledger_chunk_offset * CHUNK_SIZE (0 if offset is 0)
    let prelude_size_bytes = (ledger_chunk_offset as usize) * (CHUNK_SIZE as usize);
    let prelude_tx_header = if prelude_size_bytes > 0 {
        let prelude_data = vec![0xCD; prelude_size_bytes];
        let pre_tx = signer
            .create_transaction(prelude_data, H256::zero())
            .expect("create prelude tx");
        let pre_tx = signer.sign_transaction(pre_tx).expect("sign prelude tx");
        Some(pre_tx.header)
    } else {
        None
    };

    // Merklize prelude tx headers (0 or 1)
    let prelude_headers: Vec<DataTransactionHeader> =
        prelude_tx_header.clone().into_iter().collect();
    let (prelude_tx_root, _prelude_paths) =
        DataTransactionLedger::merklize_tx_root(&prelude_headers);

    // Prelude block (height=1), previous is genesis
    let prelude_publish_ledger = DataTransactionLedger {
        ledger_id: DataLedger::Publish.into(),
        tx_root: H256::zero(),
        tx_ids: H256List(Vec::new()),
        total_chunks: 0,
        expires: None,
        proofs: None,
        required_proof_count: Some(1),
    };
    let prelude_submit_ledger = DataTransactionLedger {
        ledger_id: DataLedger::Submit.into(),
        tx_root: prelude_tx_root,
        tx_ids: H256List(prelude_tx_header.into_iter().map(|h| h.id).collect()),
        total_chunks: ledger_chunk_offset, // previous total becomes this
        expires: Some(41_000_000),
        proofs: None,
        required_proof_count: None,
    };
    let prelude_block_hash = H256::from_slice(&[1_u8; 32]);
    let mut prelude_block_v = IrysBlockHeader::default();
    {
        let inner = &mut *prelude_block_v;
        inner.height = 1;
        inner.reward_address = miner_address;
        inner.poa = irys_types::PoaData {
            tx_path: None,
            data_path: None,
            chunk: None,
            ledger_id: None,
            partition_chunk_offset: 0,
            partition_hash,
        };
        inner.block_hash = prelude_block_hash;
        inner.previous_block_hash = genesis_hash;
        inner.previous_cumulative_diff = U256::from(4000);
        inner.miner_address = miner_address;
        inner.signature = Signature::test_signature().into();
        inner.timestamp = 1_700_000_000_000_u128.saturating_sub(1);
        inner.data_ledgers = vec![prelude_publish_ledger, prelude_submit_ledger];
    }
    let prelude_block = Arc::new(prelude_block_v);

    // Migrate the prelude block
    {
        let block_height_pre_migration = {
            let block_index_guard = BlockIndexReadGuard::new(block_index.clone());
            let bi = block_index_guard.read();
            bi.num_blocks().saturating_sub(1)
        };
        let prelude_all_txs = Arc::new(prelude_headers.clone());
        let (tx_prelude, rx_prelude) = tokio::sync::oneshot::channel();
        block_index_tx
            .send(BlockIndexServiceMessage::MigrateBlock {
                block_header: prelude_block.clone(),
                all_txs: prelude_all_txs,
                response: tx_prelude,
            })
            .expect("send migrate prelude block");
        rx_prelude
            .await
            .expect("receive migration result (prelude)")
            .expect("migrate prelude block");

        // ensure the migration has occured
        loop {
            let current_height = {
                let block_index_guard = BlockIndexReadGuard::new(block_index.clone());
                let bi = block_index_guard.read();
                bi.num_blocks().saturating_sub(1)
            };
            if current_height != block_height_pre_migration {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        }
    }

    // Derive actual previous total chunks from the latest BlockIndexItem to avoid zero-max edge cases
    let prev_total = {
        let prelude_block_index_guard = BlockIndexReadGuard::new(block_index.clone());
        let prelude_index = prelude_block_index_guard.read();
        let latest_height = prelude_index.latest_height();
        let latest_item = prelude_index
            .get_item(latest_height)
            .expect("latest block index item should exist");
        latest_item.ledgers[DataLedger::Submit as usize].total_chunks
    };

    // Align partition_chunk_offset with slot start so that ledger_chunk_offset falls in the next block
    let slot_start =
        parent_slot_index * consensus.num_partitions_per_slot * consensus.num_chunks_in_partition;

    // Enforce deterministic prelude effect: it must advance Submit.total_chunks to exactly slot_start
    assert!(
        prev_total == slot_start,
        "prelude did not add expected chunks: prev_total={} slot_start={}",
        prev_total,
        slot_start
    );
    partition_chunk_offset = 0;

    // Compute PoA chunk using the adjusted partition_chunk_offset
    let mut poa_chunk: Vec<u8> = data.clone();
    {
        let mut entropy_chunk = Vec::<u8>::with_capacity(CHUNK_SIZE as usize);
        compute_entropy_chunk(
            miner_address,
            partition_chunk_offset as u64,
            partition_hash.into(),
            consensus.entropy_packing_iterations,
            CHUNK_SIZE as usize,
            &mut entropy_chunk,
            consensus.chain_id,
        );
        xor_vec_u8_arrays_in_place(&mut poa_chunk, &entropy_chunk);
    }

    // Boundary-like block (height=2), previous is prelude
    let submit_ledger = DataTransactionLedger {
        ledger_id: DataLedger::Submit.into(),
        tx_root,
        tx_ids: H256List(vec![tx_header.id]),
        // Use slot_start + 1 to make this block's bounds [slot_start..slot_start+1)
        total_chunks: slot_start + 1,
        expires: Some(42_000_000),
        proofs: None,
        required_proof_count: None,
    };
    let publish_ledger = DataTransactionLedger {
        ledger_id: DataLedger::Publish.into(),
        tx_root: H256::zero(),
        tx_ids: H256List(Vec::new()),
        total_chunks: 0,
        expires: None,
        proofs: None,
        required_proof_count: Some(1),
    };
    let mut synthetic_block_v = IrysBlockHeader::default();
    {
        let inner = &mut *synthetic_block_v;
        inner.height = 2;
        inner.reward_address = miner_address;
        inner.poa = irys_types::PoaData {
            tx_path: Some(Base64(tx_path)),
            data_path: Some(Base64(data_path)),
            chunk: Some(Base64(poa_chunk.clone())),
            ledger_id: Some(DataLedger::Submit.into()),
            partition_chunk_offset,
            partition_hash,
        };
        inner.block_hash = H256::from_slice(&[2_u8; 32]);
        inner.previous_block_hash = prelude_block_hash;
        inner.previous_cumulative_diff = U256::from(4000);
        inner.miner_address = miner_address;
        inner.signature = Signature::test_signature().into();
        inner.timestamp = 1_700_000_000_000;
        inner.data_ledgers = vec![publish_ledger, submit_ledger];
    }
    let synthetic_block = Arc::new(synthetic_block_v);

    //panic!("gets here");

    // Migrate the boundary-like block into the BlockIndex so get_block_bounds() works
    let txs = Arc::new(vec![tx_header.clone()]);
    let (tx_migrate, rx_migrate) = tokio::sync::oneshot::channel();
    block_index_tx
        .send(BlockIndexServiceMessage::MigrateBlock {
            block_header: synthetic_block.clone(),
            all_txs: txs.clone(),
            response: tx_migrate,
        })
        .expect("send migrate block");
    //panic!("gets here");
    rx_migrate
        .await
        .expect("receive migration result")
        .expect("migrate synthetic block");

    //panic!("does not get here 2");

    // Obtain a read guard for PoA validation
    let block_index_guard = BlockIndexReadGuard::new(block_index.clone());

    // 6) Parent epoch snapshot PoA validation MUST pass
    poa_is_valid(
        &synthetic_block.poa,
        &block_index_guard,
        &parent_epoch_snapshot,
        &consensus,
        &miner_address,
    )
    .expect("Parent epoch snapshot PoA should be valid for crafted block");

    // 7) Build a CHILD-like snapshot by cloning parent and changing the slot index for the
    //    target partition_hash. This simulates an epoch-boundary slot movement.
    let mut child_epoch_snapshot = parent_epoch_snapshot.clone();
    // Fetch existing assignment and re-insert with a different slot_index.
    let mut pa = child_epoch_snapshot
        .partition_assignments
        .get_assignment(partition_hash)
        .expect("partition assignment should exist for partition_hash");
    // Choose a different slot index deterministically (toggle between 0 and 1)
    pa.slot_index = Some(pa.slot_index.unwrap_or(0).wrapping_add(1));
    // Replace the assignment in the cloned snapshot
    child_epoch_snapshot
        .partition_assignments
        .data_partitions
        .insert(partition_hash, pa);

    // 8) Child-like epoch snapshot PoA validation MUST fail due to Merkle proof mismatch
    match poa_is_valid(
        &synthetic_block.poa,
        &block_index_guard,
        &child_epoch_snapshot,
        &consensus,
        &miner_address,
    ) {
        // The child snapshot must reject; depending on slot movement it may fail due to merkle mismatch
        // or because the computed ledger offset is out of the synthetic block's bounds.
        Err(
            PreValidationError::MerkleProofInvalid(_)
            | PreValidationError::PoAChunkOffsetOutOfBlockBounds
            | PreValidationError::BlockBoundsLookupError(_),
        ) => {
            info!("Child-like snapshot rejected PoA as expected due to slot change/out-of-bounds");
        }
        Ok(()) => panic!("Child-like snapshot unexpectedly validated crafted PoA"),
        Err(e) => panic!("Unexpected child-like PoA error: {:?}", e),
    }

    // Keep tmp alive for the duration of the test
    drop(block_index);
    drop(tmp_dir);
    Ok(())
}

/// Initialize a minimal testing context suitable for constructing BlockIndex and EpochSnapshot,
/// returning:
/// - TempDir: to keep paths alive
/// - Arc<RwLock<BlockIndex>>
/// - BlockIndex service sender
/// - Parent EpochSnapshot
/// - Miner Address
/// - ConsensusConfig
async fn init_min_context(
    base_node_config: &NodeConfig,
) -> (
    TempDir,
    Arc<RwLock<BlockIndex>>,
    tokio::sync::mpsc::UnboundedSender<BlockIndexServiceMessage>,
    EpochSnapshot,
    Address,
    irys_types::ConsensusConfig,
    H256,
    irys_types::TokioServiceHandle,
) {
    // Dedicated tmp dir per test
    let tmp = temporary_directory(Some("deterministic_boundary_poa"), false);
    let mut node_config = base_node_config.clone();
    node_config.base_directory = tmp.path().to_path_buf();
    node_config.storage.num_writes_before_sync = 1;

    let consensus = node_config.consensus_config();

    // 1) Build a genesis block and populate commitments
    let mut genesis_block = IrysBlockHeader::new_mock_header();
    genesis_block.height = 0;
    let config = irys_types::Config::new(node_config.clone());

    let (commitments, initial_treasury) =
        add_genesis_commitments(&mut genesis_block, &config).await;
    genesis_block.treasury = initial_treasury;

    // 2) Setup BlockIndex and its service
    let block_index = Arc::new(RwLock::new(
        BlockIndex::new(&node_config)
            .await
            .expect("create block index"),
    ));
    let (block_index_tx, block_index_rx) = tokio::sync::mpsc::unbounded_channel();
    let block_index_handle = BlockIndexService::spawn_service(
        block_index_rx,
        block_index.clone(),
        &consensus,
        tokio::runtime::Handle::current(),
    );

    // 3) Create parent EpochSnapshot from genesis + commitments
    let storage_submodules_config =
        StorageSubmodulesConfig::load(config.node_config.base_directory.clone())
            .expect("load storage submodules config");
    let parent_epoch_snapshot = EpochSnapshot::new(
        &storage_submodules_config,
        genesis_block.clone(),
        commitments,
        &config,
    );

    // 4) Migrate genesis into BlockIndex
    let arc_genesis = Arc::new(genesis_block.clone());
    let (tx_migrate, rx_migrate) = tokio::sync::oneshot::channel();
    block_index_tx
        .send(BlockIndexServiceMessage::MigrateBlock {
            block_header: arc_genesis.clone(),
            all_txs: Arc::new(vec![]),
            response: tx_migrate,
        })
        .expect("send migrate block");
    rx_migrate
        .await
        .expect("recv migrate")
        .expect("index genesis");

    let miner_address = config.irys_signer().address();
    let genesis_hash = genesis_block.block_hash;
    (
        tmp,
        block_index,
        block_index_tx,
        parent_epoch_snapshot,
        miner_address,
        consensus,
        genesis_hash,
        block_index_handle,
    )
}
