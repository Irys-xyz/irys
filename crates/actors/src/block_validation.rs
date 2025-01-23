use crate::{
    block_index_service::BlockIndexReadGuard, epoch_service::PartitionAssignmentsReadGuard,
    mining::hash_to_number, vdf_service::VdfStepsReadGuard,
};
use irys_database::Ledger;
use irys_packing::{capacity_single::compute_entropy_chunk, xor_vec_u8_arrays_in_place};
use irys_storage::ii;
use irys_types::{
    calculate_difficulty, next_cumulative_diff, storage_config::StorageConfig, validate_path,
    Address, DifficultyAdjustmentConfig, IrysBlockHeader, PoaData, VDFStepsConfig, H256,
};
use irys_vdf::last_step_checkpoints_is_valid;
use openssl::sha;
use tracing::{debug, info};

/// Full pre-validation steps for a block
pub async fn prevalidate_block(
    block: IrysBlockHeader,
    previous_block: IrysBlockHeader,
    _block_index_guard: BlockIndexReadGuard,
    _partitions_guard: PartitionAssignmentsReadGuard,
    storage_config: StorageConfig,
    difficulty_config: DifficultyAdjustmentConfig,
    vdf_config: VDFStepsConfig,
    steps_guard: VdfStepsReadGuard,
    _miner_address: Address,
) -> eyre::Result<()> {
    if block.chunk_hash != sha::sha256(&block.poa.chunk.0).into() {
        return Err(eyre::eyre!(
            "Invalid block: chunk hash distinct from PoA chunk hash"
        ));
    }

    // Check prev_output (vdf)
    prev_output_is_valid(&block, &previous_block)?;

    // Check the difficulty
    difficulty_is_valid(&block, &previous_block, &difficulty_config)?;

    // Check the cumulative difficulty
    cumulative_difficulty_is_valid(&block, &previous_block)?;

    // Check the solution_hash
    solution_hash_is_valid(&block)?;

    // Recall range check
    recall_recall_range_is_valid(&block, &storage_config, &steps_guard)?;

    // We only check last_step_checkpoints during pre-validation
    last_step_checkpoints_is_valid(&block.vdf_limiter_info, &vdf_config).await?;

    Ok(())
}

pub fn prev_output_is_valid(
    block: &IrysBlockHeader,
    previous_block: &IrysBlockHeader,
) -> eyre::Result<()> {
    if block.vdf_limiter_info.prev_output == previous_block.vdf_limiter_info.output {
        Ok(())
    } else {
        Err(eyre::eyre!(
            "vdf_limiter.prev_output does not match previous blocks vdf_limiter.output"
        ))
    }
}

/// Validates if a block's difficulty matches the expected difficulty calculated
/// from previous block data.
/// Returns Ok if valid, Err if the difficulty doesn't match the calculated value.
pub fn difficulty_is_valid(
    block: &IrysBlockHeader,
    previous_block: &IrysBlockHeader,
    difficulty_config: &DifficultyAdjustmentConfig,
) -> eyre::Result<()> {
    let block_height = block.height;
    let current_timestamp = block.timestamp;
    let last_diff_timestamp = previous_block.last_diff_timestamp;
    let current_difficulty = previous_block.diff;

    let (diff, _stats) = calculate_difficulty(
        block_height,
        last_diff_timestamp,
        current_timestamp,
        current_difficulty,
        &difficulty_config,
    );

    if diff == block.diff {
        Ok(())
    } else {
        Err(eyre::eyre!("Invalid difficulty"))
    }
}

/// Validates if a block's cumulative difficulty equals the previous cumulative difficulty
/// plus the expected hashes from its new difficulty. Returns Ok if valid.
///
/// Note: Requires valid block difficulty - call difficulty_is_valid() first.
pub fn cumulative_difficulty_is_valid(
    block: &IrysBlockHeader,
    previous_block: &IrysBlockHeader,
) -> eyre::Result<()> {
    let previous_cumulative_diff = previous_block.cumulative_diff;
    let new_diff = block.diff;

    let cumulative_diff = next_cumulative_diff(previous_cumulative_diff, new_diff);
    if cumulative_diff == block.cumulative_diff {
        Ok(())
    } else {
        Err(eyre::eyre!("Invalid cumulative_difficulty"))
    }
}

/// Checks to see if the `solution_hash` exceeds the difficulty threshold
///
/// Note: Requires valid block difficulty - call difficulty_is_valid() first.
pub fn solution_hash_is_valid(block: &IrysBlockHeader) -> eyre::Result<()> {
    let solution_hash = block.solution_hash;
    let solution_diff = hash_to_number(&solution_hash.0);

    if solution_diff >= block.diff {
        Ok(())
    } else {
        Err(eyre::eyre!("Invalid solution_hash"))
    }
}

