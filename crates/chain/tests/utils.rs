use actix::MailboxError;
use actix_web::middleware::Logger;
use actix_web::App;
use base58::ToBase58;
use futures::future::select;
use irys_actors::{
    block_producer::SolutionFoundMessage,
    block_tree_service::get_canonical_chain,
    block_validation,
    mempool_service::{TxIngressError, TxIngressMessage},
    packing::wait_for_packing,
    vdf_service::VdfStepsReadGuard,
    SetTestBlocksRemainingMessage,
};
use irys_api_server::{create_listener, routes};
use irys_chain::{IrysNode, IrysNodeCtx};
use irys_database::tx_header_by_txid;
use irys_packing::capacity_single::compute_entropy_chunk;
use irys_packing::unpack;
use irys_storage::ii;
use irys_testing_utils::utils::tempfile::TempDir;
use irys_testing_utils::utils::temporary_directory;
use irys_types::irys::IrysSigner;
use irys_types::{
    block_production::Seed, block_production::SolutionContext, Address, DataLedger, H256List, H256,
};
use irys_types::{
    CommitmentTransaction, Config, IrysTransactionHeader, NodeConfig, NodeMode, TxChunkOffset,
};
use irys_vdf::{step_number_to_salt_number, vdf_sha};
use reth::rpc::types::engine::ExecutionPayloadEnvelopeV1Irys;
use reth_primitives::irys_primitives::CommitmentType;
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::{future::Future, time::Duration};
use tokio::time::sleep;
use tracing::info;

use std::collections::HashMap;

use actix_web::{
    dev::{Service, ServiceResponse},
    test,
};
use awc::{body::MessageBody, http::StatusCode};
use irys_database::tables::IrysBlockHeaders;
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
    config: &Config,
) -> SolutionContext {
    let max_retries = 20;
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
    let mut salt = irys_types::U256::from(step_number_to_salt_number(
        &config.consensus.vdf,
        step_num - 1_u64,
    ));
    let mut seed = steps[0];

    let mut checkpoints: Vec<H256> =
        vec![H256::default(); config.consensus.vdf.num_checkpoints_in_vdf_step];

    vdf_sha(
        &mut hasher,
        &mut salt,
        &mut seed,
        config.consensus.vdf.num_checkpoints_in_vdf_step,
        config.consensus.vdf.sha_1s_difficulty,
        &mut checkpoints,
    );

    let partition_hash = H256::zero();
    let recall_range_idx = block_validation::get_recall_range(
        step_num,
        &config.consensus,
        &vdf_steps_guard,
        &partition_hash,
    )
    .await
    .expect("valid recall range");

    let mut entropy_chunk = Vec::<u8>::with_capacity(config.consensus.chunk_size as usize);
    compute_entropy_chunk(
        miner_addr,
        recall_range_idx as u64 * config.consensus.num_chunks_in_recall_range,
        partition_hash.into(),
        config.consensus.entropy_packing_iterations,
        config.consensus.chunk_size as usize, // take it from storage config
        &mut entropy_chunk,
        config.consensus.chain_id,
    );

    let max: irys_types::serialization::U256 = irys_types::serialization::U256::MAX;
    let mut le_bytes = [0u8; 32];
    max.to_little_endian(&mut le_bytes);
    let solution_hash = H256(le_bytes);

    SolutionContext {
        partition_hash,
        // FIXME: SolutionContext should in future use PartitionChunkOffset::from()
        // chunk_offset appears to be the end byte rather than the start byte that gets read
        // therefore a saturating_mul is fine as it will read all data up to that point
        // this is also a test util fn, and so less of a concern than a "domain logic" fn
        chunk_offset: TryInto::<u32>::try_into(recall_range_idx)
            .expect("Value exceeds u32::MAX")
            .saturating_mul(
                config
                    .consensus
                    .num_chunks_in_recall_range
                    .try_into()
                    .expect("Value exceeds u32::MAX"),
            ),
        mining_address: miner_addr,
        chunk: entropy_chunk,
        vdf_step: step_num,
        checkpoints: H256List(checkpoints),
        seed: Seed(steps[1]),
        solution_hash,
        ..Default::default()
    }
}

pub async fn random_port() -> eyre::Result<u16> {
    let listener = create_listener(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))?;
    //the assigned port will be random (decided by the OS)
    let port = listener
        .local_addr()
        .map_err(|e| eyre::eyre!("Error getting local address: {:?}", &e))?
        .port();
    Ok(port)
}

// Reasons tx could fail to be added to mempool
#[derive(Debug)]
pub enum AddTxError {
    CreateTx(eyre::Report),
    TxIngress(TxIngressError),
    Mailbox(MailboxError),
}

