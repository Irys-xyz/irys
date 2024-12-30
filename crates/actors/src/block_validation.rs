use crate::{block_index::BlockIndexReadGuard, epoch_service::PartitionAssignmentsReadGuard};
use irys_database::Ledger;
use irys_packing::{capacity_single::compute_entropy_chunk, xor_vec_u8_arrays_in_place};
use irys_types::{storage_config::StorageConfig, validate_path, IrysBlockHeader, PoaData};
use openssl::sha;

/// Full pre-validation steps for a block
pub fn block_is_valid(
    block: &IrysBlockHeader,
    prev_block_header: Option<IrysBlockHeader>,
) -> eyre::Result<()> {
    if block.chunk_hash != sha::sha256(&block.poa.chunk.0).into() {
        return Err(eyre::eyre!(
            "Invalid block: chunk hash distinct from PoA chunk hash"
        ));
    }

    //TODO: check block_hash

    //TODO: check vdf steps

    // check PoA recall range

    if let Some(prev_block) = prev_block_header {
        if block.previous_block_hash != prev_block.block_hash {
            return Err(eyre::eyre!(
                "Invalid block: previous blocks indep_hash is not the parent block"
            ));
        }

        // check retarget

        // check difficulty

        //
    } // if there is no previous block error ?

    Ok(())
}

