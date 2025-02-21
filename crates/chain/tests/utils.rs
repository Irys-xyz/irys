use futures::future::select;
use irys_actors::block_producer::SolutionFoundMessage;
use irys_actors::block_validation;
use irys_chain::IrysNodeCtx;
use irys_packing::capacity_single::compute_entropy_chunk;
use irys_packing::unpack;
use irys_storage::ii;
use irys_types::{
    block_production::Seed, block_production::SolutionContext, Address, H256List, H256,
};
use irys_types::{Config, StorageConfig, TxChunkOffset, VDFStepsConfig};
use irys_vdf::vdf_state::VdfStepsReadGuard;
use irys_vdf::{step_number_to_salt_number, vdf_sha};
use reth::rpc::types::engine::ExecutionPayloadEnvelopeV1Irys;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::{future::Future, time::Duration};
use tokio::time::sleep;
use tracing::debug;
use tracing::info;

use std::collections::HashMap;

use actix_web::{
    dev::{Service, ServiceResponse},
    test,
};
use awc::{body::MessageBody, http::StatusCode};
use irys_database::{tables::IrysBlockHeaders, Ledger};
use irys_types::{
    Base64, DatabaseProvider, IrysBlockHeader, IrysTransaction, LedgerChunkOffset, PackedChunk,
    UnpackedChunk,
};
use reth_db::cursor::*;
use reth_db::Database;
use tracing::error;

pub async fn capacity_chunk_solution(
    miner_addr: Address,
    vdf_steps_guard: VdfStepsReadGuard,
    vdf_config: &VDFStepsConfig,
    storage_config: &StorageConfig,
) -> SolutionContext {
    let max_retries = 20;
    let testnet_config = Config::testnet();
    let mut i = 1;
    let initial_step_num = vdf_steps_guard.read().global_step;
    let mut step_num: u64 = 0;
    // wait to have at least 2 new steps
    while i < max_retries && step_num < initial_step_num + 2 {
        sleep(Duration::from_secs(1)).await;
        step_num = vdf_steps_guard.read().global_step;
        i += 1;
    }

    let steps: H256List = match vdf_steps_guard.read().get_steps(ii(step_num - 1, step_num)) {
        Ok(s) => s,
        Err(err) => panic!("Not enough vdf steps {:?}, waiting...", err),
    };

    // calculate last step checkpoints
    let mut hasher = Sha256::new();
    let mut salt = irys_types::U256::from(step_number_to_salt_number(vdf_config, step_num - 1_u64));
    let mut seed = steps[0];

    let mut checkpoints: Vec<H256> = vec![H256::default(); vdf_config.num_checkpoints_in_vdf_step];

    vdf_sha(
        &mut hasher,
        &mut salt,
        &mut seed,
        vdf_config.num_checkpoints_in_vdf_step,
        vdf_config.vdf_difficulty,
        &mut checkpoints,
    );

    let partition_hash = H256::zero();
    let recall_range_idx = block_validation::get_recall_range(
        step_num,
        storage_config,
        &vdf_steps_guard,
        &partition_hash,
    )
    .unwrap();

    let mut entropy_chunk = Vec::<u8>::with_capacity(storage_config.chunk_size as usize);
    compute_entropy_chunk(
        miner_addr,
        recall_range_idx as u64 * storage_config.num_chunks_in_recall_range,
        partition_hash.into(),
        storage_config.entropy_packing_iterations,
        storage_config.chunk_size as usize, // take it from storage config
        &mut entropy_chunk,
        testnet_config.irys_chain_id,
    );

    debug!("Chunk mining address: {:?} chunk_offset: {} partition hash: {:?} iterations: {} chunk size: {}", miner_addr, 0, partition_hash, storage_config.entropy_packing_iterations, storage_config.chunk_size);

    let max: irys_types::serialization::U256 = irys_types::serialization::U256::MAX;
    let mut le_bytes = [0u8; 32];
    max.to_little_endian(&mut le_bytes);
    let solution_hash = H256(le_bytes);

    SolutionContext {
        partition_hash,
        chunk_offset: recall_range_idx as u32 * storage_config.num_chunks_in_recall_range as u32,
        mining_address: miner_addr,
        chunk: entropy_chunk,
        vdf_step: step_num,
        checkpoints: H256List(checkpoints),
        seed: Seed(steps[1]),
        solution_hash,
        ..Default::default()
    }
}

pub async fn mine_blocks(node_ctx: &IrysNodeCtx, blocks: usize) -> eyre::Result<()> {
    let storage_config = &node_ctx.storage_config;
    let vdf_config = &node_ctx.vdf_config;
    let vdf_steps_guard = node_ctx.vdf_steps_guard.clone();
    for _ in 0..blocks {
        let poa_solution = capacity_chunk_solution(
            node_ctx.config.mining_signer.address(),
            vdf_steps_guard.clone(),
            vdf_config,
            storage_config,
        )
        .await;
        let _ = node_ctx
            .actor_addresses
            .block_producer
            .send(SolutionFoundMessage(poa_solution.clone()))
            .await?
            .unwrap();
    }
    Ok(())
}

pub async fn mine_block(
    node_ctx: &IrysNodeCtx,
) -> eyre::Result<Option<(Arc<IrysBlockHeader>, ExecutionPayloadEnvelopeV1Irys)>> {
    let storage_config = &node_ctx.storage_config;
    let vdf_config = &node_ctx.vdf_config;
    let vdf_steps_guard = node_ctx.vdf_steps_guard.clone();
    let poa_solution = capacity_chunk_solution(
        node_ctx.config.mining_signer.address(),
        vdf_steps_guard.clone(),
        vdf_config,
        storage_config,
    )
    .await;
    node_ctx
        .actor_addresses
        .block_producer
        .send(SolutionFoundMessage(poa_solution.clone()))
        .await?
}

