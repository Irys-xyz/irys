use actix::MailboxError;
use futures::future::select;
use irys_actors::block_producer::SolutionFoundMessage;
use irys_actors::block_validation;
use irys_actors::mempool_service::{TxIngressError, TxIngressMessage};
use irys_chain::{start_irys_node, IrysNodeCtx};
use irys_config::IrysNodeConfig;
use irys_database::tx_header_by_txid;
use irys_packing::capacity_single::compute_entropy_chunk;
use irys_packing::unpack;
use irys_storage::ii;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_testing_utils::utils::tempfile::TempDir;
use irys_types::irys::IrysSigner;
use irys_types::{
    block_production::Seed, block_production::SolutionContext, Address, H256List, H256,
};
use irys_types::{Config, IrysTransactionHeader, StorageConfig, TxChunkOffset, VDFStepsConfig};
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
use irys_database::{tables::IrysBlockHeaders, DataLedger};
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
        testnet_config.chain_id,
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

pub struct IrysNodeTest {
    pub node_ctx: IrysNodeCtx,
    pub temp_dir: TempDir,
}

// Reasons tx could fail to be added to mempool
#[derive(Debug)]
pub enum AddTxError {
    CreateTx(eyre::Report),
    TxIngress(TxIngressError),
    Mailbox(MailboxError),
}

impl IrysNodeTest {
    pub async fn new(name: &str) -> Self {
        let (node_ctx, temp_dir) = start_node(name).await;
        Self { node_ctx, temp_dir }
    }

    pub async fn new_with_config(
        name: &str,
        config: Option<Config>,
        node_config: Option<IrysNodeConfig>,
    ) -> Self {
        let (node_ctx, temp_dir) = start_node_config(name, config, node_config).await;
        Self { node_ctx, temp_dir }
    }

    pub async fn wait_until_height(&self, target_height: u64, max_seconds: usize) {
        let mut retries = 0;
        let max_retries = max_seconds; // 1 second per retry
        while self.node_ctx.block_index_guard.read().latest_height() < target_height && retries < max_retries
        {
            sleep(Duration::from_secs(1)).await;
            retries += 1;
        }
        if retries == max_retries {
            panic!("Failed to reach target height after {} retries", retries);
        } else {
            info!(
                "got block after {} seconds and {} retries",
                max_seconds, &retries
            );
        }
    }
    
    pub fn get_height(&self) -> u64 {
        self.node_ctx.block_index_guard.read().latest_height()
    }
    
    pub async fn mine_block(&self) -> eyre::Result<()> {
        self.mine_blocks(1).await?;
        Ok(())
    }
    
    pub async fn mine_blocks(&self, num_blocks: usize) -> eyre::Result<()> {
        let height = self.get_height();
        self.node_ctx.actor_addresses.set_mining(true)?;
        self.wait_until_height( height + num_blocks as u64, 60 * num_blocks).await;
        self.node_ctx.actor_addresses.set_mining(false)?;
        Ok(())
    }

    pub async fn create_submit_data_tx(
        &self,
        account: &IrysSigner,
        data: Vec<u8>,
    ) -> Result<IrysTransaction, AddTxError> {
        let tx = account
            .create_transaction(data, None)
            .map_err(AddTxError::CreateTx)?;
        let tx = account.sign_transaction(tx).map_err(AddTxError::CreateTx)?;
    
        match self.node_ctx
            .actor_addresses
            .mempool
            .send(TxIngressMessage(tx.header.clone()))
            .await
        {
            Ok(Ok(())) => return Ok(tx),
            Ok(Err(tx_error)) => return Err(AddTxError::TxIngress(tx_error)),
            Err(e) => return Err(AddTxError::Mailbox(e)),
        };
    }
    
    pub fn get_tx_header(&self, tx_id: &H256) -> eyre::Result<IrysTransactionHeader> {
        match self.node_ctx.db.view_eyre(|tx| tx_header_by_txid(tx, tx_id)) {
            Ok(Some(tx_header)) => Ok(tx_header),
            Ok(None) => Err(eyre::eyre!("No tx header found for txid {:?}", tx_id)),
            Err(e) => Err(eyre::eyre!("Failed to collect tx header: {}", e)),
        }
    }
    
    pub fn get_block_by_height(
        &self,
        height: u64,
        include_chunk: bool,
    ) -> eyre::Result<IrysBlockHeader> {
        if let Some(block) = self.node_ctx.block_index_guard.read().get_item(height as usize) {
            match &self.node_ctx.db.view_eyre(|tx| {
                irys_database::block_header_by_hash(tx, &block.block_hash, include_chunk)
            })? {
                Some(db_irys_block) => Ok(db_irys_block.clone()),
                None => Err(eyre::eyre!("Block at height {} not found", height)),
            }
        } else {
            Err(eyre::eyre!("Block item not found at height {}", height))
        }
    }    

    pub fn get_block_by_hash(
        &self,
        hash: &H256,
        include_chunk: bool,
    ) -> eyre::Result<IrysBlockHeader> {
        match &self.node_ctx.db.view_eyre(|tx| {
            irys_database::block_header_by_hash(tx, hash, include_chunk)
        })? {
            Some(db_irys_block) => Ok(db_irys_block.clone()),
            None => Err(eyre::eyre!("Block with hash {} not found", hash)),
        }
    }    

    pub async fn stop(self) {
        self.node_ctx.stop().await;
    }
}

pub async fn start_node(name: &str) -> (IrysNodeCtx, TempDir) {
    start_node_config(name, None, None).await
}

/// need to return tmp_dir to prevent it from being dropped
pub async fn start_node_config(
    name: &str,
    config: Option<Config>,
    node_config: Option<IrysNodeConfig>,
) -> (IrysNodeCtx, TempDir) {
    std::env::set_var("RUST_LOG", "debug");
    let temp_dir = setup_tracing_and_temp_dir(Some(name), false);
    let testnet_config = config.unwrap_or_else(|| Config::testnet());
    let storage_config = StorageConfig::new(&testnet_config);
    let mut config = node_config.unwrap_or_else(|| IrysNodeConfig::new(&testnet_config));
    config.base_directory = temp_dir.path().to_path_buf();
    (
        start_irys_node(config, storage_config, testnet_config.clone())
            .await
            .unwrap(),
        temp_dir,
    )
}

pub async fn mine_blocks(node_ctx: &IrysNodeCtx, blocks: usize) -> eyre::Result<()> {
    for _ in 0..blocks {
        mine_block(node_ctx).await?.unwrap();
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
        node_ctx.node_config.mining_signer.address(),
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
            node_ctx.node_config.mining_signer.address(),
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
    ledger: DataLedger,
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
    ledger: DataLedger,
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
        if block_header.data_ledgers[ledger].tx_ids.0.contains(&txid) {
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
    if let Some(packed_chunk) = get_chunk(&app, DataLedger::Publish, chunk_offset).await {
        let unpacked_chunk = unpack(
            &packed_chunk,
            storage_config.entropy_packing_iterations,
            storage_config.chunk_size as usize,
            storage_config.chain_id,
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
