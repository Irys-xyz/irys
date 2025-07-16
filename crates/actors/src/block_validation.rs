use std::{sync::Arc, time::Duration};

use crate::block_tree_service::ValidationResult;
use crate::{
    block_discovery::{get_commitment_tx_in_parallel, get_data_tx_in_parallel},
    mempool_service::MempoolServiceMessage,
    mining::hash_to_number,
    services::ServiceSenders,
    shadow_tx_generator::ShadowTxGenerator,
};
use alloy_consensus::Transaction as _;
use alloy_eips::eip7685::{Requests, RequestsOrHash};
use alloy_rpc_types_engine::ExecutionData;
use async_trait::async_trait;
use base58::ToBase58 as _;
use eyre::{ensure, OptionExt as _};
use irys_database::{block_header_by_hash, db::IrysDatabaseExt as _, SystemLedger};
use irys_domain::{BlockIndexReadGuard, EmaSnapshot, EpochSnapshot};
use irys_packing::{capacity_single::compute_entropy_chunk, xor_vec_u8_arrays_in_place};
use irys_reth::alloy_rlp::Decodable as _;
use irys_reth::shadow_tx::ShadowTransaction;
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_reward_curve::HalvingCurve;
use irys_storage::ii;
use irys_types::{
    app_state::DatabaseProvider, calculate_difficulty, next_cumulative_diff, validate_path,
    Address, CommitmentTransaction, Config, ConsensusConfig, DataLedger, DataTransactionHeader,
    DifficultyAdjustmentConfig, IrysBlockHeader, PoaData, H256,
};
use irys_vdf::last_step_checkpoints_is_valid;
use irys_vdf::state::VdfStateReadonly;
use itertools::*;
use openssl::sha;
use reth::revm::primitives::B256;
use reth::rpc::api::EngineApiClient as _;
use reth::rpc::types::engine::ExecutionPayload;
use reth_ethereum_primitives::Block;
use tracing::{debug, error, info};

/// Trait for providing execution payloads for block validation
#[async_trait]
pub trait PayloadProvider: Clone + Send + Sync + 'static {
    /// Waits for the execution payload to arrive over gossip. This method will first check the local
    /// cache, then try to retrieve the payload from the network if it is not found locally.
    async fn wait_for_payload(&self, evm_block_hash: &B256) -> Option<ExecutionData>;
}