/// Waits for the provided future to resolve, and if it doesn't after `timeout_duration`,
/// triggers the building/mining of a block, and then waits again.
/// designed for use with calls that expect to be able to send and confirm a tx in a single exposed future
pub async fn future_or_mine_on_timeout<F, T>(
    node_ctx: IrysNodeCtx,
    mut future: F,
    timeout_duration: Duration,
    vdf_steps_guard: VdfStepsReadGuard,
    vdf_config: &VDFStepsConfig,
    storage_config: &StorageConfig,
) -> eyre::Result<T>
where
    F: Future<Output = T> + Unpin,
{
    loop {
        let poa_solution = capacity_chunk_solution(
            node_ctx.config.mining_signer.address(),
            vdf_steps_guard.clone(),
            vdf_config,
            storage_config,
        )
        .await;
        let race = select(&mut future, Box::pin(sleep(timeout_duration))).await;
        match race {
            // provided future finished
            futures::future::Either::Left((res, _)) => return Ok(res),
            // we need another block
            futures::future::Either::Right(_) => {
                info!("deployment timed out, creating new block..")
            }
        };

        let _ = node_ctx
            .actor_addresses
            .block_producer
            .send(SolutionFoundMessage(poa_solution.clone()))
            .await?
            .unwrap();
    }
}

/// Helper function for testing chunk uploads. Posts a single chunk of transaction data
/// to the /v1/chunk endpoint and verifies successful response.
pub async fn post_chunk<T, B>(
    app: &T,
    tx: &IrysTransaction,
    chunk_index: usize,
    chunks: &Vec<[u8; 32]>,
) where
    T: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
{
    let chunk = UnpackedChunk {
        data_root: tx.header.data_root,
        data_size: tx.header.data_size,
        data_path: Base64(tx.proofs[chunk_index].proof.to_vec()),
        bytes: Base64(chunks[chunk_index].to_vec()),
        tx_offset: TxChunkOffset::from(chunk_index as u32),
    };

    let resp = test::call_service(
        app,
        test::TestRequest::post()
            .uri("/v1/chunk")
            .set_json(&chunk)
            .to_request(),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
}

/// Retrieves a ledger chunk via HTTP GET request using the actix-web test framework.
///
/// # Arguments
/// * `app` - The actix-web service
/// * `ledger` - Target ledger
/// * `chunk_offset` - Ledger relative chunk offset
///
/// Returns `Some(PackedChunk)` if found (HTTP 200), `None` otherwise.
pub async fn get_chunk<T, B>(
    app: &T,
    ledger: Ledger,
    chunk_offset: LedgerChunkOffset,
) -> Option<PackedChunk>
where
    T: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: MessageBody,
{
    let req = test::TestRequest::get()
        .uri(&format!(
            "/v1/chunk/ledger/{}/{}",
            ledger as usize, chunk_offset
        ))
        .to_request();

    let res = test::call_service(&app, req).await;

    if res.status() == StatusCode::OK {
        let packed_chunk: PackedChunk = test::read_body_json(res).await;
        Some(packed_chunk)
    } else {
        None
    }
}

/// Finds and returns the parent block header containing a given transaction ID.
/// Takes a transaction ID, ledger type, and database connection.
/// Returns None if the transaction isn't found in any block.
pub fn get_block_parent(
    txid: H256,
    ledger: Ledger,
    db: &DatabaseProvider,
) -> Option<IrysBlockHeader> {
    let read_tx = db
        .tx()
        .map_err(|e| {
            error!("Failed to create transaction: {}", e);
        })
        .ok()?;

    let mut read_cursor = read_tx
        .new_cursor::<IrysBlockHeaders>()
        .map_err(|e| {
            error!("Failed to create cursor: {}", e);
        })
        .ok()?;

    let walker = read_cursor
        .walk(None)
        .map_err(|e| {
            error!("Failed to create walker: {}", e);
        })
        .ok()?;

    let block_headers = walker
        .collect::<Result<HashMap<_, _>, _>>()
        .map_err(|e| {
            error!("Failed to collect results: {}", e);
        })
        .ok()?;

    // Loop tough all the blocks and find the one that contains the txid
    for block_header in block_headers.values() {
        if block_header.ledgers[ledger].tx_ids.0.contains(&txid) {
            return Some(IrysBlockHeader::from(block_header.clone()));
        }
    }

    None
}

/// Verifies that a published chunk matches its expected content.
/// Gets a chunk from storage, unpacks it, and compares against expected bytes.
/// Panics if the chunk is not found or content doesn't match expectations.
pub async fn verify_published_chunk<T, B>(
    app: &T,
    chunk_offset: LedgerChunkOffset,
    expected_bytes: &[u8; 32],
    storage_config: &StorageConfig,
) where
    T: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: MessageBody,
{
    if let Some(packed_chunk) = get_chunk(&app, Ledger::Publish, chunk_offset).await {
        let unpacked_chunk = unpack(
            &packed_chunk,
            storage_config.entropy_packing_iterations,
            storage_config.chunk_size as usize,
            storage_config.irys_chain_id,
        );
        if unpacked_chunk.bytes.0 != expected_bytes {
            println!(
                "ledger_chunk_offset: {}\nfound: {:?}\nexpected: {:?}",
                chunk_offset, unpacked_chunk.bytes.0, expected_bytes
            )
        }
        assert_eq!(unpacked_chunk.bytes.0, expected_bytes);
    } else {
        panic!(
            "Chunk not found! Publish ledger chunk_offset: {}",
            chunk_offset
        );
    }
}
