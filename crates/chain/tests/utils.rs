use actix_http::Request;
use actix_web::test::call_service;
use actix_web::test::{self, TestRequest};
use actix_web::App;
use actix_web::{
    body::BoxBody,
    dev::{Service, ServiceResponse},
    Error,
};
use alloy_eips::BlockId;
use awc::{body::MessageBody, http::StatusCode};
use base58::ToBase58 as _;
use eyre::{eyre, OptionExt as _};
use futures::future::select;
use irys_actors::{
    block_discovery::BlockDiscoveredMessage,
    block_producer::SolutionFoundMessage,
    block_tree_service::{get_canonical_chain, BlockState, ChainState, ReorgEvent},
    block_validation,
    mempool_service::{MempoolServiceMessage, MempoolTxs, TxIngressError},
    packing::wait_for_packing,
    SetTestBlocksRemainingMessage,
};
use irys_api_server::{create_listener, routes};
use irys_chain::{IrysNode, IrysNodeCtx};
use irys_database::{
    db::IrysDatabaseExt as _,
    get_cache_size,
    tables::{CachedChunks, IngressProofs, IrysBlockHeaders},
    tx_header_by_txid, CommitmentSnapshotStatus,
};
use irys_packing::capacity_single::compute_entropy_chunk;
use irys_packing::unpack;
use irys_primitives::CommitmentType;
use irys_storage::ii;
use irys_testing_utils::utils::tempfile::TempDir;
use irys_testing_utils::utils::temporary_directory;
use irys_types::{
    block_production::Seed, block_production::SolutionContext, irys::IrysSigner,
    partition::PartitionAssignment, Address, DataLedger, GossipBroadcastMessage, H256List, H256,
};
use irys_types::{
    Base64, CommitmentTransaction, Config, DatabaseProvider, IrysBlockHeader, IrysTransaction,
    IrysTransactionHeader, IrysTransactionId, LedgerChunkOffset, NodeConfig, NodeMode, PackedChunk,
    PeerAddress, RethPeerInfo, TxChunkOffset, UnpackedChunk,
};
use irys_vdf::state::VdfStateReadonly;
use irys_vdf::{step_number_to_salt_number, vdf_sha};
use reth::network::{PeerInfo, Peers as _};
use reth::payload::EthBuiltPayload;
use reth_db::{cursor::*, transaction::DbTx as _, Database as _};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::{future::Future, time::Duration};
use tokio::{sync::oneshot::error::RecvError, time::sleep};
use tracing::{debug, debug_span, error, info};

pub async fn capacity_chunk_solution(
    miner_addr: Address,
    vdf_steps_guard: VdfStateReadonly,
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
        config.consensus.vdf.num_iterations_per_checkpoint(),
        &mut checkpoints,
    );

    let partition_hash = H256::zero();
    let recall_range_idx = block_validation::get_recall_range(
        step_num,
        &config.consensus,
        &vdf_steps_guard,
        &partition_hash,
    )
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
    let mut le_bytes = [0_u8; 32];
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

pub fn random_port() -> eyre::Result<u16> {
    let listener = create_listener(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))?;
    //the assigned port will be random (decided by the OS)
    let port = listener
        .local_addr()
        .map_err(|e| eyre::eyre!("Error getting local address: {:?}", &e))?
        .port();
    Ok(port)
}

// Reasons tx could fail to be added to mempool
#[derive(Debug, thiserror::Error)]
pub enum AddTxError {
    #[error("Failed to create transaction")]
    CreateTx(eyre::Report),
    #[error("Failed to add transaction to mempool")]
    TxIngress(TxIngressError),
    #[error("Failed to send transaction to mailbox")]
    Mailbox(RecvError),
}

// TODO: add an "name" field for debug logging
pub struct IrysNodeTest<T = ()> {
    pub node_ctx: T,
    pub cfg: NodeConfig,
    pub temp_dir: TempDir,
}

impl IrysNodeTest<()> {
    pub fn default_async() -> Self {
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
        let node = IrysNode::new(self.cfg.clone()).unwrap();
        let node_ctx = node.start().await.expect("node cannot be initialized");
        IrysNodeTest {
            cfg: self.cfg,
            node_ctx,
            temp_dir: self.temp_dir,
        }
    }

    pub async fn start_with_name(self, log_name: &str) -> IrysNodeTest<IrysNodeCtx> {
        let span = debug_span!("NODE", name = %log_name);
        let _enter = span.enter();
        self.start().await
    }

    pub async fn start_and_wait_for_packing(
        self,
        log_name: &str,
        seconds_to_wait: usize,
    ) -> IrysNodeTest<IrysNodeCtx> {
        let span = debug_span!("NODE", name = %log_name);
        let _enter = span.enter();
        let node = self.start().await;
        node.wait_for_packing(seconds_to_wait).await;
        node
    }
}

impl IrysNodeTest<IrysNodeCtx> {
    pub fn testnet_peer(&self) -> NodeConfig {
        let node_config = &self.node_ctx.config.node_config;
        // Initialize the peer with a random signer, copying the genesis config
        let peer_signer = IrysSigner::random_signer(&node_config.consensus_config());
        self.testnet_peer_with_signer(&peer_signer)
    }