/// Full pre-validation steps for a block
pub async fn prevalidate_block(
    block: IrysBlockHeader,
    previous_block: IrysBlockHeader,
    parent_epoch_snapshot: Arc<EpochSnapshot>,
    config: Config,
    reward_curve: Arc<HalvingCurve>,
    parent_ema_snapshot: &EmaSnapshot,
) -> eyre::Result<()> {
    debug!(
        block_hash = ?block.block_hash.0.to_base58(),
        ?block.height,
        "Prevalidating block",
    );

    let poa_chunk: Vec<u8> = match &block.poa.chunk {
        Some(chunk) => chunk.clone().into(),
        None => return Err(eyre::eyre!("Missing PoA chunk to be pre validated")),
    };

    if block.chunk_hash != sha::sha256(&poa_chunk).into() {
        return Err(eyre::eyre!(
            "Invalid block: chunk hash distinct from PoA chunk hash"
        ));
    }

    // Check prev_output (vdf)
    prev_output_is_valid(&block, &previous_block)?;
    debug!(
        block_hash = ?block.block_hash.0.to_base58(),
        ?block.height,
        "prev_output_is_valid",
    );

    // Check the difficulty
    difficulty_is_valid(
        &block,
        &previous_block,
        &config.consensus.difficulty_adjustment,
    )?;

    debug!(
        block_hash = ?block.block_hash.0.to_base58(),
        ?block.height,
        "difficulty_is_valid",
    );

    // Check the cumulative difficulty
    cumulative_difficulty_is_valid(&block, &previous_block)?;
    debug!(
        block_hash = ?block.block_hash.0.to_base58(),
        ?block.height,
        "cumulative_difficulty_is_valid",
    );

    check_poa_data_expiration(&block.poa, parent_epoch_snapshot.clone())?;
    debug!("poa data not expired");

    // Check the solution_hash
    solution_hash_is_valid(&block, &previous_block)?;
    debug!(
        block_hash = ?block.block_hash.0.to_base58(),
        ?block.height,
        "solution_hash_is_valid",
    );

    // We only check last_step_checkpoints during pre-validation
    last_step_checkpoints_is_valid(&block.vdf_limiter_info, &config.consensus.vdf).await?;

    // Check that the oracle price does not exceed the EMA pricing parameters
    let oracle_price_valid = EmaSnapshot::oracle_price_is_valid(
        block.oracle_irys_price,
        previous_block.oracle_irys_price,
        config.consensus.token_price_safe_range,
    );
    ensure!(oracle_price_valid, "Oracle price must be valid");

    // Check that the EMA has been correctly calculated
    let ema_valid = {
        let res = parent_ema_snapshot
            .calculate_ema_for_new_block(
                &previous_block,
                block.oracle_irys_price,
                config.consensus.token_price_safe_range,
                config.consensus.ema.price_adjustment_interval,
            )
            .ema;
        res == block.ema_irys_price
    };
    ensure!(ema_valid, "EMA must be valid");

    // Check valid curve price
    let reward = reward_curve.reward_between(
        // adjust ms to sec
        previous_block.timestamp.saturating_div(1000),
        block.timestamp.saturating_div(1000),
    )?;
    let encoded_reward = block.reward_amount;
    ensure!(
        reward.amount == block.reward_amount,
        "reward amount mismatch, expected {reward:}, got {encoded_reward:}"
    );

    // After pre-validating a bunch of quick checks we validate the signature
    // TODO: We may want to further check if the signer is a staked address
    // this is a little more advanced though as it requires knowing what the
    // commitment states looked like when this block was produced. For now
    // we just accept any valid signature.
    ensure!(block.is_signature_valid(), "block signature is not valid");

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
            "vdf_limiter.prev_output ({}) does not match previous blocks vdf_limiter.output ({})",
            &block.vdf_limiter_info.prev_output,
            &previous_block.vdf_limiter_info.output
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
        difficulty_config,
    );

    if diff == block.diff {
        Ok(())
    } else {
        Err(eyre::eyre!(
            "Invalid difficulty (expected {} got {})",
            &diff,
            &block.diff
        ))
    }
}

/// Checks PoA data chunk data solution partitions has not expired
pub fn check_poa_data_expiration(
    poa: &PoaData,
    epoch_snapshot: Arc<EpochSnapshot>,
) -> eyre::Result<()> {
    let is_data_partition_assigned = epoch_snapshot
        .partition_assignments
        .data_partitions
        .contains_key(&poa.partition_hash);

    // if is a data chunk
    if poa.data_path.is_some()
        && poa.tx_path.is_some()
        && poa.ledger_id.is_some()
        && !is_data_partition_assigned
    {
        return Err(eyre::eyre!(
            "Invalid data PoA, partition hash is not a data partition, it may have expired"
        ));
    };
    Ok(())
}

/// Validates if a block's cumulative difficulty equals the previous cumulative difficulty
/// plus the expected hashes from its new difficulty. Returns Ok if valid.
///
/// Note: Requires valid block difficulty - call `difficulty_is_valid()` first.
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
        Err(eyre::eyre!(
            "Invalid cumulative_difficulty (expected {}, got {})",
            &cumulative_diff,
            &block.cumulative_diff
        ))
    }
}

/// Checks to see if the `solution_hash` exceeds the difficulty threshold
/// of the previous block
///
/// Note: Requires valid block difficulty - call `difficulty_is_valid()` first.
pub fn solution_hash_is_valid(
    block: &IrysBlockHeader,
    previous_block: &IrysBlockHeader,
) -> eyre::Result<()> {
    let solution_hash = block.solution_hash;
    let solution_diff = hash_to_number(&solution_hash.0);

    if solution_diff >= previous_block.diff {
        Ok(())
    } else {
        Err(eyre::eyre!(
            "Invalid solution_hash - expected difficulty >={}, got {} (diff: {})",
            &previous_block.diff,
            &solution_diff,
            &previous_block.diff.abs_diff(solution_diff)
        ))
    }
}

/// Returns Ok if the vdf recall range in the block is valid
pub async fn recall_recall_range_is_valid(
    block: &IrysBlockHeader,
    config: &ConsensusConfig,
    steps_guard: &VdfStateReadonly,
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
    config: &ConsensusConfig,
    steps_guard: &VdfStateReadonly,
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
        partition_hash,
    )
}