/// Returns Ok if the provided `PoA` is valid, Err otherwise
/// Check recall range is valid
pub fn recall_recall_range_is_valid(
    block: &IrysBlockHeader,
    config: &StorageConfig,
    steps_guard: &VdfStepsReadGuard,
) -> eyre::Result<()> {
    let num_recall_ranges_in_partition =
        irys_efficient_sampling::num_recall_ranges_in_partition(config);
    let reset_step_number = irys_efficient_sampling::reset_step_number(
        block.vdf_limiter_info.global_step_number,
        config,
    );
    info!(
        "Validating recall ranges steps from: {} to: {}",
        reset_step_number, block.vdf_limiter_info.global_step_number
    );
    let steps = steps_guard.read().get_steps(ii(
        reset_step_number,
        block.vdf_limiter_info.global_step_number,
    ))?;
    irys_efficient_sampling::recall_range_is_valid(
        (block.poa.partition_chunk_offset as u64 / config.num_chunks_in_recall_range) as usize,
        num_recall_ranges_in_partition as usize,
        &steps,
        &block.poa.partition_hash,
    )
}

pub fn get_recall_range(
    step_num: u64,
    config: &StorageConfig,
    steps_guard: &VdfStepsReadGuard,
    partition_hash: &H256,
) -> eyre::Result<usize> {
    let num_recall_ranges_in_partition =
        irys_efficient_sampling::num_recall_ranges_in_partition(config);
    let reset_step_number = irys_efficient_sampling::reset_step_number(step_num, config);
    let steps = steps_guard
        .read()
        .get_steps(ii(reset_step_number, step_num))?;
    irys_efficient_sampling::get_recall_range(
        num_recall_ranges_in_partition as usize,
        &steps,
        &partition_hash,
    )
}

/// Returns Ok if the provided PoA is valid, Err otherwise
pub fn poa_is_valid(
    poa: &PoaData,
    block_index_guard: &BlockIndexReadGuard,
    partitions_guard: &PartitionAssignmentsReadGuard,
    config: &StorageConfig,
    miner_address: &Address,
) -> eyre::Result<()> {
    debug!("PoA validating mining address: {:?} chunk_offset: {} partition hash: {:?} iterations: {} chunk size: {}", miner_address, poa.partition_chunk_offset, poa.partition_hash, config.entropy_packing_iterations, config.chunk_size);
    // data chunk
    if let (Some(data_path), Some(tx_path), Some(ledger_id)) =
        (poa.data_path.clone(), poa.tx_path.clone(), poa.ledger_id)
    {
        // partition data -> ledger data
        let partition_assignment = partitions_guard
            .read()
            .get_assignment(poa.partition_hash)
            .unwrap();

        let ledger_chunk_offset = partition_assignment.slot_index.unwrap() as u64
            * config.num_partitions_in_slot
            * config.num_chunks_in_partition
            + poa.partition_chunk_offset as u64;

        // ledger data -> block
        let ledger = Ledger::try_from(ledger_id).unwrap();

        let bb = block_index_guard
            .read()
            .get_block_bounds(ledger, ledger_chunk_offset);
        if !(bb.start_chunk_offset..=bb.end_chunk_offset).contains(&ledger_chunk_offset) {
            return Err(eyre::eyre!("PoA chunk offset out of block bounds"));
        };

        let block_chunk_offset = (ledger_chunk_offset - bb.start_chunk_offset) as u128;

        // tx_path validation
        let tx_path_result = validate_path(
            bb.tx_root.0,
            &tx_path,
            block_chunk_offset * (config.chunk_size as u128),
        )?;

        if !(tx_path_result.left_bound..=tx_path_result.right_bound)
            .contains(&(block_chunk_offset * (config.chunk_size as u128)))
        {
            return Err(eyre::eyre!("PoA chunk offset out of tx bounds"));
        }

        let tx_chunk_offset =
            block_chunk_offset * (config.chunk_size as u128) - tx_path_result.left_bound;

        // data_path validation
        let data_path_result =
            validate_path(tx_path_result.leaf_hash, &data_path, tx_chunk_offset)?;

        if !(data_path_result.left_bound..=data_path_result.right_bound).contains(&tx_chunk_offset)
        {
            return Err(eyre::eyre!(
                "PoA chunk offset out of tx's data chunks bounds"
            ));
        }

        let mut entropy_chunk = Vec::<u8>::with_capacity(config.chunk_size as usize);
        compute_entropy_chunk(
            *miner_address,
            poa.partition_chunk_offset as u64,
            poa.partition_hash.into(),
            config.entropy_packing_iterations,
            config.chunk_size as usize,
            &mut entropy_chunk,
        );

        let mut poa_chunk: Vec<u8> = poa.chunk.clone().into();
        xor_vec_u8_arrays_in_place(&mut poa_chunk, &entropy_chunk);

        // Because all chunks are packed as config.chunk_size, if the proof chunk is
        // smaller we need to trim off the excess padding introduced by packing ?
        let (poa_chunk_pad_trimmed, _) = poa_chunk.split_at(
            (config
                .chunk_size
                .min((data_path_result.right_bound - data_path_result.left_bound) as u64))
                as usize,
        );

        let poa_chunk_hash = sha::sha256(poa_chunk_pad_trimmed);

        if poa_chunk_hash != data_path_result.leaf_hash {
            return Err(eyre::eyre!(
                "PoA chunk hash mismatch\n{:?}\nleaf_hash: {:?}\nledger_id: {}\nledger_chunk_offset: {}",
                poa_chunk,
                data_path_result.leaf_hash,
                ledger_id,
                ledger_chunk_offset
            ));
        }
    } else {
        let mut entropy_chunk = Vec::<u8>::with_capacity(config.chunk_size as usize);
        compute_entropy_chunk(
            *miner_address,
            poa.partition_chunk_offset as u64,
            poa.partition_hash.into(),
            config.entropy_packing_iterations,
            config.chunk_size as usize,
            &mut entropy_chunk,
        );

        if entropy_chunk != poa.chunk.0 {
            if poa.chunk.0.len() <= 32 {
                debug!("Chunk PoA:{:?}", poa.chunk.0);
                debug!("Entropy  :{:?}", entropy_chunk);
            }
            return Err(eyre::eyre!(
                "PoA capacity chunk mismatch {:?} /= {:?}",
                entropy_chunk.first(),
                poa.chunk.0.first()
            ));
        }
    }
    Ok(())
}

