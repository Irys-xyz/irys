use crate::{block_index::BlockIndexReadGuard, epoch_service::PartitionAssignmentsReadGuard};
use irys_database::Ledger;
use irys_packing::{capacity_single::compute_entropy_chunk, xor_vec_u8_arrays_in_place};
use irys_types::{
    storage_config::StorageConfig, validate_path, Address, IrysBlockHeader, PoaData, VDFStepsConfig,
};
use irys_vdf::checkpoints_are_valid;
use openssl::sha;

/// Full pre-validation steps for a block
pub fn block_is_valid(
    block: &IrysBlockHeader,
    block_index_guard: &BlockIndexReadGuard,
    partitions_guard: &PartitionAssignmentsReadGuard,
    storage_config: &StorageConfig,
    vdf_config: &VDFStepsConfig,
    miner_address: &Address,
) -> eyre::Result<()> {
    if block.chunk_hash != sha::sha256(&block.poa.chunk.0).into() {
        return Err(eyre::eyre!(
            "Invalid block: chunk hash distinct from PoA chunk hash"
        ));
    }

    //TODO: check block_hash

    // check vdf steps
    checkpoints_are_valid(&block.vdf_limiter_info, vdf_config)?;

    // check PoA
    poa_is_valid(
        &block.poa,
        block_index_guard,
        partitions_guard,
        storage_config,
        miner_address,
    )?;
    Ok(())
}

/// Returns Ok if the provided `PoA` is valid, Err otherwise
pub fn poa_is_valid(
    poa: &PoaData,
    block_index_guard: &BlockIndexReadGuard,
    partitions_guard: &PartitionAssignmentsReadGuard,
    config: &StorageConfig,
    miner_address: &Address,
) -> eyre::Result<()> {
    // data chunk
    if let (Some(data_path), Some(tx_path), Some(ledger_num)) =
        (poa.data_path.clone(), poa.tx_path.clone(), poa.ledger_num)
    {
        // partition data -> ledger data
        let partition_assignment = partitions_guard
            .read()
            .get_assignment(poa.partition_hash)
            .unwrap();

        let ledger_chunk_offset = partition_assignment.slot_index.unwrap() as u64
            * config.num_partitions_in_slot
            * config.num_chunks_in_partition
            + poa.partition_chunk_offset;

        // ledger data -> block
        let ledger = Ledger::try_from(ledger_num).unwrap();

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
            poa.partition_chunk_offset,
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
            return Err(eyre::eyre!("PoA chunk hash mismatch"));
        }
    } else {
        let mut entropy_chunk = Vec::<u8>::with_capacity(config.chunk_size as usize);
        compute_entropy_chunk(
            *miner_address,
            poa.partition_chunk_offset,
            poa.partition_hash.into(),
            config.entropy_packing_iterations,
            config.chunk_size as usize,
            &mut entropy_chunk,
        );

        if entropy_chunk != poa.chunk.0 {
            return Err(eyre::eyre!("PoA capacity chunk mismatch"));
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
        block_index::{BlockIndexActor, GetBlockIndexGuardMessage},
        block_producer::BlockConfirmedMessage,
        epoch_service::{
            EpochServiceActor, EpochServiceConfig, GetLedgersGuardMessage,
            GetPartitionAssignmentsGuardMessage, NewEpochMessage,
        },
    };
    use actix::prelude::*;
    use irys_config::IrysNodeConfig;
    use irys_database::{BlockIndex, Initialized};
    use irys_types::{
        irys::IrysSigner, Address, Base64, H256List, IrysTransaction, IrysTransactionHeader,
        Signature, TransactionLedger, H256, U256,
    };

    use std::sync::{Arc, RwLock};
    use tracing::log::LevelFilter;
    use tracing::{debug, info};

    use super::*;

    fn init_logger() {
        let _ = env_logger::builder()
            // Include all events in tests
            .filter_level(LevelFilter::max())
            // Ensure events are captured by `cargo test`
            .is_test(true)
            // Ignore errors initializing the logger if tests race to configure it
            .try_init();
    }

    #[actix::test]
    async fn poa_test_3_complete_txs() {
        let chunk_size: usize = 32;
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
        txs: &Vec<IrysTransaction>,
        poa_chunk: &mut Vec<u8>,
        poa_tx_num: usize,
        poa_chunk_num: usize,
        total_chunks_in_tx: usize,
        chunk_size: usize,
    ) {
        init_logger();
        // Initialize genesis block at height 0
        let mut genesis_block = IrysBlockHeader::new();
        genesis_block.height = 0;
        let arc_genesis = Arc::new(genesis_block);

        let miner_address = Address::random();

        // Create epoch service with random miner address
        let storage_config = StorageConfig {
            chunk_size: chunk_size.try_into().unwrap(),
            num_chunks_in_partition: 10,
            num_chunks_in_recall_range: 2,
            num_partitions_in_slot: 1,
            miner_address,
            min_writes_before_sync: 1,
            entropy_packing_iterations: 1_000,
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

        let block_index_actor = BlockIndexActor::new(block_index.clone(), storage_config.clone());
        let block_index_addr = block_index_actor.start();

        let msg = BlockConfirmedMessage(arc_genesis.clone(), Arc::new(vec![]));

        match block_index_addr.send(msg).await {
            Ok(_) => info!("Genesis block indexed"),
            Err(_) => panic!("Failed to index genesis block"),
        }

        let partition_assignment = partitions_guard
            .read()
            .get_assignment(partition_hash)
            .unwrap();

        debug!("Partition assignment {:?}", partition_assignment);

        let height: u64;
        {
            height = block_index.read().unwrap().num_blocks().max(1) - 1;
        }

        let mut entropy_chunk = Vec::<u8>::with_capacity(chunk_size);
        compute_entropy_chunk(
            miner_address,
            (poa_tx_num * 3 /* tx's size in chunks */  + poa_chunk_num) as u64,
            partition_hash.into(),
            config.storage_config.entropy_packing_iterations,
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
            ledger_num: Some(1),
            partition_chunk_offset: (poa_tx_num * 3 /* 3 chunks in each tx */ + poa_chunk_num)
                as u64,
            partition_hash,
        };

        // Create a block from the tx
        let irys_block = IrysBlockHeader {
            height,
            reward_address: miner_address,
            poa: poa.clone(),
            block_hash: H256::zero(),
            previous_block_hash: H256::zero(),
            previous_cumulative_diff: U256::from(4000),
            miner_address,
            signature: Signature::test_signature().into(),
            timestamp: 1000,
            ledgers: vec![
                // Permanent Publish Ledger
                TransactionLedger {
                    tx_root: H256::zero(),
                    txids: H256List(Vec::new()),
                    max_chunk_offset: 0,
                    expires: None,
                    proofs: None,
                },
                // Term Submit Ledger
                TransactionLedger {
                    tx_root,
                    txids: H256List(data_tx_ids.clone()),
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
        let block_confirm_message = BlockConfirmedMessage(block.clone(), Arc::clone(&txs));

        match block_index_addr.send(block_confirm_message.clone()).await {
            Ok(_) => info!("Second block indexed"),
            Err(_) => panic!("Failed to index second block"),
        };

        let block_index_guard = block_index_addr
            .send(GetBlockIndexGuardMessage)
            .await
            .unwrap();

        let ledger_chunk_offset = partition_assignment.slot_index.unwrap() as u64
            * storage_config.num_partitions_in_slot
            * storage_config.num_chunks_in_partition
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
            &partitions_guard,
            &storage_config,
            &miner_address,
        );

        assert!(poa_valid.is_ok(), "PoA should be valid");
    }
}