/// Returns Ok if the provided `PoA` is valid, Err otherwise
#[tracing::instrument(skip_all, fields(
    ?miner_address,
    chunk_offset = ?poa.partition_chunk_offset,
    partition_hash = ?poa.partition_hash,
    entropy_packing_iterations = ?config.entropy_packing_iterations,
    chunk_size = ?config.chunk_size
), err)]

pub fn poa_is_valid(
    poa: &PoaData,
    block_index_guard: &BlockIndexReadGuard,
    epoch_snapshot: &EpochSnapshot,
    config: &ConsensusConfig,
    miner_address: &Address,
) -> eyre::Result<()> {
    debug!("PoA validating");
    let mut poa_chunk: Vec<u8> = match &poa.chunk {
        Some(chunk) => chunk.clone().into(),
        None => return Err(eyre::eyre!("Missing PoA chunk to be validated")),
    };
    // data chunk
    if let (Some(data_path), Some(tx_path), Some(ledger_id)) =
        (poa.data_path.clone(), poa.tx_path.clone(), poa.ledger_id)
    {
        // partition data -> ledger data
        let partition_assignment = epoch_snapshot
            .get_data_partition_assignment(poa.partition_hash)
            .unwrap();

        let ledger_chunk_offset = partition_assignment.slot_index.unwrap() as u64
            * config.num_partitions_per_slot
            * config.num_chunks_in_partition
            + poa.partition_chunk_offset as u64;

        // ledger data -> block
        let ledger = DataLedger::try_from(ledger_id).unwrap();

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
            config.chain_id,
        );

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
            config.chain_id,
        );
        if entropy_chunk != poa_chunk {
            if poa_chunk.len() <= 32 {
                debug!("Chunk PoA:{:?}", poa_chunk);
                debug!("Entropy  :{:?}", entropy_chunk);
            }
            return Err(eyre::eyre!(
                "PoA capacity chunk mismatch {:?} /= {:?}",
                entropy_chunk.first(),
                poa_chunk.first()
            ));
        }
    }
    Ok(())
}