//==============================================================================
// Tests
//------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use crate::{
        block_index_service::{BlockIndexService, GetBlockIndexGuardMessage},
        epoch_service::{
            EpochServiceActor, EpochServiceConfig, GetLedgersGuardMessage,
            GetPartitionAssignmentsGuardMessage, NewEpochMessage,
        },
        BlockFinalizedMessage,
    };
    use actix::prelude::*;
    use dev::Registry;
    use irys_config::IrysNodeConfig;
    use irys_database::{BlockIndex, Initialized};
    use irys_types::{
        irys::IrysSigner, partition::PartitionAssignment, Address, Base64, H256List,
        IrysTransaction, IrysTransactionHeader, Signature, TransactionLedger, H256, U256,
    };
    use std::sync::{Arc, RwLock};
    use tracing::log::LevelFilter;
    use tracing::{debug, info};

    use super::*;

    pub(super) struct TestContext {
        pub block_index: Arc<RwLock<BlockIndex<Initialized>>>,
        pub block_index_actor: Addr<BlockIndexService>,
        pub partitions_guard: PartitionAssignmentsReadGuard,
        pub storage_config: StorageConfig,
        pub miner_address: Address,
        pub partition_hash: H256,
        pub partition_assignment: PartitionAssignment,
    }

    async fn init() -> TestContext {
        let _ = env_logger::builder()
            // Include all events in tests
            .filter_level(LevelFilter::max())
            // Ensure events are captured by `cargo test`
            .is_test(true)
            // Ignore errors initializing the logger if tests race to configure it
            .try_init();

        let mut genesis_block = IrysBlockHeader::new();
        genesis_block.height = 0;
        let arc_genesis = Arc::new(genesis_block);

        let miner_address = Address::random();
        let chunk_size = 32;

        // Create epoch service with random miner address
        let storage_config = StorageConfig {
            chunk_size: chunk_size,
            num_chunks_in_partition: 10,
            num_chunks_in_recall_range: 2,
            num_partitions_in_slot: 1,
            miner_address,
            min_writes_before_sync: 1,
            entropy_packing_iterations: 1_000,
            num_confirmations_for_finality: 1, // Testnet / single node config
        };

        let config = EpochServiceConfig {
            storage_config: storage_config.clone(),
            ..Default::default()
        };

        let epoch_service = EpochServiceActor::new(Some(config.clone()));
        let epoch_service_addr = epoch_service.start();

        // Tell the epoch service to initialize the ledgers
        let msg = NewEpochMessage(arc_genesis.clone());
        match epoch_service_addr.send(msg).await {
            Ok(_) => info!("Genesis Epoch tasks complete."),
            Err(_) => panic!("Failed to perform genesis epoch tasks"),
        }

        let ledgers_guard = epoch_service_addr
            .send(GetLedgersGuardMessage)
            .await
            .unwrap();
        let partitions_guard = epoch_service_addr
            .send(GetPartitionAssignmentsGuardMessage)
            .await
            .unwrap();

        let ledgers = ledgers_guard.read();
        debug!("ledgers: {:?}", ledgers);

        let sub_slots = ledgers.get_slots(Ledger::Submit);

        let partition_hash = sub_slots[0].partitions[0];

        let arc_config = Arc::new(IrysNodeConfig::default());
        let block_index: Arc<RwLock<BlockIndex<Initialized>>> = Arc::new(RwLock::new(
            BlockIndex::default()
                .reset(&arc_config.clone())
                .unwrap()
                .init(arc_config.clone())
                .await
                .unwrap(),
        ));

        let block_index_actor = BlockIndexService::new(block_index.clone(), storage_config.clone());
        Registry::set(block_index_actor.start());

        let msg = BlockFinalizedMessage {
            block_header: arc_genesis.clone(),
            all_txs: Arc::new(vec![]),
        };

        let block_index_actor = BlockIndexService::from_registry();
        match block_index_actor.send(msg).await {
            Ok(_) => info!("Genesis block indexed"),
            Err(_) => panic!("Failed to index genesis block"),
        }

        let partition_assignment = partitions_guard
            .read()
            .get_assignment(partition_hash)
            .unwrap();

        debug!("Partition assignment {:?}", partition_assignment);

        TestContext {
            block_index,
            block_index_actor,
            partitions_guard,
            storage_config,
            miner_address,
            partition_hash,
            partition_assignment,
        }
    }

    #[actix::test]
    async fn poa_test_3_complete_txs() {
        let chunk_size: usize = 32;
        let context = init().await;
        // Create a bunch of TX chunks
        let data_chunks = vec![
            vec![[0; 32], [1; 32], [2; 32]], // tx0
            vec![[3; 32], [4; 32], [5; 32]], // tx1
            vec![[6; 32], [7; 32], [8; 32]], // tx2
        ];

        // Create a bunch of signed TX from the chunks
        // Loop though all the data_chunks and create wrapper tx for them
        let signer = IrysSigner::random_signer_with_chunk_size(chunk_size);
        let mut txs: Vec<IrysTransaction> = Vec::new();

        for chunks in &data_chunks {
            let mut data: Vec<u8> = Vec::new();
            for chunk in chunks {
                data.extend_from_slice(chunk);
            }
            let tx = signer.create_transaction(data, None).unwrap();
            let tx = signer.sign_transaction(tx).unwrap();
            txs.push(tx);
        }

        for poa_tx_num in 0..3 {
            for poa_chunk_num in 0..3 {
                let mut poa_chunk: Vec<u8> = data_chunks[poa_tx_num][poa_chunk_num].into();
                poa_test(
                    &context,
                    &txs,
                    &mut poa_chunk,
                    poa_tx_num,
                    poa_chunk_num,
                    9,
                    chunk_size,
                )
                .await;
            }
        }
    }

    #[actix::test]
    async fn poa_not_complete_last_chunk_test() {
        let context = init().await;
        let chunk_size: usize = 32;

        // Create a signed TX from the chunks
        let signer = IrysSigner::random_signer_with_chunk_size(chunk_size);
        let mut txs: Vec<IrysTransaction> = Vec::new();

        let data = vec![3; 40]; //32 + 8 last incomplete chunk
        let tx = signer.create_transaction(data.clone(), None).unwrap();
        let tx = signer.sign_transaction(tx).unwrap();
        txs.push(tx);

        let poa_tx_num = 0;

        for poa_chunk_num in 0..2 {
            let mut poa_chunk: Vec<u8> = data[poa_chunk_num * chunk_size
                ..std::cmp::min((poa_chunk_num + 1) * chunk_size, data.len())]
                .to_vec();
            poa_test(
                &context,
                &txs,
                &mut poa_chunk,
                poa_tx_num,
                poa_chunk_num,
                2,
                chunk_size,
            )
            .await;
        }
    }

    async fn poa_test(
        context: &TestContext,
        txs: &Vec<IrysTransaction>,
        poa_chunk: &mut Vec<u8>,
        poa_tx_num: usize,
        poa_chunk_num: usize,
        total_chunks_in_tx: usize,
        chunk_size: usize,
    ) {
        // Initialize genesis block at height 0
        let height: u64;
        {
            height = context.block_index.read().unwrap().num_blocks();
        }

        let mut entropy_chunk = Vec::<u8>::with_capacity(chunk_size);
        compute_entropy_chunk(
            context.miner_address,
            (poa_tx_num * 3 /* tx's size in chunks */  + poa_chunk_num) as u64,
            context.partition_hash.into(),
            context.storage_config.entropy_packing_iterations,
            chunk_size,
            &mut entropy_chunk,
        );

        xor_vec_u8_arrays_in_place(poa_chunk, &entropy_chunk);

        // Create vectors of tx headers and txids
        let tx_headers: Vec<IrysTransactionHeader> =
            txs.iter().map(|tx| tx.header.clone()).collect();

        let data_tx_ids = tx_headers.iter().map(|h| h.id).collect::<Vec<H256>>();

        let (tx_root, tx_path) = TransactionLedger::merklize_tx_root(&tx_headers);

        let poa = PoaData {
            tx_path: Some(Base64(tx_path[poa_tx_num].proof.clone())),
            data_path: Some(Base64(txs[poa_tx_num].proofs[poa_chunk_num].proof.clone())),
            chunk: Base64(poa_chunk.clone()),
            ledger_id: Some(1),
            partition_chunk_offset: (poa_tx_num * 3 /* 3 chunks in each tx */ + poa_chunk_num)
                as u32,
            recall_chunk_index: 0,
            partition_hash: context.partition_hash,
        };

        // Create a block from the tx
        let irys_block = IrysBlockHeader {
            height,
            reward_address: context.miner_address,
            poa: poa.clone(),
            block_hash: H256::zero(),
            previous_block_hash: H256::zero(),
            previous_cumulative_diff: U256::from(4000),
            miner_address: context.miner_address,
            signature: Signature::test_signature().into(),
            timestamp: 1000,
            ledgers: vec![
                // Permanent Publish Ledger
                TransactionLedger {
                    ledger_id: Ledger::Publish.into(),
                    tx_root: H256::zero(),
                    tx_ids: H256List(Vec::new()),
                    max_chunk_offset: 0,
                    expires: None,
                    proofs: None,
                },
                // Term Submit Ledger
                TransactionLedger {
                    ledger_id: Ledger::Submit.into(),
                    tx_root,
                    tx_ids: H256List(data_tx_ids.clone()),
                    max_chunk_offset: 9,
                    expires: Some(1622543200),
                    proofs: None,
                },
            ],
            ..IrysBlockHeader::default()
        };

        // Send the block confirmed message
        let block = Arc::new(irys_block);
        let txs = Arc::new(tx_headers);
        let block_finalized_message = BlockFinalizedMessage {
            block_header: block.clone(),
            all_txs: Arc::clone(&txs),
        };

        match context
            .block_index_actor
            .send(block_finalized_message.clone())
            .await
        {
            Ok(_) => info!("Second block indexed"),
            Err(_) => panic!("Failed to index second block"),
        };

        let block_index_guard = context
            .block_index_actor
            .send(GetBlockIndexGuardMessage)
            .await
            .unwrap();

        let ledger_chunk_offset = context.partition_assignment.slot_index.unwrap() as u64
            * context.storage_config.num_partitions_in_slot
            * context.storage_config.num_chunks_in_partition
            + (poa_tx_num * 3 /* 3 chunks in each tx */ + poa_chunk_num) as u64;

        assert_eq!(
            ledger_chunk_offset,
            (poa_tx_num * 3 /* 3 chunks in each tx */ + poa_chunk_num) as u64,
            "ledger_chunk_offset mismatch"
        );

        // ledger data -> block
        let bb = block_index_guard
            .read()
            .get_block_bounds(Ledger::Submit, ledger_chunk_offset);
        info!("block bounds: {:?}", bb);

        assert_eq!(bb.start_chunk_offset, 0, "start_chunk_offset should be 0");
        assert_eq!(
            bb.end_chunk_offset, total_chunks_in_tx as u64,
            "end_chunk_offset should be 9, tx has 9 chunks"
        );

        let poa_valid = poa_is_valid(
            &poa,
            &block_index_guard,
            &context.partitions_guard,
            &context.storage_config,
            &context.miner_address,
        );

        assert!(poa_valid.is_ok(), "PoA should be valid");
    }
}