// TODO: add an "name" field for debug logging
pub struct IrysNodeTest<T = ()> {
    pub node_ctx: T,
    pub cfg: NodeConfig,
    pub temp_dir: TempDir,
}

impl IrysNodeTest<()> {
    pub async fn default_async() -> Self {
        let config = NodeConfig::testnet();
        Self::new_genesis(config)
    }

    /// Start a new test node in peer-sync mode
    pub fn new(mut config: NodeConfig) -> Self {
        config.mode = NodeMode::PeerSync;
        Self::new_inner(config)
    }

    /// Start a new test node in genesis mode
    pub fn new_genesis(mut config: NodeConfig) -> Self {
        config.mode = NodeMode::Genesis;
        Self::new_inner(config)
    }

    fn new_inner(mut config: NodeConfig) -> Self {
        let temp_dir = temporary_directory(None, false);
        config.base_directory = temp_dir.path().to_path_buf();
        Self {
            cfg: config,
            temp_dir,
            node_ctx: (),
        }
    }

    pub async fn start(self) -> IrysNodeTest<IrysNodeCtx> {
        let node = IrysNode::new(self.cfg.clone()).await.unwrap();
        let node_ctx = node.start().await.expect("node cannot be initialized");
        IrysNodeTest {
            cfg: self.cfg,
            node_ctx,
            temp_dir: self.temp_dir,
        }
    }

    pub async fn start_and_wait_for_packing(
        self,
        seconds_to_wait: u64,
    ) -> IrysNodeTest<IrysNodeCtx> {
        let node = self.start().await;
        node.wait_for_packing(seconds_to_wait).await;
        node
    }
}