/// Validates that the shadow transactions in the EVM block match the expected shadow transactions
/// generated from the Irys block data.
pub async fn shadow_transactions_are_valid(
    config: &Config,
    service_senders: &ServiceSenders,
    block: &IrysBlockHeader,
    reth_adapter: &IrysRethNodeAdapter,
    db: &DatabaseProvider,
    payload_provider: impl PayloadProvider,
) -> eyre::Result<()> {
    // 1. Validate that the evm block is valid
    let payload = payload_provider
        .wait_for_payload(&block.evm_block_hash)
        .await
        .ok_or_eyre("reth execution payload never arrived")?;

    let engine_api_client = reth_adapter.inner.engine_http_client();
    let ExecutionData { payload, sidecar } = payload;

    let ExecutionPayload::V3(payload) = payload else {
        eyre::bail!("irys-reth expects that all payloads are of v3 type");
    };
    ensure!(
        payload.withdrawals().is_empty(),
        "withdrawals must always be empty"
    );

    let versioned_hashes = sidecar
        .versioned_hashes()
        .ok_or_eyre("version hashes must be present")?
        .clone();
    loop {
        let payload = engine_api_client
            .new_payload_v4(
                payload.clone(),
                versioned_hashes.clone(),
                block.previous_block_hash.into(),
                RequestsOrHash::Requests(Requests::new(vec![])),
            )
            .await?;
        match payload.status {
            alloy_rpc_types_engine::PayloadStatusEnum::Invalid { validation_error } => {
                return Err(eyre::Report::msg(validation_error))
            }
            alloy_rpc_types_engine::PayloadStatusEnum::Syncing => {
                tracing::debug!("syncing extra blocks to validate payload");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
            alloy_rpc_types_engine::PayloadStatusEnum::Valid => {
                tracing::info!("reth payload already known & is valid");
                break;
            }
            alloy_rpc_types_engine::PayloadStatusEnum::Accepted => {
                tracing::info!("accepted a side-chain (fork) payload");
                break;
            }
        }
    }
    let evm_block: Block = payload.try_into_block()?;

    // 2. Extract shadow transactions from the beginning of the block
    let mut expect_shadow_txs = true;
    let actual_shadow_txs = evm_block
        .body
        .transactions
        .into_iter()
        .map(|tx| {
            if expect_shadow_txs {
                let shadow_tx = ShadowTransaction::decode(&mut tx.input().as_ref());
                let tx_signer = tx.into_signed().recover_signer()?;
                let Ok(shadow_tx) = shadow_tx else {
                    // after reaching first non-shadow tx, we scan the rest of the
                    // txs to check if we don't have any stray shadow txs in there
                    expect_shadow_txs = false;
                    return Ok(None);
                };

                ensure!(
                    block.miner_address == tx_signer,
                    "Shadow tx signer is not the miner"
                );
                Ok(Some(shadow_tx))
            } else {
                // ensure that no other shadow txs are present in the block
                let shadow_tx = ShadowTransaction::decode(&mut tx.input().as_ref());
                ensure!(
                    shadow_tx.is_err(),
                    "shadow tx injected in the middle of the block"
                );
                Ok(None)
            }
        })
        .filter_map(std::result::Result::transpose);

    // 3. Generate expected shadow transactions
    let expected_txs =
        generate_expected_shadow_transactions_from_db(config, service_senders, block, db).await?;

    // 4. Validate they match
    validate_shadow_transactions_match(actual_shadow_txs, expected_txs.into_iter())
}

/// Generates expected shadow transactions by looking up required data from the mempool or database
#[tracing::instrument(skip_all, err)]
async fn generate_expected_shadow_transactions_from_db<'a>(
    config: &Config,
    service_senders: &ServiceSenders,
    block: &'a IrysBlockHeader,
    db: &DatabaseProvider,
) -> eyre::Result<Vec<ShadowTransaction>> {
    // Look up previous block to get EVM hash
    let prev_block = {
        let (tx_prev, rx_prev) = tokio::sync::oneshot::channel();
        service_senders
            .mempool
            .send(MempoolServiceMessage::GetBlockHeader(
                block.previous_block_hash,
                false,
                tx_prev,
            ))?;
        match rx_prev.await? {
            Some(h) => h,
            None => db
                .view_eyre(|tx| block_header_by_hash(tx, &block.previous_block_hash, false))?
                .ok_or_eyre("Previous block not found")?,
        }
    };

    // Look up commitment txs
    let commitment_txs = extract_commitment_txs(config, service_senders, block, db).await?;

    // Lookup data txs
    let data_txs = extract_data_txs(service_senders, block, db).await?;

    let shadow_txs = ShadowTxGenerator::new(
        &block.height,
        &block.reward_address,
        &block.reward_amount,
        &prev_block,
    );
    let shadow_txs = shadow_txs
        .generate_all(&commitment_txs, &data_txs)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(shadow_txs)
}

async fn extract_commitment_txs(
    config: &Config,
    service_senders: &ServiceSenders,
    block: &IrysBlockHeader,
    db: &DatabaseProvider,
) -> Result<Vec<CommitmentTransaction>, eyre::Error> {
    let is_epoch_block = block.height % config.consensus.epoch.num_blocks_in_epoch == 0;
    let commitment_txs = if is_epoch_block {
        // IMPORTANT: on epoch blocks we don't generate shadow txs for commitment txs
        vec![]
    } else {
        match &block.system_ledgers[..] {
            [ledger] => {
                ensure!(
                    ledger.ledger_id == SystemLedger::Commitment,
                    "only commitment ledger supported"
                );

                get_commitment_tx_in_parallel(ledger.tx_ids.0.clone(), &service_senders.mempool, db)
                    .await?
            }
            [] => {
                // this is valid as we can have a block that contains 0 system ledgers
                vec![]
            }
            // this is to ensure that we don't skip system ledgers and forget to add them to validation in the future
            [..] => eyre::bail!("Currently we support at most 1 system ledger per block"),
        }
    };
    Ok(commitment_txs)
}