/// Returns Ok if the provided PoA is valid, Err otherwise
pub fn poa_is_valid(
    poa: &PoaData,
    block_index_guard: &BlockIndexReadGuard,
    partitions_guard: &PartitionAssignmentsReadGuard,
    config: &StorageConfig,
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

        // TODO: check if bounds are byte or chunk relative
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
            config.miner_address,
            poa.partition_chunk_offset,
            poa.partition_hash.into(),
            config.entropy_packing_iterations,
            config.chunk_size as usize,
            &mut entropy_chunk,
        );

        let mut poa_chunk: Vec<u8> = poa.chunk.clone().into();
        xor_vec_u8_arrays_in_place(&mut poa_chunk, &entropy_chunk);

        // TODO: check if bounds are byte or chunk relative, if are chunk how I remove last chunk padding ?
        // Because all chunks are packed as config.chunk_size, if the proof chunk is
        // smaller we need to trim off the excess padding introduced by packing ?
        let (poa_chunk_padd_trimmed, _) = poa_chunk.split_at(
            (config
                .chunk_size
                .min((data_path_result.right_bound - data_path_result.left_bound) as u64))
                as usize,
        );

        let poa_chunk_hash = sha::sha256(&poa_chunk_padd_trimmed);

        if !(poa_chunk_hash == data_path_result.leaf_hash) {
            return Err(eyre::eyre!("PoA chunk hash mismatch"));
        }
    } else {
        let mut entropy_chunk = Vec::<u8>::with_capacity(config.chunk_size as usize);
        compute_entropy_chunk(
            config.miner_address,
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
        irys::IrysSigner, Address, Base64, H256List, IrysSignature, IrysTransaction,
        IrysTransactionHeader, Signature, TransactionLedger, VDFLimiterInfo, H256, U256,
    };
    use reth::revm::primitives::B256;
    use std::str::FromStr;
    use std::sync::{Arc, RwLock};
    use tracing::log::LevelFilter;
    use tracing::{debug, error, info};

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
    async fn poa_test() {
        init_logger();
        // Initialize genesis block at height 0
        let mut genesis_block = IrysBlockHeader::new();
        genesis_block.height = 0;
        let arc_genesis = Arc::new(genesis_block);

        let chunk_size = 32;
        let miner_address = Address::random();

        // Create epoch service with random miner address
        let storage_config = StorageConfig {
            chunk_size,
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

        let mut epoch_service = EpochServiceActor::new(Some(config.clone()));
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

        // Create a bunch of TX chunks
        let mut data_chunks = vec![
            vec![[0; 32], [1; 32], [2; 32]], // tx0 TODO: test last not complete chunk
            vec![[3; 32], [4; 32], [5; 32]], // tx1
            vec![[6; 32], [7; 32], [8; 32]], // tx2
        ];

        let poa_tx_num = 1;
        let poa_chunk_num = 2;

        // Create a bunch of signed TX from the chunks
        // Loop though all the data_chunks and create wrapper tx for them
        let signer = IrysSigner::random_signer_with_chunk_size(chunk_size as usize);
        let mut txs: Vec<IrysTransaction> = Vec::new();

        for chunks in &data_chunks {
            let mut data: Vec<u8> = Vec::new();
            for chunk in chunks {
                data.extend_from_slice(chunk);
            }
            info!("tx data len {}", data.len());
            let tx = signer.create_transaction(data, None).unwrap();
            let tx = signer.sign_transaction(tx).unwrap();
            info!("tx data in tx header len {}", tx.header.data_size);
            txs.push(tx);
        }

        // Create vectors of tx headers and txids
        let tx_headers: Vec<IrysTransactionHeader> =
            txs.iter().map(|tx| tx.header.clone()).collect();
        let data_tx_ids = tx_headers
            .iter()
            .map(|h| h.id.clone())
            .collect::<Vec<H256>>();

        let mut entropy_chunk = Vec::<u8>::with_capacity(chunk_size as usize);
        compute_entropy_chunk(
            miner_address,
            poa_tx_num * 3 /* tx's size in chunks */  + poa_chunk_num,
            partition_hash.into(),
            config.storage_config.entropy_packing_iterations,
            chunk_size as usize,
            &mut entropy_chunk,
        );

        let mut poa_chunk: Vec<u8> =
            data_chunks[poa_tx_num as usize][poa_chunk_num as usize].into();
        xor_vec_u8_arrays_in_place(&mut poa_chunk, &entropy_chunk);

        let (tx_root, tx_path) = TransactionLedger::merklize_tx_root(&tx_headers);

        let poa = PoaData {
            tx_path: Some(Base64(tx_path[poa_tx_num as usize].proof.clone())),
            data_path: Some(Base64(
                txs[poa_tx_num as usize].proofs[poa_chunk_num as usize]
                    .proof
                    .clone(),
            )),
            chunk: Base64(poa_chunk),
            ledger_num: Some(1),
            partition_chunk_offset: poa_tx_num * 3 /* 3 chunks in each tx */ + poa_chunk_num,
            partition_hash,
        };

        // Create a block from the tx
        let irys_block = IrysBlockHeader {
            diff: U256::from(1000),
            last_diff_timestamp: 1622543200,
            cumulative_diff: U256::from(5000),
            solution_hash: H256::zero(),
            previous_solution_hash: H256::zero(),
            last_epoch_hash: H256::random(),
            chunk_hash: H256::zero(),
            height,
            block_hash: H256::zero(),
            previous_block_hash: H256::zero(),
            previous_cumulative_diff: U256::from(4000),
            poa: poa.clone(),
            reward_address: Address::ZERO,
            reward_key: Base64::from_str("").unwrap(),
            signature: Signature::test_signature().into(),
            timestamp: 1000,
            ledgers: vec![
                // Permanent Publish Ledger
                TransactionLedger {
                    tx_root: H256::zero(),
                    txids: H256List(Vec::new()),
                    proofs: None,
                    max_chunk_offset: 0,
                    expires: None,
                },
                // Term Submit Ledger
                TransactionLedger {
                    tx_root,
                    txids: H256List(data_tx_ids.clone()),
                    proofs: None,
                    max_chunk_offset: 9,
                    expires: Some(1622543200),
                },
            ],
            evm_block_hash: B256::ZERO,
            vdf_limiter_info: VDFLimiterInfo::default(),
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
            + poa_tx_num * 3 /* 3 chunks in each tx */ + poa_chunk_num;

        info!("ledger chunk offset: {:?}", ledger_chunk_offset);

        // ledger data -> block
        let bb = block_index_guard
            .read()
            .get_block_bounds(Ledger::Submit, ledger_chunk_offset);
        info!("block bounds: {:?}", bb);

        if let Err(err) = poa_is_valid(&poa, &block_index_guard, &partitions_guard, &storage_config)
        {
            panic!("PoA error {:?}", err);
        }
    }
}