    pub fn testnet_peer_with_signer(&self, peer_signer: &IrysSigner) -> NodeConfig {
        use irys_types::{PeerAddress, RethPeerInfo};

        let node_config = &self.node_ctx.config.node_config;

        if node_config.mode == NodeMode::PeerSync {
            panic!("Can only create a peer from a genesis config");
        }

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

    pub async fn testnet_peer_with_assignments(&self, peer_signer: &IrysSigner) -> Self {
        self.testnet_peer_with_assignments_and_name(peer_signer, "PEER")
            .await
    }

    pub async fn testnet_peer_with_assignments_and_name(
        &self,
        peer_signer: &IrysSigner,
        name: &'static str,
    ) -> Self {
        let seconds_to_wait = 20;

        // Create a new peer config using the provided signer
        let peer_config = self.testnet_peer_with_signer(peer_signer);

        // Start the peer node
        let peer_node = IrysNodeTest::new(peer_config).start_with_name(name).await;

        // Get the latest block hash to use as anchor
        let current_height = self.get_height().await;
        let latest_block = self
            .get_block_by_height(current_height)
            .await
            .expect("to get latest block");
        let anchor = latest_block.block_hash;

        // Post stake + pledge commitments to establish validator status
        let stake_tx = peer_node.post_stake_commitment(anchor).await;
        let pledge_tx = peer_node.post_pledge_commitment(anchor).await;

        // Wait for commitment transactions to show up in this node's mempool
        self.wait_for_mempool(stake_tx.id, seconds_to_wait)
            .await
            .expect("stake tx to be in mempool");
        self.wait_for_mempool(pledge_tx.id, seconds_to_wait)
            .await
            .expect("pledge tx to be in mempool");

        // Mine a block to get the commitments included
        self.mine_block()
            .await
            .expect("to mine block with commitments");

        // Get epoch configuration to calculate when next epoch round occurs
        let num_blocks_in_epoch = self.node_ctx.config.consensus.epoch.num_blocks_in_epoch;
        let current_height_after_commitment = self.get_height().await;

        // Calculate how many blocks we need to mine to reach the next epoch
        let blocks_until_next_epoch =
            num_blocks_in_epoch - (current_height_after_commitment % num_blocks_in_epoch);

        // Mine blocks until we reach the next epoch round
        for _ in 0..blocks_until_next_epoch {
            self.mine_block()
                .await
                .expect("to mine block towards next epoch");
        }

        let final_height = self.get_height().await;

        // Wait for the peer to receive & process the epoch block
        peer_node
            .wait_until_height(final_height, seconds_to_wait)
            .await
            .expect("peer to sync to epoch height");

        // Wait for packing to complete on the peer (this indicates partition assignments are active)
        peer_node.wait_for_packing(seconds_to_wait).await;

        // Verify that partition assignments were created
        let peer_assignments = peer_node.get_partition_assignments(peer_signer.address());

        // Ensure at least one partition has been assigned
        assert!(
            !peer_assignments.is_empty(),
            "Peer should have at least one partition assignment"
        );

        peer_node
    }

    /// get block height in block index
    pub async fn wait_until_block_index_height(
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
                "Failed to reach target height of {} after {} retries",
                target_height,
                retries
            ))
        } else {
            info!(
                "got block at height: {} after {} seconds and {} retries",
                target_height, max_seconds, &retries
            );
            Ok(())
        }
    }

    pub async fn wait_for_packing(&self, seconds_to_wait: usize) {
        wait_for_packing(
            self.node_ctx.actor_addresses.packing.clone(),
            Some(Duration::from_secs(seconds_to_wait as u64)),
        )
        .await
        .expect("for packing to complete in the wait period");
    }

    pub async fn start_mining(&self) {
        if self.node_ctx.start_mining().await.is_err() {
            panic!("Expected to start mining")
        }
    }

    pub async fn stop_mining(&self) {
        if self.node_ctx.stop_mining().await.is_err() {
            panic!("Expected to stop mining")
        }
    }

    pub async fn start_public_api(
        &self,
    ) -> impl Service<Request, Response = ServiceResponse<BoxBody>, Error = Error> {
        let api_state = self.node_ctx.get_api_state();

        actix_web::test::init_service(
            App::new()
                // Remove the logger middleware
                .app_data(actix_web::web::Data::new(api_state))
                .service(routes()),
        )
        .await
    }

    pub async fn wait_until_height(
        &self,
        target_height: u64,
        max_seconds: usize,
    ) -> eyre::Result<H256> {
        let mut retries = 0;
        let max_retries = max_seconds; // 1 second per retry

        loop {
            let canonical_chain = get_canonical_chain(self.node_ctx.block_tree_guard.clone())
                .await
                .unwrap();
            let latest_block = canonical_chain.0.last().unwrap();

            if latest_block.height >= target_height {
                info!(
                    "reached height {} after {} retries",
                    target_height, &retries
                );
                return Ok(latest_block.block_hash);
            }

            if retries >= max_retries {
                return Err(eyre::eyre!(
                    "Failed to reach target height {} after {} retries",
                    target_height,
                    retries
                ));
            }

            sleep(Duration::from_secs(1)).await;
            retries += 1;
        }
    }

    pub async fn wait_until_height_confirmed(
        &self,
        target_height: u64,
        max_seconds: usize,
    ) -> eyre::Result<H256> {
        let mut retries = 0;
        let max_retries = max_seconds; // 1 second per retry

        loop {
            let canonical_chain = get_canonical_chain(self.node_ctx.block_tree_guard.clone())
                .await
                .unwrap();
            let latest_block = canonical_chain.0.last().unwrap();

            let latest_height = latest_block.height;
            let not_onchain_count = canonical_chain.1 as u64;
            if (latest_height - not_onchain_count) >= target_height {
                info!(
                    "reached height {} after {} retries",
                    target_height, &retries
                );

                return Ok(latest_block.block_hash);
            }

            if retries >= max_retries {
                return Err(eyre::eyre!(
                    "Failed to reach target height {} after {} retries",
                    target_height,
                    retries
                ));
            }

            sleep(Duration::from_secs(1)).await;
            retries += 1;
        }
    }

    pub async fn wait_for_chunk(
        &self,
        app: &impl actix_web::dev::Service<
            actix_http::Request,
            Response = ServiceResponse,
            Error = actix_web::Error,
        >,
        ledger: DataLedger,
        offset: i32,
        seconds: usize,
    ) -> eyre::Result<()> {
        let delay = Duration::from_secs(1);
        for attempt in 1..seconds {
            if let Some(_packed_chunk) =
                get_chunk(&app, ledger, LedgerChunkOffset::from(offset)).await
            {
                info!("chunk found {} attempts", attempt);
                return Ok(());
            }
            sleep(delay).await;
        }

        Err(eyre::eyre!(
            "Failed waiting for chunk to arrive. Waited {} seconds",
            seconds,
        ))
    }

    /// check number of chunks in the CachedChunks table
    /// return Ok(()) once it matches the expected value
    pub async fn wait_for_chunk_cache_count(
        &self,
        expected_value: u64,
        timeout_secs: usize,
    ) -> eyre::Result<()> {
        const CHECKS_PER_SECOND: usize = 10;
        let delay = Duration::from_millis(1000 / CHECKS_PER_SECOND as u64);
        let max_attempts = timeout_secs * CHECKS_PER_SECOND;

        for _ in 0..max_attempts {
            let chunk_cache_count = self
                .node_ctx
                .db
                .view_eyre(|tx| {
                    get_cache_size::<CachedChunks, _>(tx, self.node_ctx.config.consensus.chunk_size)
                })?
                .0;

            if chunk_cache_count == expected_value {
                return Ok(());
            }

            tokio::time::sleep(delay).await;
        }

        Err(eyre::eyre!(
            "Timed out after {} seconds waiting for chunk_cache_count == {}",
            timeout_secs,
            expected_value
        ))
    }

    /// mine blocks until the txs are found in the block index, i.e. mdbx
    pub async fn wait_for_migrated_txs(
        &self,
        mut unconfirmed_txs: Vec<IrysTransactionHeader>,
        seconds: usize,
    ) -> eyre::Result<()> {
        let delay = Duration::from_secs(1);
        for attempt in 1..seconds {
            // Do we have any unconfirmed tx?
            let Some(tx) = unconfirmed_txs.first() else {
                // if not return we are done
                return Ok(());
            };

            let ro_tx = self
                .node_ctx
                .db
                .as_ref()
                .tx()
                .map_err(|e| {
                    tracing::error!("Failed to create mdbx transaction: {}", e);
                })
                .unwrap();

            // Retrieve the transaction header from database
            if let Ok(Some(header)) = tx_header_by_txid(&ro_tx, &tx.id) {
                // the proofs may be added to the tx during promotion
                // and so we cant do a direct comparison
                // we can however check some key fields are equal
                assert_eq!(tx.id, header.id);
                assert_eq!(tx.anchor, header.anchor);
                tracing::info!("Transaction was retrieved ok after {} attempts", attempt);
                unconfirmed_txs.pop();
            };
            drop(ro_tx);
            mine_blocks(&self.node_ctx, 1).await.unwrap();
            sleep(delay).await;
        }
        Err(eyre::eyre!(
            "Failed waiting for confirmed txs. Waited {} seconds",
            seconds,
        ))
    }

    /// wait for data tx to be in mempool and it's IngressProofs to be in database
    pub async fn wait_for_ingress_proofs(
        &self,
        mut unconfirmed_promotions: Vec<H256>,
        seconds: usize,
    ) -> eyre::Result<()> {
        tracing::info!(
            "waiting up to {} seconds for unconfirmed_promotions: {:?}",
            seconds,
            unconfirmed_promotions
        );
        for attempts in 1..seconds {
            // Do we have any unconfirmed promotions?
            let Some(txid) = unconfirmed_promotions.first() else {
                // if not return we are done
                return Ok(());
            };

            // create db read transaction
            let ro_tx = self
                .node_ctx
                .db
                .as_ref()
                .tx()
                .map_err(|e| {
                    tracing::error!("Failed to create mdbx transaction: {}", e);
                })
                .unwrap();

            // Retrieve the transaction header from mempool or database
            let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
            self.node_ctx
                .service_senders
                .mempool
                .send(MempoolServiceMessage::GetDataTxs(vec![*txid], oneshot_tx))?;
            if let Some(tx_header) = oneshot_rx.await.unwrap().first().unwrap() {
                //read its ingressproof(s)
                if let Some(proof) = ro_tx.get::<IngressProofs>(tx_header.data_root).unwrap() {
                    assert_eq!(proof.data_root, tx_header.data_root);
                    tracing::info!("Proofs available after {} attempts", attempts);
                    unconfirmed_promotions.pop();
                };
            }
            drop(ro_tx);
            mine_block(&self.node_ctx).await.unwrap();
            sleep(Duration::from_secs(1)).await;
        }

        Err(eyre::eyre!(
            "Failed waiting for ingress proofs. Waited {} seconds",
            seconds,
        ))
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
            .height
    }

    /// Returns a future that resolves when a reorg is detected.
    ///
    /// Subscribes to the reorg broadcast channel and waits up to `seconds_to_wait` for the ReorgEvent.
    /// The future can be created before triggering operations that might cause a reorg.
    ///
    /// # Returns
    /// * `Ok(ReorgEvent)` - Details about the reorg (orphaned blocks, new chain, fork point)
    /// * `Err` - On timeout or channel closure
    ///
    /// # Example
    /// ```
    /// let reorg_future = node.wait_for_reorg(30);
    /// peer.mine_competing_block().await?;
    /// let reorg = reorg_future.await?;
    /// ```
    pub fn wait_for_reorg(
        &self,
        seconds_to_wait: usize,
    ) -> impl Future<Output = eyre::Result<ReorgEvent>> {
        // Subscribe to reorg events
        let mut reorg_rx = self.node_ctx.service_senders.subscribe_reorgs();
        let timeout_duration = Duration::from_secs(seconds_to_wait as u64);

        // Return the future without awaiting it
        async move {
            match tokio::time::timeout(timeout_duration, reorg_rx.recv()).await {
                Ok(Ok(reorg_event)) => {
                    info!(
                    "Reorg detected: {} blocks in old fork, {} in new fork, fork at height {}, new tip: {}",
                    reorg_event.old_fork.len(),
                    reorg_event.new_fork.len(),
                    reorg_event.fork_parent.height,
                    reorg_event.new_tip.0.to_base58()
                );
                    Ok(reorg_event)
                }
                Ok(Err(err)) => Err(eyre::eyre!("Reorg broadcast channel closed: {}", err)),
                Err(_) => Err(eyre::eyre!(
                    "Timeout: No reorg event received within {} seconds",
                    seconds_to_wait
                )),
            }
        }
    }

    pub async fn mine_block(&self) -> eyre::Result<IrysBlockHeader> {
        let height = self.get_height().await;
        self.mine_blocks(1).await?;
        let hash = self.wait_until_height(height + 1, 10).await?;
        self.get_block_by_hash(&hash)
    }

    pub async fn mine_blocks(&self, num_blocks: usize) -> eyre::Result<()> {
        self.node_ctx
            .actor_addresses
            .block_producer
            .do_send(SetTestBlocksRemainingMessage(Some(num_blocks as u64)));
        let height = self.get_height().await;
        self.node_ctx.start_mining().await?;
        let _block_hash = self
            .wait_until_height(height + num_blocks as u64, 60 * num_blocks)
            .await?;
        self.node_ctx
            .actor_addresses
            .block_producer
            .do_send(SetTestBlocksRemainingMessage(None));
        self.node_ctx.stop_mining().await
    }

    pub async fn mine_blocks_without_gossip(&self, num_blocks: usize) -> eyre::Result<()> {
        let prev_is_syncing = self.node_ctx.sync_state.is_syncing();
        self.gossip_disable();
        self.mine_blocks(num_blocks).await?;
        self.node_ctx.sync_state.set_is_syncing(prev_is_syncing);
        Ok(())
    }

    pub async fn mine_block_without_gossip(
        &self,
    ) -> eyre::Result<(Arc<IrysBlockHeader>, EthBuiltPayload)> {
        let prev_is_syncing = self.node_ctx.sync_state.is_syncing();
        self.gossip_disable();
        let res = mine_block(&self.node_ctx).await?.unwrap();
        self.node_ctx.sync_state.set_is_syncing(prev_is_syncing);
        Ok(res)
    }

    pub fn get_commitment_snapshot_status(
        &self,
        commitment_tx: &CommitmentTransaction,
    ) -> CommitmentSnapshotStatus {
        let commitment_snapshot = self
            .node_ctx
            .block_tree_guard
            .read()
            .canonical_commitment_snapshot();

        let is_staked = self
            .node_ctx
            .block_tree_guard
            .read()
            .canonical_epoch_snapshot()
            .commitment_state
            .read()
            .unwrap()
            .is_staked(commitment_tx.signer);
        commitment_snapshot.get_commitment_status(commitment_tx, is_staked)
    }

    /// wait for specific block to be available via block tree guard
    ///   i.e. in the case of a fork, check a specific block has been gossiped between peers,
    ///        even though it may not become part of the canonical chain.
    pub async fn wait_for_block(
        &self,
        hash: &H256,
        seconds_to_wait: usize,
    ) -> eyre::Result<IrysBlockHeader> {
        let retries_per_second = 50;
        let max_retries = seconds_to_wait * retries_per_second;
        let mut retries = 0;

        for _ in 0..max_retries {
            if let Ok(block) = self.get_block_by_hash(hash) {
                info!("block found in block tree after {} retries", &retries);
                return Ok(block);
            }

            sleep(Duration::from_millis((1000 / retries_per_second) as u64)).await;
            retries += 1;
        }

        Err(eyre::eyre!(
            "Failed to locate block in block tree after {} retries",
            retries
        ))
    }

    /// wait for tx to appear in the mempool or be found in the database
    pub async fn wait_for_mempool(
        &self,
        tx_id: IrysTransactionId,
        seconds_to_wait: usize,
    ) -> eyre::Result<()> {
        let mempool_service = self.node_ctx.service_senders.mempool.clone();
        let mut retries = 0;
        let max_retries = seconds_to_wait; // 1 second per retry

        for _ in 0..max_retries {
            let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
            mempool_service.send(MempoolServiceMessage::DataTxExists(tx_id, oneshot_tx))?;

            //if transaction exists
            if oneshot_rx
                .await
                .expect("to process ChunkIngressMessage")
                .expect("boolean response to transaction existence")
            {
                break;
            }

            sleep(Duration::from_secs(1)).await;
            retries += 1;
        }

        if retries == max_retries {
            Err(eyre::eyre!(
                "Failed to locate tx in mempool after {} retries",
                retries
            ))
        } else {
            info!("transaction found in mempool after {} retries", &retries);
            Ok(())
        }
    }

    pub async fn wait_for_mempool_commitment_txs(
        &self,
        mut tx_ids: Vec<H256>,
        seconds_to_wait: usize,
    ) -> eyre::Result<()> {
        let mempool_service = self.node_ctx.service_senders.mempool.clone();
        let mut retries = 0;
        let max_retries = seconds_to_wait * 5; // 200ms per retry

        while let Some(tx_id) = tx_ids.pop() {
            'inner: while retries < max_retries {
                let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
                mempool_service.send(MempoolServiceMessage::GetCommitmentTxs {
                    commitment_tx_ids: vec![tx_id],
                    response: oneshot_tx,
                })?;

                //if transaction exists in mempool
                if oneshot_rx
                    .await
                    .expect("to process GetCommitmentTxs")
                    .contains_key(&tx_id)
                {
                    break 'inner;
                }

                sleep(Duration::from_millis(200)).await;
                retries += 1;
            }
        }

        if retries == max_retries {
            tracing::error!(
                "transaction not found in mempool after {} retries",
                &retries
            );
            Err(eyre::eyre!(
                "Failed to locate tx in mempool after {} retries",
                retries
            ))
        } else {
            info!("transactions found in mempool after {} retries", &retries);
            Ok(())
        }
    }

    // Get the best txs from the mempool, based off the account state at the optional parent EVM block
    // if None is provided, it will use the latest state.
    pub async fn get_best_mempool_tx(
        &self,
        parent_evm_block_hash: Option<BlockId>,
    ) -> eyre::Result<MempoolTxs> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.node_ctx
            .service_senders
            .mempool
            .send(MempoolServiceMessage::GetBestMempoolTxs(
                parent_evm_block_hash,
                tx,
            ))
            .expect("to send MempoolServiceMessage");
        rx.await.expect("to receive best transactions from mempool")
    }

    pub fn peer_address(&self) -> PeerAddress {
        let http = &self.node_ctx.config.node_config.http;
        let gossip = &self.node_ctx.config.node_config.gossip;

        PeerAddress {
            api: format!("{}:{}", http.bind_ip, http.bind_port)
                .parse()
                .expect("valid SocketAddr expected"),
            gossip: format!("{}:{}", gossip.bind_ip, gossip.bind_port)
                .parse()
                .expect("valid SocketAddr expected"),
            execution: RethPeerInfo::default(),
        }
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

        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let response =
            self.node_ctx
                .service_senders
                .mempool
                .send(MempoolServiceMessage::IngestDataTx(
                    tx.header.clone(),
                    oneshot_tx,
                ));
        if let Err(e) = response {
            tracing::error!("channel closed, unable to send to mempool: {:?}", e);
        }

        match oneshot_rx.await {
            Ok(Ok(())) => Ok(tx),
            Ok(Err(tx_error)) => Err(AddTxError::TxIngress(tx_error)),
            Err(e) => Err(AddTxError::Mailbox(e)),
        }
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

    /// read storage tx from mbdx i.e. block index
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

    /// read storage tx from mempool
    pub async fn get_storage_tx_header_from_mempool(
        &self,
        tx_id: &H256,
    ) -> eyre::Result<IrysTransactionHeader> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let tx_ingress_msg = MempoolServiceMessage::GetDataTxs(vec![*tx_id], oneshot_tx);
        if let Err(err) = self.node_ctx.service_senders.mempool.send(tx_ingress_msg) {
            tracing::error!(
                "API Failed to deliver MempoolServiceMessage::GetDataTxs: {:?}",
                err
            );
        }
        let mempool_response = oneshot_rx.await.expect(
            "to receive IrysTransactionResponse from MempoolServiceMessage::GetDataTxs message",
        );
        let maybe_mempool_tx = mempool_response.first();
        if let Some(Some(tx)) = maybe_mempool_tx {
            return Ok(tx.clone());
        }
        Err(eyre::eyre!("No tx header found for txid {:?}", tx_id))
    }

    /// read commitment tx from mempool
    pub async fn get_commitment_tx_from_mempool(
        &self,
        tx_id: &H256,
    ) -> eyre::Result<CommitmentTransaction> {
        // try to get commitment tx from mempool
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let tx_ingress_msg = MempoolServiceMessage::GetCommitmentTxs {
            commitment_tx_ids: vec![*tx_id],
            response: oneshot_tx,
        };
        if let Err(err) = self.node_ctx.service_senders.mempool.send(tx_ingress_msg) {
            tracing::error!(
                "API Failed to deliver MempoolServiceMessage::GetCommitmentTxs: {:?}",
                err
            );
        }
        let mempool_response = oneshot_rx.await.expect(
            "to receive IrysTransactionResponse from MempoolServiceMessage::GetCommitmentTxs message",
        );
        let maybe_mempool_tx = mempool_response.get(tx_id);
        if let Some(tx) = maybe_mempool_tx {
            return Ok(tx.clone());
        }
        Err(eyre::eyre!("No tx header found for txid {:?}", tx_id))
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
            .find(|e| e.height == height)
            .and_then(|e| {
                self.node_ctx
                    .block_tree_guard
                    .read()
                    .get_block(&e.block_hash)
                    .cloned()
            })
            .ok_or_else(|| eyre::eyre!("Block at height {} not found", height))
    }

    pub fn gossip_block(&self, block_header: &IrysBlockHeader) -> eyre::Result<()> {
        self.node_ctx
            .service_senders
            .gossip_broadcast
            .send(GossipBroadcastMessage::from((*block_header).clone()))?;

        Ok(())
    }

    pub fn gossip_eth_block(
        &self,
        block: &reth::primitives::SealedBlock<reth::primitives::Block>,
    ) -> eyre::Result<()> {
        self.node_ctx
            .service_senders
            .gossip_broadcast
            .send(GossipBroadcastMessage::from((block).clone()))?;

        Ok(())
    }

    /// reads block header from database
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

    /// get block from block tree guard
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
        let cfg = self.cfg;
        IrysNodeTest {
            node_ctx: (),
            cfg,
            temp_dir: self.temp_dir,
        }
    }

    /// useful in tests when creating forks and
    /// needing to send specific blocks between specific peers
    pub async fn send_block_to_peer(
        &self,
        peer: &Self,
        irys_block_header: &IrysBlockHeader,
    ) -> eyre::Result<()> {
        match peer
            .node_ctx
            .actor_addresses
            .block_discovery_addr
            .send(BlockDiscoveredMessage(Arc::new(irys_block_header.clone())))
            .await
        {
            Ok(_) => Ok(()),
            Err(res) => {
                tracing::error!(
                    "Sent block to peer. Block {:?} ({}) failed pre-validation: {:?}",
                    &irys_block_header.block_hash.0,
                    &irys_block_header.height,
                    res
                );
                Err(eyre!(
                    "Sent block to peer. Block {:?} ({}) failed pre-validation: {:?}",
                    &irys_block_header.block_hash.0,
                    &irys_block_header.height,
                    res
                ))
            }
        }
    }

    pub async fn post_data_tx_without_gossip(
        &self,
        anchor: H256,
        data: Vec<u8>,
        signer: &IrysSigner,
    ) -> IrysTransaction {
        let prev_is_syncing = self.node_ctx.sync_state.is_syncing();
        self.gossip_disable();
        let tx = self.post_data_tx(anchor, data, signer).await;
        self.node_ctx.sync_state.set_is_syncing(prev_is_syncing);
        tx
    }

    pub async fn post_data_tx(
        &self,
        anchor: H256,
        data: Vec<u8>,
        signer: &IrysSigner,
    ) -> IrysTransaction {
        let tx = signer
            .create_transaction(data, Some(anchor))
            .expect("Expect to create a storage transaction from the data");
        let tx = signer
            .sign_transaction(tx)
            .expect("to sign the storage transaction");

        let client = awc::Client::default();
        let api_uri = self.node_ctx.config.node_config.api_uri();
        let url = format!("{}/v1/tx", api_uri);
        let mut response = client
            .post(url)
            .send_json(&tx.header) // Send the tx as JSON in the request body
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
                serde_json::to_string_pretty(&tx.header).unwrap(),
            );
        } else {
            info!(
                "Response status: {}\n{}",
                response.status(),
                serde_json::to_string_pretty(&tx).unwrap()
            );
        }
        tx
    }

    pub async fn post_data_tx_raw(&self, tx: &IrysTransactionHeader) {
        let client = awc::Client::default();
        let api_uri = self.node_ctx.config.node_config.api_uri();
        let url = format!("{}/v1/tx", api_uri);
        let mut response = client
            .post(url)
            .send_json(&tx) // Send the tx as JSON in the request body
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
                serde_json::to_string_pretty(&tx).unwrap(),
            );
        } else {
            info!(
                "Response status: {}\n{}",
                response.status(),
                serde_json::to_string_pretty(&tx).unwrap()
            );
        }
    }

    pub async fn post_chunk_32b(
        &self,
        tx: &IrysTransaction,
        chunk_index: usize,
        chunks: &[[u8; 32]],
    ) {
        let chunk = UnpackedChunk {
            data_root: tx.header.data_root,
            data_size: tx.header.data_size,
            data_path: Base64(tx.proofs[chunk_index].proof.clone()),
            bytes: Base64(chunks[chunk_index].to_vec()),
            tx_offset: TxChunkOffset::from(chunk_index as u32),
        };

        let client = awc::Client::default();
        let api_uri = self.node_ctx.config.node_config.api_uri();
        let url = format!("{}/v1/chunk", api_uri);
        let response = client
            .post(url)
            .send_json(&chunk) // Send the tx as JSON in the request body
            .await
            .expect("client post failed");

        debug!("chunk_index: {:?}", chunk_index);
        assert_eq!(response.status(), StatusCode::OK);
    }

    pub async fn post_commitment_tx(&self, commitment_tx: &CommitmentTransaction) {
        let api_uri = self.node_ctx.config.node_config.api_uri();
        self.post_commitment_tx_request(&api_uri, commitment_tx)
            .await;
    }

    pub async fn post_commitment_tx_raw_without_gossip(
        &self,
        commitment_tx: &CommitmentTransaction,
    ) {
        let api_uri = self.node_ctx.config.node_config.api_uri();
        let prev_is_syncing = self.node_ctx.sync_state.is_syncing();
        self.node_ctx.sync_state.set_is_syncing(true);
        self.post_commitment_tx_request(&api_uri, commitment_tx)
            .await;
        self.node_ctx.sync_state.set_is_syncing(prev_is_syncing);
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

    pub async fn post_pledge_commitment_without_gossip(
        &self,
        anchor: H256,
    ) -> CommitmentTransaction {
        let prev_is_syncing = self.node_ctx.sync_state.is_syncing();
        self.gossip_disable();

        let stake_tx = self.post_pledge_commitment(anchor).await;
        self.node_ctx.sync_state.set_is_syncing(prev_is_syncing);

        stake_tx
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

    pub async fn post_stake_commitment_without_gossip(
        &self,
        anchor: H256,
    ) -> CommitmentTransaction {
        let prev_is_syncing = self.node_ctx.sync_state.is_syncing();
        self.gossip_disable();

        let stake_tx = self.post_stake_commitment(anchor).await;
        self.node_ctx.sync_state.set_is_syncing(prev_is_syncing);

        stake_tx
    }

    pub fn get_partition_assignments(&self, miner_address: Address) -> Vec<PartitionAssignment> {
        let epoch_snapshot = self
            .node_ctx
            .block_tree_guard
            .read()
            .canonical_epoch_snapshot();

        epoch_snapshot.get_partition_assignments(miner_address)
    }

    async fn post_commitment_tx_request(
        &self,
        api_uri: &str,
        commitment_tx: &CommitmentTransaction,
    ) {
        info!("Posting Commitment TX: {}", commitment_tx.id.0.to_base58());

        let client = awc::Client::default();
        let url = format!("{}/v1/commitment_tx", api_uri);
        let result = client
            .post(url)
            .send_json(commitment_tx) // Send the commitment_tx as JSON in the request body
            .await;

        let mut response = match result {
            Ok(r) => r,
            Err(e) => {
                error!("Failed to post commitment transaction: {e}");
                return;
            }
        };

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

    // disconnect all Reth peers from network
    // return Vec<PeerInfo>> as it was prior to disconnect
    pub async fn disconnect_all_reth_peers(&self) -> eyre::Result<Vec<PeerInfo>> {
        let ctx = self.node_ctx.reth_node_adapter.clone();

        let all_peers_prior = ctx.inner.network.get_all_peers().await?;
        for peer in all_peers_prior.iter() {
            ctx.inner.network.disconnect_peer(peer.remote_id);
        }

        while !ctx.inner.network.get_all_peers().await?.is_empty() {
            sleep(Duration::from_millis(100)).await;
        }

        let all_peers_after = ctx.inner.network.get_all_peers().await?;
        assert!(
            all_peers_after.is_empty(),
            "the peer should be completely disconnected",
        );

        Ok(all_peers_prior)
    }

    // Reconnect Reth peers passed to fn
    pub fn reconnect_all_reth_peers(&self, peers: &Vec<PeerInfo>) {
        for peer in peers {
            self.node_ctx
                .reth_node_adapter
                .inner
                .network
                .connect_peer(peer.remote_id, peer.remote_addr);
        }
    }

    // enable node to gossip until disabled
    pub fn gossip_enable(&self) {
        self.node_ctx.sync_state.set_gossip_reception_enabled(true);
        self.node_ctx.sync_state.set_gossip_broadcast_enabled(true);
    }

    // disable node ability to gossip until enabled
    pub fn gossip_disable(&self) {
        self.node_ctx.sync_state.set_gossip_reception_enabled(false);
        self.node_ctx.sync_state.set_gossip_broadcast_enabled(false);
    }
}

pub async fn mine_blocks(
    node_ctx: &IrysNodeCtx,
    blocks: usize,
) -> eyre::Result<Vec<(Arc<IrysBlockHeader>, EthBuiltPayload)>> {
    let mut results = Vec::with_capacity(blocks);
    for _ in 0..blocks {
        results.push(mine_block(node_ctx).await?.unwrap());
    }
    Ok(results)
}

pub async fn mine_block(
    node_ctx: &IrysNodeCtx,
) -> eyre::Result<Option<(Arc<IrysBlockHeader>, EthBuiltPayload)>> {
    let poa_solution = solution_context(node_ctx).await?;

    node_ctx
        .actor_addresses
        .block_producer
        .send(SolutionFoundMessage(poa_solution.clone()))
        .await?
}

pub async fn solution_context(node_ctx: &IrysNodeCtx) -> Result<SolutionContext, eyre::Error> {
    let vdf_steps_guard = node_ctx.vdf_steps_guard.clone();
    node_ctx.start_vdf().await?;
    let poa_solution = capacity_chunk_solution(
        node_ctx.config.node_config.miner_address(),
        vdf_steps_guard.clone(),
        &node_ctx.config,
    )
    .await;
    node_ctx.stop_vdf().await?;
    Ok(poa_solution)
}

#[derive(Debug, Clone, PartialEq)]
pub enum BlockValidationOutcome {
    StoredOnNode(ChainState),
    Discarded,
}

pub async fn mine_block_and_wait_for_validation(
    node_ctx: &IrysNodeCtx,
) -> eyre::Result<(
    Arc<IrysBlockHeader>,
    EthBuiltPayload,
    BlockValidationOutcome,
)> {
    let (block, reth_payload) = mine_block(node_ctx)
        .await?
        .ok_or_eyre("block not returned")?;
    let block_hash = &block.block_hash;
    let res = read_block_from_state(node_ctx, block_hash).await;

    Ok((block, reth_payload, res))
}

pub async fn read_block_from_state(
    node_ctx: &IrysNodeCtx,
    block_hash: &H256,
) -> BlockValidationOutcome {
    let mut was_validation_scheduled = false;

    for _ in 0..500 {
        let result = {
            let read = node_ctx.block_tree_guard.read();
            let mut result = read
                .get_block_and_status(block_hash)
                .into_iter()
                .map(|(_, state)| *state);
            result.next()
        };

        let Some(chain_state) = result else {
            // If we previously saw "validation scheduled" and now block status is None,
            // it means the block was discarded
            if was_validation_scheduled {
                return BlockValidationOutcome::Discarded;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            continue;
        };
        match chain_state {
            ChainState::NotOnchain(BlockState::ValidationScheduled)
            | ChainState::Validated(BlockState::ValidationScheduled) => {
                was_validation_scheduled = true;
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
            _ => return BlockValidationOutcome::StoredOnNode(chain_state),
        }
    }
    BlockValidationOutcome::Discarded
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
        let race = select(&mut future, Box::pin(sleep(timeout_duration))).await;
        match race {
            // provided future finished
            futures::future::Either::Left((res, _)) => return Ok(res),
            // we need another block
            futures::future::Either::Right(_) => {
                info!("deployment timed out, creating new block..")
            }
        };
        mine_block(&node_ctx).await?;
    }
}

/// Helper function for testing chunk uploads. Posts a single chunk of transaction data
/// to the /v1/chunk endpoint and verifies successful response.
pub async fn post_chunk<T, B>(
    app: &T,
    tx: &IrysTransaction,
    chunk_index: usize,
    chunks: &[[u8; 32]],
) where
    T: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
{
    let chunk = UnpackedChunk {
        data_root: tx.header.data_root,
        data_size: tx.header.data_size,
        data_path: Base64(tx.proofs[chunk_index].proof.clone()),
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

/// Posts a storage transaction to the node via HTTP POST request using the actix-web test framework.
///
/// This function submits the transaction header to the `/v1/tx` endpoint and verifies
/// that the response has a successful HTTP 200 status code. The response body is logged
/// for debugging purposes.
///
/// # Arguments
/// * `app` - The actix-web service to test against
/// * `tx` - The Irys transaction to submit (only the header is sent)
///
/// # Panics
/// Panics if the response status is not HTTP 200 OK.
pub async fn post_data_tx<T, B>(app: &T, tx: &IrysTransaction)
where
    T: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: MessageBody + Unpin,
{
    let req = TestRequest::post()
        .uri("/v1/tx")
        .set_json(tx.header.clone())
        .to_request();

    let resp = call_service(&app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    debug!("Response body: {:#?}", body);
    assert_eq!(status, StatusCode::OK);
}

pub async fn post_commitment_tx<T, B>(app: &T, tx: &CommitmentTransaction)
where
    T: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: MessageBody + Unpin,
{
    let req = TestRequest::post()
        .uri("/v1/commitment_tx")
        .set_json(tx.clone())
        .to_request();

    let resp = call_service(&app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    debug!("Response body: {:#?}", body);
    assert_eq!(status, StatusCode::OK);
}

pub fn new_stake_tx(anchor: &H256, signer: &IrysSigner) -> CommitmentTransaction {
    let stake_tx = CommitmentTransaction {
        commitment_type: CommitmentType::Stake,
        // TODO: real staking amounts
        fee: 1,
        anchor: *anchor,
        ..Default::default()
    };

    signer.sign_commitment(stake_tx).unwrap()
}

pub fn new_pledge_tx(anchor: &H256, signer: &IrysSigner) -> CommitmentTransaction {
    let stake_tx = CommitmentTransaction {
        commitment_type: CommitmentType::Pledge,
        // TODO: real pledging amounts
        fee: 1,
        anchor: *anchor,
        ..Default::default()
    };

    signer.sign_commitment(stake_tx).unwrap()
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