async fn extract_data_txs(
    service_senders: &ServiceSenders,
    block: &IrysBlockHeader,
    db: &DatabaseProvider,
) -> Result<Vec<DataTransactionHeader>, eyre::Error> {
    let txs = match &block.data_ledgers[..] {
        [publish_ledger, submit_ledger] => {
            ensure!(
                publish_ledger.ledger_id == DataLedger::Publish,
                "Publish ledger must be the first ledger in the data ledgers"
            );
            ensure!(
                submit_ledger.ledger_id == DataLedger::Submit,
                "Submit ledger must be the second ledger in the data ledgers"
            );
            // we only access the submit ledger data. Publish ledger does not require billing the user extra
            get_data_tx_in_parallel(submit_ledger.tx_ids.0.clone(), &service_senders.mempool, db)
                .await?
        }
        // this is to ensure that we don't skip system ledgers and forget to add them to validation in the future
        [..] => eyre::bail!("Expect exactly 2 data ledgers to be present on the block"),
    };
    Ok(txs)
}

/// Validates  the actual shadow transactions match the expected ones
#[tracing::instrument(skip_all, err)]
fn validate_shadow_transactions_match(
    actual: impl Iterator<Item = eyre::Result<ShadowTransaction>>,
    expected: impl Iterator<Item = ShadowTransaction>,
) -> eyre::Result<()> {
    // Validate each expected shadow transaction
    for (idx, data) in actual.zip_longest(expected).enumerate() {
        let EitherOrBoth::Both(actual, expected) = data else {
            // If either of the shadow txs is not present, it means it was not generated as `expected`
            // or it was not included in the block. either way - an error
            tracing::warn!(?data, "shadow tx len mismatch");
            eyre::bail!("actual and expected shadow txs lens differ");
        };
        let actual = actual?;
        ensure!(
            actual == expected,
            "Shadow transaction mismatch at idx {}. expected {:?}, got {:?}",
            idx,
            expected,
            actual
        );
    }

    Ok(())
}