impl IrysNodeTest<IrysNodeCtx> {
    #[cfg(any(test, feature = "test-utils"))]
    pub fn testnet_peer(&self) -> NodeConfig {
        let node_config = &self.node_ctx.config.node_config;
        let peer_signer = IrysSigner::random_signer(&node_config.consensus_config());
        self.testnet_peer_with_signer(&peer_signer)
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn testnet_peer_with_signer(&self, peer_signer: &IrysSigner) -> NodeConfig {
        use irys_types::{PeerAddress, RethPeerInfo};

        let node_config = &self.node_ctx.config.node_config;

        if node_config.mode == NodeMode::PeerSync {
            panic!("Can only create a peer from a genesis config");
        }

        // Initialize the peer with a random signer, copying the genesis config

        let mut peer_config = node_config.clone();
        peer_config.mining_key = peer_signer.signer.clone();
        peer_config.reward_address = peer_signer.address();

        // Make sure this peer does port randomization instead of copying the genesis ports
        peer_config.http.bind_port = 0;
        peer_config.http.public_port = 0;
        peer_config.gossip.bind_port = 0;
        peer_config.gossip.public_port = 0;

        // Make sure to mark this config as a peer
        peer_config.mode = NodeMode::PeerSync;

        // Add the genesis node details as a trusted peer
        peer_config.trusted_peers = vec![
            (PeerAddress {
                api: format!(
                    "{}:{}",
                    node_config.http.public_ip, node_config.http.public_port
                )
                .parse()
                .expect("valid SocketAddr expected"),
                gossip: format!(
                    "{}:{}",
                    node_config.http.bind_ip, node_config.http.bind_port
                )
                .parse()
                .expect("valid SocketAddr expected"),
                execution: RethPeerInfo::default(),
            }),
        ];
        peer_config
    }
    pub async fn wait_until_height_on_chain(
        &self,
        target_height: u64,
        max_seconds: usize,
    ) -> eyre::Result<()> {
        let mut retries = 0;
        let max_retries = max_seconds; // 1 second per retry
        while self.node_ctx.block_index_guard.read().latest_height() < target_height
            && retries < max_retries
        {
            sleep(Duration::from_secs(1)).await;
            retries += 1;
        }
        if retries == max_retries {
            Err(eyre::eyre!(
                "Failed to reach target height after {} retries",
                retries
            ))
        } else {
            info!(
                "got block after {} seconds and {} retries",
                max_seconds, &retries
            );
            Ok(())
        }
    }

    pub async fn wait_for_packing(&self, seconds_to_wait: u64) {
        wait_for_packing(
            self.node_ctx.actor_addresses.packing.clone(),
            Some(Duration::from_secs(seconds_to_wait)),
        )
        .await
        .expect("for packing to complete in the wait period");
    }

    pub fn start_mining(&self, vdf_mining_state_sender: tokio::sync::mpsc::Sender<bool>) {
        if self
            .node_ctx
            .actor_addresses
            .start_mining(vdf_mining_state_sender)
            .is_err()
        {
            panic!("Expected to start mining")
        }
    }

    pub fn stop_mining(&self, vdf_mining_state_sender: tokio::sync::mpsc::Sender<bool>) {
        if self
            .node_ctx
            .actor_addresses
            .stop_mining(vdf_mining_state_sender)
            .is_err()
        {
            panic!("Expected to stop mining")
        }
    }

    pub async fn start_public_api(&self) {
        let (ema_tx, _ema_rx) = tokio::sync::mpsc::unbounded_channel();
        let api_state = self.node_ctx.get_api_state(ema_tx);

        let _app = actix_web::test::init_service(
            App::new()
                .wrap(Logger::default())
                .app_data(actix_web::web::Data::new(api_state))
                .service(routes()),
        )
        .await;
    }

    pub async fn wait_until_height(
        &self,
        target_height: u64,
        max_seconds: usize,
    ) -> eyre::Result<()> {
        let mut retries = 0;
        let max_retries = max_seconds; // 1 second per retry

        while get_canonical_chain(self.node_ctx.block_tree_guard.clone())
            .await
            .unwrap()
            .0
            .last()
            .unwrap()
            .1
            < target_height
            && retries < max_retries
        {
            sleep(Duration::from_secs(1)).await;
            retries += 1;
        }
        if retries == max_retries {
            Err(eyre::eyre!(
                "Failed to reach target height after {} retries",
                retries
            ))
        } else {
            info!(
                "got block after {} seconds and {} retries",
                max_seconds, &retries
            );
            Ok(())
        }
    }

    pub fn get_height_on_chain(&self) -> u64 {
        self.node_ctx.block_index_guard.read().latest_height()
    }

    pub async fn get_height(&self) -> u64 {
        get_canonical_chain(self.node_ctx.block_tree_guard.clone())
            .await
            .unwrap()
            .0
            .last()
            .unwrap()
            .1
    }

    pub async fn mine_block(&self) -> eyre::Result<()> {
        self.mine_blocks(1).await
    }

    pub async fn mine_blocks(&self, num_blocks: usize) -> eyre::Result<()> {
        self.node_ctx
            .actor_addresses
            .block_producer
            .do_send(SetTestBlocksRemainingMessage(Some(num_blocks as u64)));
        let height = self.get_height().await;
        self.node_ctx.actor_addresses.set_mining(true)?;
        self.wait_until_height(height + num_blocks as u64, 60 * num_blocks)
            .await?;
        self.node_ctx
            .actor_addresses
            .block_producer
            .do_send(SetTestBlocksRemainingMessage(None));
        self.node_ctx.actor_addresses.set_mining(false)
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

        match self
            .node_ctx
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

    pub fn create_signed_data_tx(
        &self,
        account: &IrysSigner,
        data: Vec<u8>,
    ) -> Result<IrysTransaction, AddTxError> {
        let tx = account
            .create_transaction(data, None)
            .map_err(AddTxError::CreateTx)?;
        account.sign_transaction(tx).map_err(AddTxError::CreateTx)
    }

    pub fn get_tx_header(&self, tx_id: &H256) -> eyre::Result<IrysTransactionHeader> {
        match self
            .node_ctx
            .db
            .view_eyre(|tx| tx_header_by_txid(tx, tx_id))
        {
            Ok(Some(tx_header)) => Ok(tx_header),
            Ok(None) => Err(eyre::eyre!("No tx header found for txid {:?}", tx_id)),
            Err(e) => Err(eyre::eyre!("Failed to collect tx header: {}", e)),
        }
    }

    pub fn get_block_by_height_on_chain(
        &self,
        height: u64,
        include_chunk: bool,
    ) -> eyre::Result<IrysBlockHeader> {
        self.node_ctx
            .block_index_guard
            .read()
            .get_item(height)
            .ok_or_else(|| eyre::eyre!("Block at height {} not found", height))
            .and_then(|block| {
                self.node_ctx
                    .db
                    .view_eyre(|tx| {
                        irys_database::block_header_by_hash(tx, &block.block_hash, include_chunk)
                    })?
                    .ok_or_else(|| eyre::eyre!("Block at height {} not found", height))
            })
    }

    pub async fn get_block_by_height(&self, height: u64) -> eyre::Result<IrysBlockHeader> {
        get_canonical_chain(self.node_ctx.block_tree_guard.clone())
            .await
            .unwrap()
            .0
            .iter()
            .find(|(_, blk_height, _, _)| *blk_height == height)
            .map(|(blk_hash, _, _, _)| {
                self.node_ctx
                    .block_tree_guard
                    .read()
                    .get_block(blk_hash)
                    .cloned()
            })
            .flatten()
            .ok_or_else(|| eyre::eyre!("Block at height {} not found", height))
    }

    pub fn get_block_by_hash_on_chain(
        &self,
        hash: &H256,
        include_chunk: bool,
    ) -> eyre::Result<IrysBlockHeader> {
        match &self
            .node_ctx
            .db
            .view_eyre(|tx| irys_database::block_header_by_hash(tx, hash, include_chunk))?
        {
            Some(db_irys_block) => Ok(db_irys_block.clone()),
            None => Err(eyre::eyre!("Block with hash {} not found", hash)),
        }
    }

    pub fn get_block_by_hash(&self, hash: &H256) -> eyre::Result<IrysBlockHeader> {
        self.node_ctx
            .block_tree_guard
            .read()
            .get_block(hash)
            .cloned()
            .ok_or_else(|| eyre::eyre!("Block with hash {} not found", hash))
    }

    pub async fn stop(self) -> IrysNodeTest<()> {
        self.node_ctx.stop().await;
        let cfg = NodeConfig { ..self.cfg };
        IrysNodeTest {
            node_ctx: (),
            cfg,
            temp_dir: self.temp_dir,
        }
    }

    pub async fn post_pledge_commitment(&self, anchor: H256) -> CommitmentTransaction {
        let pledge_tx = CommitmentTransaction {
            commitment_type: CommitmentType::Pledge,
            anchor,
            fee: 1,
            ..Default::default()
        };
        let signer = self.cfg.signer();
        let pledge_tx = signer.sign_commitment(pledge_tx).unwrap();
        info!("Generated pledge_tx.id: {}", pledge_tx.id.0.to_base58());

        // Submit pledge commitment via API
        let api_uri = self.node_ctx.config.node_config.api_uri();
        self.post_commitment_tx_request(&api_uri, &pledge_tx).await;

        pledge_tx
    }

    pub async fn post_stake_commitment(&self, anchor: H256) -> CommitmentTransaction {
        let stake_tx = CommitmentTransaction {
            commitment_type: CommitmentType::Stake,
            // TODO: real staking amounts
            fee: 1,
            anchor,
            ..Default::default()
        };

        let signer = self.cfg.signer();
        let stake_tx = signer.sign_commitment(stake_tx).unwrap();
        info!("Generated stake_tx.id: {}", stake_tx.id.0.to_base58());

        // Submit stake commitment via public API
        let api_uri = self.node_ctx.config.node_config.api_uri();
        self.post_commitment_tx_request(&api_uri, &stake_tx).await;

        stake_tx
    }

    async fn post_commitment_tx_request(
        &self,
        api_uri: &str,
        commitment_tx: &CommitmentTransaction,
    ) {
        info!("Posting Commitment TX: {}", commitment_tx.id.0.to_base58());

        let client = awc::Client::default();
        let url = format!("{}/v1/commitment_tx", api_uri);
        let mut response = client
            .post(url)
            .send_json(commitment_tx) // Send the commitment_tx as JSON in the request body
            .await
            .expect("client post failed");

        if response.status() != StatusCode::OK {
            // Read the response body
            let body_bytes = response.body().await.expect("Failed to read response body");
            let body_str = String::from_utf8_lossy(&body_bytes);

            panic!(
                "Response status: {} - {}\nRequest Body: {}",
                response.status(),
                body_str,
                serde_json::to_string_pretty(&commitment_tx).unwrap(),
            );
        } else {
            info!(
                "Response status: {}\n{}",
                response.status(),
                serde_json::to_string_pretty(&commitment_tx).unwrap()
            );
        }
    }
}

pub async fn mine_blocks(
    node_ctx: &IrysNodeCtx,
    blocks: usize,
) -> eyre::Result<Vec<(Arc<IrysBlockHeader>, ExecutionPayloadEnvelopeV1Irys)>> {
    let mut results = Vec::with_capacity(blocks);
    for _ in 0..blocks {
        results.push(mine_block(node_ctx).await?.unwrap());
    }
    Ok(results)
}

pub async fn mine_block(
    node_ctx: &IrysNodeCtx,
) -> eyre::Result<Option<(Arc<IrysBlockHeader>, ExecutionPayloadEnvelopeV1Irys)>> {
    let vdf_steps_guard = node_ctx.vdf_steps_guard.clone();
    let poa_solution = capacity_chunk_solution(
        node_ctx.config.node_config.miner_address(),
        vdf_steps_guard.clone(),
        &node_ctx.config,
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
) -> eyre::Result<T>
where
    F: Future<Output = T> + Unpin,
{
    loop {
        let poa_solution = capacity_chunk_solution(
            node_ctx.config.node_config.miner_address(),
            node_ctx.vdf_steps_guard.clone(),
            &node_ctx.config,
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
    config: &Config,
) where
    T: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: MessageBody,
{
    if let Some(packed_chunk) = get_chunk(&app, DataLedger::Publish, chunk_offset).await {
        let unpacked_chunk = unpack(
            &packed_chunk,
            config.consensus.entropy_packing_iterations,
            config.consensus.chunk_size as usize,
            config.consensus.chain_id,
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