pub fn is_seed_data_valid(
    block_header: &IrysBlockHeader,
    previous_block_header: &IrysBlockHeader,
    reset_frequency: u64,
) -> ValidationResult {
    let vdf_info = &block_header.vdf_limiter_info;
    let expected_seed_data = vdf_info.calculate_seeds(reset_frequency, previous_block_header);

    // TODO: difficulty validation adjustment is likely needs to be done here too,
    //  but difficulty is not yet implemented
    let are_seeds_valid =
        expected_seed_data.0 == vdf_info.next_seed && expected_seed_data.1 == vdf_info.seed;
    if are_seeds_valid {
        ValidationResult::Valid
    } else {
        error!(
            "Seed data is invalid. Expected: {:?}, got: {:?}",
            expected_seed_data, vdf_info
        );
        ValidationResult::Invalid
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        block_index_service::{BlockIndexService, GetBlockIndexGuardMessage},
        BlockFinalizedMessage,
    };
    use actix::{prelude::*, SystemRegistry};
    use irys_config::StorageSubmodulesConfig;
    use irys_database::add_genesis_commitments;
    use irys_domain::{BlockIndex, EpochSnapshot};
    use irys_testing_utils::utils::temporary_directory;
    use irys_types::{
        irys::IrysSigner, partition::PartitionAssignment, Address, Base64, DataTransaction,
        DataTransactionHeader, DataTransactionLedger, H256List, NodeConfig, Signature, H256, U256,
    };
    use std::sync::{Arc, RwLock};
    use tempfile::TempDir;
    use tracing::{debug, info};

    use super::*;

    pub(super) struct TestContext {
        pub block_index: Arc<RwLock<BlockIndex>>,
        pub block_index_actor: Addr<BlockIndexService>,
        pub miner_address: Address,
        pub epoch_snapshot: EpochSnapshot,
        pub partition_hash: H256,
        pub partition_assignment: PartitionAssignment,
        pub consensus_config: ConsensusConfig,
        #[expect(dead_code)]
        pub node_config: NodeConfig,
    }

    async fn init() -> (TempDir, TestContext) {
        let data_dir = temporary_directory(Some("block_validation_tests"), false);
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 100,
                ..ConsensusConfig::testnet()
            }),
            base_directory: data_dir.path().to_path_buf(),
            ..NodeConfig::testnet()
        };
        let config = Config::new(node_config);

        let mut genesis_block = IrysBlockHeader::new_mock_header();
        genesis_block.height = 0;
        let chunk_size = 32;
        let mut node_config = NodeConfig::testnet();
        node_config.storage.num_writes_before_sync = 1;
        node_config.base_directory = data_dir.path().to_path_buf();
        let consensus_config = ConsensusConfig {
            chunk_size,
            num_chunks_in_partition: 10,
            num_chunks_in_recall_range: 2,
            num_partitions_per_slot: 1,
            entropy_packing_iterations: 1_000,
            block_migration_depth: 1,
            ..node_config.consensus_config()
        };

        let commitments = add_genesis_commitments(&mut genesis_block, &config);

        let arc_genesis = Arc::new(genesis_block.clone());
        let signer = config.irys_signer();
        let miner_address = signer.address();

        // Create epoch service with random miner address
        let block_index = Arc::new(RwLock::new(BlockIndex::new(&node_config).await.unwrap()));

        let block_index_actor =
            BlockIndexService::new(block_index.clone(), &consensus_config).start();
        SystemRegistry::set(block_index_actor.clone());

        let storage_submodules_config =
            StorageSubmodulesConfig::load(config.node_config.base_directory.clone()).unwrap();

        // Create an epoch snapshot for the genesis block
        let epoch_snapshot = EpochSnapshot::new(
            &storage_submodules_config,
            genesis_block,
            commitments.clone(),
            &config,
        );
        info!("Genesis Epoch tasks complete.");

        let partition_hash = epoch_snapshot.ledgers.get_slots(DataLedger::Submit)[0].partitions[0];

        let msg = BlockFinalizedMessage {
            block_header: arc_genesis.clone(),
            all_txs: Arc::new(vec![]),
        };

        let block_index_actor = BlockIndexService::from_registry();
        match block_index_actor.send(msg).await {
            Ok(_) => info!("Genesis block indexed"),
            Err(_) => panic!("Failed to index genesis block"),
        }

        let partition_assignment = epoch_snapshot
            .get_data_partition_assignment(partition_hash)
            .unwrap();

        debug!("Partition assignment {:?}", partition_assignment);

        (
            data_dir,
            TestContext {
                block_index,
                block_index_actor,
                miner_address,
                epoch_snapshot,
                partition_hash,
                partition_assignment,
                consensus_config,
                node_config,
            },
        )
    }

    #[actix::test]
    async fn poa_test_3_complete_txs() {
        let (_tmp, context) = init().await;
        // Create a bunch of TX chunks
        let data_chunks = vec![
            vec![[0; 32], [1; 32], [2; 32]], // tx0
            vec![[3; 32], [4; 32], [5; 32]], // tx1
            vec![[6; 32], [7; 32], [8; 32]], // tx2
        ];

        // Create a bunch of signed TX from the chunks
        // Loop though all the data_chunks and create wrapper tx for them
        let signer = IrysSigner::random_signer(&context.consensus_config);
        let mut txs: Vec<DataTransaction> = Vec::new();

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
                    context.consensus_config.chunk_size as usize,
                )
                .await;
            }
        }
    }

    #[actix::test]
    async fn poa_not_complete_last_chunk_test() {
        let (_tmp, context) = init().await;

        // Create a signed TX from the chunks
        let signer = IrysSigner::random_signer(&context.consensus_config);
        let mut txs: Vec<DataTransaction> = Vec::new();

        let data = vec![3; 40]; //32 + 8 last incomplete chunk
        let tx = signer.create_transaction(data.clone(), None).unwrap();
        let tx = signer.sign_transaction(tx).unwrap();
        txs.push(tx);

        let poa_tx_num = 0;
        let chunk_size = context.consensus_config.chunk_size as usize;
        for poa_chunk_num in 0..2 {
            let mut poa_chunk: Vec<u8> = data[poa_chunk_num * (chunk_size)
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

    #[actix::test]
    async fn is_seed_data_valid_should_validate_seeds() {
        let reset_frequency = 2;

        let mut parent_header = IrysBlockHeader::new_mock_header();
        let parent_seed = BlockHash::from_slice(&[2; 32]);
        let parent_next_seed = BlockHash::from_slice(&[3; 32]);
        parent_header.block_hash = BlockHash::from_slice(&[4; 32]);
        parent_header.vdf_limiter_info.seed = parent_seed;
        parent_header.vdf_limiter_info.next_seed = parent_next_seed;

        let mut header_2 = IrysBlockHeader::new_mock_header();
        // Reset frequency is 2, so setting global_step_number to 3 and adding 2 steps
        //  should result in the seeds being rotated
        header_2.vdf_limiter_info.global_step_number = 3;
        header_2.vdf_limiter_info.steps = H256List(vec![H256::zero(); 2]);
        header_2
            .vdf_limiter_info
            .set_seeds(reset_frequency, &parent_header);
        let is_valid = is_seed_data_valid(&header_2, &parent_header, reset_frequency);

        assert_eq!(
            header_2.vdf_limiter_info.next_seed,
            parent_header.block_hash
        );
        assert_eq!(header_2.vdf_limiter_info.seed, parent_next_seed);
        assert!(
            matches!(is_valid, ValidationResult::Valid),
            "Seed data should be valid"
        );

        // Now let's try to rotate the seeds when no rotation is needed by increasing the
        // reset frequency
        let large_reset_frequency = 100;
        let is_valid = is_seed_data_valid(&header_2, &parent_header, large_reset_frequency);
        assert!(
            matches!(is_valid, ValidationResult::Invalid),
            "Seed data should still be valid"
        );

        // Now let's try to set some random seeds that are not valid
        header_2.vdf_limiter_info.seed = BlockHash::from_slice(&[5; 32]);
        header_2.vdf_limiter_info.next_seed = BlockHash::from_slice(&[6; 32]);
        let is_valid = is_seed_data_valid(&header_2, &parent_header, reset_frequency);

        assert!(
            matches!(is_valid, ValidationResult::Invalid),
            "Seed data should be invalid"
        );
    }

    async fn poa_test(
        context: &TestContext,
        txs: &[DataTransaction],
        #[expect(
            clippy::ptr_arg,
            reason = "we need to clone this so it needs to be a Vec"
        )]
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
            context.consensus_config.entropy_packing_iterations,
            chunk_size,
            &mut entropy_chunk,
            context.consensus_config.chain_id,
        );

        xor_vec_u8_arrays_in_place(poa_chunk, &entropy_chunk);

        // Create vectors of tx headers and txids
        let tx_headers: Vec<DataTransactionHeader> =
            txs.iter().map(|tx| tx.header.clone()).collect();

        let data_tx_ids = tx_headers.iter().map(|h| h.id).collect::<Vec<H256>>();

        let (tx_root, tx_path) = DataTransactionLedger::merklize_tx_root(&tx_headers);

        let poa = PoaData {
            tx_path: Some(Base64(tx_path[poa_tx_num].proof.clone())),
            data_path: Some(Base64(txs[poa_tx_num].proofs[poa_chunk_num].proof.clone())),
            chunk: Some(Base64(poa_chunk.clone())),
            ledger_id: Some(1),
            partition_chunk_offset: (poa_tx_num * 3 /* 3 chunks in each tx */ + poa_chunk_num)
                .try_into()
                .expect("Value exceeds u32::MAX"),
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
            data_ledgers: vec![
                // Permanent Publish Ledger
                DataTransactionLedger {
                    ledger_id: DataLedger::Publish.into(),
                    tx_root: H256::zero(),
                    tx_ids: H256List(Vec::new()),
                    max_chunk_offset: 0,
                    expires: None,
                    proofs: None,
                },
                // Term Submit Ledger
                DataTransactionLedger {
                    ledger_id: DataLedger::Submit.into(),
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
            * context.consensus_config.num_partitions_per_slot
            * context.consensus_config.num_chunks_in_partition
            + (poa_tx_num * 3 /* 3 chunks in each tx */ + poa_chunk_num) as u64;

        assert_eq!(
            ledger_chunk_offset,
            (poa_tx_num * 3 /* 3 chunks in each tx */ + poa_chunk_num) as u64,
            "ledger_chunk_offset mismatch"
        );

        // ledger data -> block
        let bb = block_index_guard
            .read()
            .get_block_bounds(DataLedger::Submit, ledger_chunk_offset);
        info!("block bounds: {:?}", bb);

        assert_eq!(bb.start_chunk_offset, 0, "start_chunk_offset should be 0");
        assert_eq!(
            bb.end_chunk_offset, total_chunks_in_tx as u64,
            "end_chunk_offset should be 9, tx has 9 chunks"
        );

        let poa_valid = poa_is_valid(
            &poa,
            &block_index_guard,
            &context.epoch_snapshot,
            &context.consensus_config,
            &context.miner_address,
        );

        assert!(poa_valid.is_ok(), "PoA should be valid");
    }
}
