use actix_http::Request;
use actix_web::http::StatusCode;
use actix_web::test::call_service;
use actix_web::test::{self, TestRequest};
use actix_web::App;
use actix_web::{
    body::{BoxBody, MessageBody},
    dev::{Service, ServiceResponse},
    Error,
};
use alloy_core::primitives::FixedBytes;
use alloy_eips::BlockId;
use eyre::{eyre, OptionExt as _};
use futures::future::select;
use irys_actors::block_discovery::{BlockDiscoveryFacade as _, BlockDiscoveryFacadeImpl};
use irys_actors::shadow_tx_generator::PublishLedgerWithTxs;
use irys_actors::{
    block_producer::BlockProducerCommand,
    block_tree_service::{BlockStateUpdated, ReorgEvent},
    block_validation,
    mempool_service::{MempoolServiceMessage, MempoolTxs, TxIngressError},
    MempoolServiceFacadeImpl,
};
use irys_api_client::{ApiClientExt as _, IrysApiClient};
use irys_api_server::routes;
use irys_api_server::routes::price::{CommitmentPriceInfo, PriceInfo};
use irys_chain::{IrysNode, IrysNodeCtx};
use irys_database::walk_all;
use irys_database::{
    db::IrysDatabaseExt as _,
    get_cache_size,
    tables::{CachedChunks, IngressProofs, IrysBlockHeaders},
    tx_header_by_txid,
};
use irys_domain::{
    get_canonical_chain, BlockState, BlockTreeEntry, ChainState, ChunkType,
    CommitmentSnapshotStatus, EmaSnapshot, EpochSnapshot,
};
use irys_macros_diag_slow::diag_slow;
use irys_p2p::{GossipClient, GossipServer};
use irys_packing::capacity_single::compute_entropy_chunk;
use irys_packing::unpack;
use irys_reth_node_bridge::ext::IrysRethRpcTestContextExt as _;
use irys_storage::ii;
use irys_testing_utils::chunk_bytes_gen;
use irys_testing_utils::utils::tempfile::TempDir;
use irys_testing_utils::utils::temporary_directory;
use irys_types::v2::GossipBroadcastMessageV2;
use irys_types::SendTraced as _;
use irys_types::{
    block_production::Seed, block_production::SolutionContext, irys::IrysSigner,
    partition::PartitionAssignment, BlockBody, BlockHash, BlockTransactions, DataLedger,
    EvmBlockHash, H256List, IrysAddress, NetworkConfigWithDefaults as _, SealedBlock, SyncMode,
    SystemLedger, H256, U256,
};
use irys_types::{
    Base64, ChunkBytes, CommitmentTransaction, CommitmentTransactionV2, CommitmentTypeV2,
    CommitmentV2WithMetadata, Config, ConsensusConfig, DataTransaction, DataTransactionHeader,
    DatabaseProvider, IngressProof, IrysBlockHeader, IrysTransactionId, LedgerChunkOffset,
    NodeConfig, NodeMode, PackedChunk, PeerAddress, TxChunkOffset, UnpackedChunk,
};
use irys_types::{
    HandshakeRequest, HandshakeRequestV2, Interval, PartitionChunkOffset, ProtocolVersion,
};
use irys_vdf::state::VdfStateReadonly;
use irys_vdf::{step_number_to_salt_number, vdf_sha};
use itertools::Itertools as _;
use reth::{
    network::{PeerInfo, Peers as _},
    payload::EthBuiltPayload,
    rpc::types::RpcBlockHash,
    rpc::{api::EthApiServer as _, types::BlockNumberOrTag},
};
use reth_db::{cursor::*, Database as _};
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::{
    future::Future,
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
use tokio::{sync::oneshot::error::RecvError, time::sleep};
use tracing::{debug, error, error_span, info, instrument, warn};

pub async fn capacity_chunk_solution(
    miner_addr: IrysAddress,
    vdf_steps_guard: VdfStateReadonly,
    config: &Config,
    difficulty: U256,
) -> SolutionContext {
    // Wait until we have at least 2 new VDF steps so we can compute checkpoints for (step-1, step)
    let max_wait_retries = 20;
    let mut i = 1;
    let initial_step_num = vdf_steps_guard.read().global_step;
    let mut step_num: u64 = 0;
    while i < max_wait_retries && step_num < initial_step_num + 2 {
        sleep(Duration::from_secs(1)).await;
        step_num = vdf_steps_guard.read().global_step;
        i += 1;
    }

    // Scan across VDF steps and chunk offsets within the recall range until we find a valid solution
    let partition_hash = H256::zero();
    let max_scan_steps: u64 = 100;
    let mut current_step = step_num;

    for _ in 0..max_scan_steps {
        // Get steps for (current_step - 1, current_step)
        let get_steps = {
            vdf_steps_guard
                .read()
                .get_steps(ii(current_step.saturating_sub(1), current_step))
        };
        let steps: H256List = match get_steps {
            Ok(s) => s,
            Err(_) => {
                // If steps are not yet available, wait briefly and try again
                sleep(Duration::from_millis(200)).await;
                continue;
            }
        };

        // Calculate last step checkpoints for current_step - 1
        let mut hasher = Sha256::new();
        let mut salt = irys_types::U256::from(step_number_to_salt_number(
            &config.vdf,
            current_step.saturating_sub(1),
        ));
        let mut seed = steps[0];

        let mut checkpoints: Vec<H256> =
            vec![H256::default(); config.vdf.num_checkpoints_in_vdf_step];

        vdf_sha(
            &mut hasher,
            &mut salt,
            &mut seed,
            config.vdf.num_checkpoints_in_vdf_step,
            config.vdf.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );

        // Determine recall range for this step
        let recall_range_idx = block_validation::get_recall_range(
            current_step,
            &config.consensus,
            &vdf_steps_guard,
            &partition_hash,
        )
        .expect("valid recall range");

        // Try each chunk within the recall range window
        for local_idx in 0..config.consensus.num_chunks_in_recall_range {
            let offset_u64 =
                (recall_range_idx as u64) * config.consensus.num_chunks_in_recall_range + local_idx;
            let partition_chunk_offset: u32 = offset_u64 as u32;

            // Compute the PoA chunk for this offset
            let mut entropy_chunk = Vec::<u8>::with_capacity(config.consensus.chunk_size as usize);
            compute_entropy_chunk(
                miner_addr,
                offset_u64,
                partition_hash.into(),
                config.consensus.entropy_packing_iterations,
                config.consensus.chunk_size as usize,
                &mut entropy_chunk,
                config.consensus.chain_id,
            );

            // solution_hash = sha256(poa_chunk || offset_le || vdf_output)
            let mut hasher_sol = Sha256::new();
            hasher_sol.update(&entropy_chunk);
            hasher_sol.update(partition_chunk_offset.to_le_bytes());
            hasher_sol.update(steps[1].as_bytes());
            let solution_hash = H256::from_slice(hasher_sol.finalize().as_slice());

            // Check difficulty: interpret hash as little-endian number
            let solution_val = irys_types::u256_from_le_bytes(&solution_hash.0);
            if solution_val >= difficulty {
                return SolutionContext {
                    partition_hash,
                    chunk_offset: partition_chunk_offset,
                    mining_address: miner_addr,
                    chunk: entropy_chunk,
                    vdf_step: current_step,
                    checkpoints: H256List(checkpoints),
                    seed: Seed(steps[1]),
                    solution_hash,
                    ..Default::default()
                };
            }
        }

        // Advance to next step and wait a bit to allow VDF to progress
        let next_step_target = current_step.saturating_add(1);
        let mut tries = 0_u8;
        while vdf_steps_guard.read().global_step < next_step_target && tries < 10 {
            sleep(Duration::from_millis(200)).await;
            tries += 1;
        }
        current_step = vdf_steps_guard.read().global_step.max(next_step_target);
    }

    // Fallback: if no valid solution found, return the best-effort solution for the latest step (may fail prevalidation)
    let steps: H256List = vdf_steps_guard
        .read()
        .get_steps(ii(current_step.saturating_sub(1), current_step))
        .expect("steps available for fallback");
    let recall_range_idx = block_validation::get_recall_range(
        current_step,
        &config.consensus,
        &vdf_steps_guard,
        &partition_hash,
    )
    .expect("valid recall range");
    let offset_u64 = (recall_range_idx as u64) * config.consensus.num_chunks_in_recall_range;
    let partition_chunk_offset: u32 = offset_u64 as u32;

    let mut entropy_chunk = Vec::<u8>::with_capacity(config.consensus.chunk_size as usize);
    compute_entropy_chunk(
        miner_addr,
        offset_u64,
        partition_hash.into(),
        config.consensus.entropy_packing_iterations,
        config.consensus.chunk_size as usize,
        &mut entropy_chunk,
        config.consensus.chain_id,
    );

    let mut hasher_sol = Sha256::new();
    hasher_sol.update(&entropy_chunk);
    hasher_sol.update(partition_chunk_offset.to_le_bytes());
    hasher_sol.update(steps[1].as_bytes());
    let solution_hash = H256::from_slice(hasher_sol.finalize().as_slice());

    SolutionContext {
        partition_hash,
        chunk_offset: partition_chunk_offset,
        mining_address: miner_addr,
        chunk: entropy_chunk,
        vdf_step: current_step,
        checkpoints: H256List(
            // recompute checkpoints for fallback
            {
                let mut h = Sha256::new();
                let mut s = irys_types::U256::from(step_number_to_salt_number(
                    &config.vdf,
                    current_step.saturating_sub(1),
                ));
                let mut sd = steps[0];
                let mut cps: Vec<H256> =
                    vec![H256::default(); config.vdf.num_checkpoints_in_vdf_step];
                vdf_sha(
                    &mut h,
                    &mut s,
                    &mut sd,
                    config.vdf.num_checkpoints_in_vdf_step,
                    config.vdf.num_iterations_per_checkpoint(),
                    &mut cps,
                );
                H256List(cps)
            }
            .0,
        ),
        seed: Seed(steps[1]),
        solution_hash,
        ..Default::default()
    }
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
    pub name: Option<String>,
    // Preserve caller intent for port allocation across restarts.
    restart_http_ports: bool,
    restart_gossip_ports: bool,
    restart_reth_ports: bool,
    /// Dedicated multi-thread runtime for test isolation.
    /// Created on start(), dropped on stop() via spawn_blocking.
    runtime: Option<tokio::runtime::Runtime>,
}

impl IrysNodeTest<()> {
    pub fn default_async() -> Self {
        let config = NodeConfig::testing();
        Self::new_genesis(config)
    }

    /// Start a new test node in peer-sync mode with full block validation
    pub fn new(mut config: NodeConfig) -> Self {
        // Do not override node_mode; allow caller to set consensus.expected_genesis_hash
        config.sync_mode = SyncMode::Full;
        Self::new_inner(config)
    }

    /// Start a new test node in genesis mode
    pub fn new_genesis(mut config: NodeConfig) -> Self {
        config.node_mode = NodeMode::Genesis;
        Self::new_inner(config)
    }

    fn new_inner(mut config: NodeConfig) -> Self {
        let temp_dir = temporary_directory(None, false);
        config.base_directory = temp_dir.path().to_path_buf();
        let restart_http_ports = config.http.bind_port == 0;
        let restart_gossip_ports = config.gossip.bind_port == 0;
        let restart_reth_ports =
            config.reth.network.use_random_ports || config.reth.network.bind_port == 0;
        Self {
            cfg: config,
            temp_dir,
            node_ctx: (),
            name: None,
            restart_http_ports,
            restart_gossip_ports,
            restart_reth_ports,
            runtime: None,
        }
    }

    #[diag_slow(state = "start".to_string())]
    pub async fn start(self) -> IrysNodeTest<IrysNodeCtx> {
        let span = self.get_span();
        let _enter = span.enter();
        let cfg_for_start = self.cfg.clone();
        let (cfg, http_listener, gossip_listener) =
            match IrysNode::bind_listeners(cfg_for_start.clone()) {
                Ok(bound) => bound,
                Err(err) => {
                    let addr_in_use = err.to_string().contains("Address already in use");
                    if !(addr_in_use && (self.restart_http_ports || self.restart_gossip_ports)) {
                        panic!("Failed to bind TCP listeners: {err:?}");
                    }

                    warn!(
                        error = ?err,
                        "Bind failed with address-in-use; retrying with fresh local ports"
                    );
                    let mut retry_cfg = cfg_for_start;
                    if self.restart_http_ports {
                        retry_cfg.http.bind_port = 0;
                        retry_cfg.http.public_port = 0;
                    }
                    if self.restart_gossip_ports {
                        retry_cfg.gossip.bind_port = 0;
                        retry_cfg.gossip.public_port = 0;
                    }
                    if self.restart_reth_ports {
                        retry_cfg.reth.network.bind_port = 0;
                        retry_cfg.reth.network.public_port = 0;
                        retry_cfg.reth.network.use_random_ports = true;
                    }
                    IrysNode::bind_listeners(retry_cfg).expect("Failed to bind TCP listeners")
                }
            };

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to build dedicated tokio runtime");

        let node = IrysNode::new_with_listeners(cfg, http_listener, gossip_listener)
            .expect("Failed to create IrysNode")
            .with_runtime_handle(runtime.handle().clone());

        let node_ctx = node.start().await.expect("node cannot be initialized");
        IrysNodeTest {
            cfg: node_ctx.config.node_config.clone(),
            node_ctx,
            temp_dir: self.temp_dir,
            name: self.name,
            restart_http_ports: self.restart_http_ports,
            restart_gossip_ports: self.restart_gossip_ports,
            restart_reth_ports: self.restart_reth_ports,
            runtime: Some(runtime),
        }
    }

    fn get_span(&self) -> tracing::Span {
        match &self.name {
            Some(name) => error_span!("NODE", node.name = %name),
            None => error_span!("NODE", node.name = "genesis"),
        }
    }

    pub fn with_name(mut self, name: &str) -> Self {
        self.name = Some(name.to_string());
        self
    }

    pub async fn start_with_name(self, log_name: &str) -> IrysNodeTest<IrysNodeCtx> {
        self.with_name(log_name).start().await
    }

    #[diag_slow(state = "start_and_wait_for_packing".to_string())]
    pub async fn start_and_wait_for_packing(
        self,
        log_name: &str,
        seconds_to_wait: usize,
    ) -> IrysNodeTest<IrysNodeCtx> {
        let span = error_span!("NODE", node.name = %log_name);
        let _enter = span.enter();
        let node = self.start().await;
        node.wait_for_packing(seconds_to_wait).await;
        node
    }
}

impl IrysNodeTest<IrysNodeCtx> {
    fn ensure_vdf_running_for_sync(&self, context: &str) {
        if !self
            .node_ctx
            .is_vdf_mining_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.node_ctx.start_vdf();
            info!("Auto-started VDF for sync context={}", context);
        }
    }

    async fn diag_wait_state(&self) -> String {
        let (
            canonical_tip_height,
            canonical_tip_hash,
            canonical_chain_len,
            not_onchain_count,
            max_diff_height,
            max_diff_hash,
        ) = {
            let tree = self.node_ctx.block_tree_guard.read();
            let (canonical_chain, not_onchain_count) = tree.get_canonical_chain();
            let canonical_tip = canonical_chain
                .last()
                .map(|entry| (entry.height(), entry.block_hash()));
            let (max_diff_height, max_diff_hash) = tree.get_max_cumulative_difficulty_block();

            (
                canonical_tip.map(|(height, _)| height),
                canonical_tip.map(|(_, block_hash)| block_hash),
                canonical_chain.len(),
                not_onchain_count,
                max_diff_height,
                max_diff_hash,
            )
        };

        let (
            block_index_height,
            block_index_hash,
            block_index_submit_total_chunks,
            block_index_publish_total_chunks,
        ) = {
            let block_index = self.node_ctx.block_index_guard.read();
            let latest = block_index.get_latest_item();

            let (submit_total_chunks, publish_total_chunks) = latest
                .as_ref()
                .map(|item| {
                    let submit_total = item
                        .ledgers
                        .iter()
                        .find(|ledger| ledger.ledger == DataLedger::Submit)
                        .map(|ledger| ledger.total_chunks)
                        .unwrap_or(0);
                    let publish_total = item
                        .ledgers
                        .iter()
                        .find(|ledger| ledger.ledger == DataLedger::Publish)
                        .map(|ledger| ledger.total_chunks)
                        .unwrap_or(0);
                    (submit_total, publish_total)
                })
                .unwrap_or((0, 0));

            (
                latest.as_ref().map(|_| block_index.latest_height()),
                latest.as_ref().map(|item| item.block_hash),
                submit_total_chunks,
                publish_total_chunks,
            )
        };

        let known_peers = self.node_ctx.peer_list.all_peers().iter().count();
        let gossip_broadcast = self.node_ctx.sync_state.is_gossip_broadcast_enabled();
        let gossip_reception = self.node_ctx.sync_state.is_gossip_reception_enabled();

        let reth_peer_count = match self
            .node_ctx
            .reth_node_adapter
            .inner
            .network
            .get_all_peers()
            .await
        {
            Ok(peers) => peers.len().to_string(),
            Err(err) => format!("error({err:#})"),
        };

        let reth_tip = match self
            .node_ctx
            .reth_node_adapter
            .reth_node
            .inner
            .eth_api()
            .block_by_number(BlockNumberOrTag::Latest, false)
            .await
        {
            Ok(Some(block)) => format!("{}@{}", block.header.number, block.header.hash),
            Ok(None) => "none".to_string(),
            Err(err) => format!("error({err:#})"),
        };

        let canonical_tip = canonical_tip_height
            .zip(canonical_tip_hash)
            .map(|(height, block_hash)| format!("{}@{}", height, block_hash))
            .unwrap_or_else(|| "none".to_string());

        let block_index_tip = block_index_height
            .zip(block_index_hash)
            .map(|(height, block_hash)| format!("{}@{}", height, block_hash))
            .unwrap_or_else(|| "none".to_string());

        format!(
            "node={:?} canonical_tip={} canonical_len={} not_onchain={} max_diff={}@{} index_tip={} index_submit_chunks={} index_publish_chunks={} peer_list={} reth_peers={} reth_tip={} gossip_tx={} gossip_rx={}",
            self.name,
            canonical_tip,
            canonical_chain_len,
            not_onchain_count,
            max_diff_height,
            max_diff_hash,
            block_index_tip,
            block_index_submit_total_chunks,
            block_index_publish_total_chunks,
            known_peers,
            reth_peer_count,
            reth_tip,
            gossip_broadcast,
            gossip_reception,
        )
    }

    pub async fn sync_state_snapshot(&self) -> String {
        self.diag_wait_state().await
    }

    /// Returns true if the next block height is in the last quarter of the pricing interval.
    pub fn ema_next_block_in_last_quarter(
        next_block_height: u64,
        price_adjustment_interval: u64,
    ) -> bool {
        // last_quarter_start = interval - ceil(interval/4)
        let last_quarter_start =
            price_adjustment_interval.saturating_sub(price_adjustment_interval.div_ceil(4));
        (next_block_height % price_adjustment_interval) >= last_quarter_start
    }

    /// Mine 2x price adjustment interval blocks to move public pricing off genesis EMA.
    pub async fn mine_two_ema_intervals(&self, price_adjustment_interval: u64) -> eyre::Result<()> {
        self.mine_blocks((price_adjustment_interval * 2) as usize)
            .await
    }

    /// Mine until the next block would fall into the last quarter of the pricing interval
    /// and return the current tip header when the condition is met.
    pub async fn ema_mine_until_next_in_last_quarter(
        &self,
        price_adjustment_interval: u64,
    ) -> eyre::Result<IrysBlockHeader> {
        loop {
            let current_tip_height = self.get_canonical_chain_height().await;
            let next_height = current_tip_height + 1;
            if Self::ema_next_block_in_last_quarter(next_height, price_adjustment_interval) {
                return self.get_block_by_height(current_tip_height).await;
            }
            self.mine_block().await?;
        }
    }

    /// Mine until the next block would NOT fall into the last quarter of the pricing interval
    /// and return the current tip header when the condition is met.
    pub async fn ema_mine_until_next_not_in_last_quarter(
        &self,
        price_adjustment_interval: u64,
    ) -> eyre::Result<IrysBlockHeader> {
        loop {
            let current_tip_height = self.get_canonical_chain_height().await;
            let next_height = current_tip_height + 1;
            if !Self::ema_next_block_in_last_quarter(next_height, price_adjustment_interval) {
                return self.get_block_by_height(current_tip_height).await;
            }
            self.mine_block().await?;
        }
    }
    /// Waits for the provided future to resolve, and if it doesn't after `timeout_duration`,
    /// mines a single block on this node and waits again.
    /// Designed for use with calls that expect to be able to send and confirm a tx in a single future.
    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn future_or_mine_on_timeout<F, T>(
        &self,
        future: F,
        timeout_duration: Duration,
    ) -> eyre::Result<T>
    where
        F: Future<Output = T>,
    {
        tokio::pin!(future);
        loop {
            let race = select(future.as_mut(), Box::pin(sleep(timeout_duration))).await;
            match race {
                // provided future finished
                futures::future::Either::Left((res, _)) => return Ok(res),
                // we need another block
                futures::future::Either::Right(_) => {
                    info!("deployment timed out, creating new block..")
                }
            };
            // Mine a single block (with payload) on this node and continue waiting
            let _ = self.mine_block_with_payload().await?;
        }
    }

    pub fn testing_peer(&self) -> NodeConfig {
        let node_config = &self.node_ctx.config.node_config;
        // Initialize the peer with a random signer, copying the genesis config
        let peer_signer = IrysSigner::random_signer(&node_config.consensus_config());
        self.testing_peer_with_signer(&peer_signer)
    }

    pub fn testing_peer_with_signer(&self, peer_signer: &IrysSigner) -> NodeConfig {
        use irys_types::{PeerAddress, RethPeerInfo};

        let node_config = &self.node_ctx.config.node_config;

        if matches!(node_config.node_mode, NodeMode::Peer) {
            panic!("Can only create a peer from a genesis config");
        }

        let mut peer_config = node_config.clone();
        peer_config.mining_key = peer_signer.signer.clone();
        peer_config.reward_address = peer_signer.address();

        // Set peer mode and expected genesis hash via consensus config
        peer_config.node_mode = NodeMode::Peer;
        peer_config.consensus.get_mut().expected_genesis_hash = Some(self.node_ctx.genesis_hash);

        // Make sure this peer does port randomization instead of copying the genesis ports
        peer_config.http.bind_port = 0;
        peer_config.http.public_port = 0;
        peer_config.gossip.bind_port = 0;
        peer_config.gossip.public_port = 0;

        // Make sure to mark this config as a peer (already set above with consensus hash)
        peer_config.sync_mode = SyncMode::Full;

        // Add the genesis node details as a trusted peer
        peer_config.trusted_peers = vec![
            (PeerAddress {
                api: format!(
                    "{}:{}",
                    node_config.http.public_ip(&node_config.network_defaults),
                    node_config.http.public_port
                )
                .parse()
                .expect("valid SocketAddr expected"),
                gossip: format!(
                    "{}:{}",
                    node_config.gossip.public_ip(&node_config.network_defaults),
                    node_config.gossip.public_port
                )
                .parse()
                .expect("valid SocketAddr expected"),
                execution: RethPeerInfo::default(),
            }),
        ];
        peer_config
    }

    /// Create a new peer config with partition assignments
    /// This will start the peer node and wait for the peer to sync the commitment block
    /// It will mine all the blocks necessary to reach the next epoch round.
    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn testing_peer_with_assignments(
        &self,
        peer_signer: &IrysSigner,
    ) -> eyre::Result<Self> {
        // Create a new peer config using the provided signer
        let peer_config = self.testing_peer_with_signer(peer_signer);

        self.testing_peer_with_assignments_and_name(peer_config, "PEER")
            .await
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn testing_peer_with_assignments_and_name(
        &self,
        config: NodeConfig,
        name: &'static str,
    ) -> eyre::Result<Self> {
        let seconds_to_wait = 20;
        let peer_address = config.miner_address();

        // Start the peer node
        let peer_node = IrysNodeTest::new(config).start_with_name(name).await;

        // Post stake + pledge commitments to establish validator status
        let stake_tx = peer_node.post_stake_commitment(None).await?;
        let pledge_tx = peer_node.post_pledge_commitment(None).await?;

        // Wait for commitment transactions to show up in this node's mempool
        self.wait_for_mempool(stake_tx.id(), seconds_to_wait)
            .await
            .expect("stake tx to be in mempool");
        self.wait_for_mempool(pledge_tx.id(), seconds_to_wait)
            .await
            .expect("pledge tx to be in mempool");

        // Get height before mining the commitment block
        let height_before_commitment = self.get_canonical_chain_height().await;

        // Mine a block to get the commitments included
        self.mine_block()
            .await
            .expect("to mine block with commitments");

        // Wait for peer to sync the commitment block
        peer_node
            .wait_for_block_at_height(height_before_commitment + 1, seconds_to_wait)
            .await
            .expect("peer to sync commitment block");

        // Get epoch configuration to calculate when next epoch round occurs
        let num_blocks_in_epoch = self.node_ctx.config.consensus.epoch.num_blocks_in_epoch;
        let current_height_after_commitment = self.get_canonical_chain_height().await;

        // Calculate how many blocks we need to mine to reach the next epoch
        let blocks_until_next_epoch =
            num_blocks_in_epoch - (current_height_after_commitment % num_blocks_in_epoch);

        // Mine blocks until we reach the next epoch round
        for _ in 0..blocks_until_next_epoch {
            let height_before_mining = self.get_canonical_chain_height().await;

            self.mine_block()
                .await
                .expect("to mine block towards next epoch");

            // Wait for peer to sync after each block to prevent race conditions
            peer_node
                .wait_for_block_at_height(height_before_mining + 1, seconds_to_wait)
                .await
                .expect("peer to sync to current height");
        }

        let final_height = self.get_canonical_chain_height().await;

        // Wait for the peer to receive & process the epoch block
        peer_node
            .wait_for_block_at_height(final_height, seconds_to_wait)
            .await
            .expect("peer to sync to epoch height");
        self.wait_for_block_at_height(final_height, seconds_to_wait)
            .await
            .unwrap();

        // Wait for packing to complete on the peer (this indicates partition assignments are active)
        peer_node.wait_for_packing(seconds_to_wait).await;

        // Verify that partition assignments were created
        let peer_assignments = peer_node.get_partition_assignments(peer_address);

        // Ensure at least one partition has been assigned
        assert!(
            !peer_assignments.is_empty(),
            "Peer should have at least one partition assignment"
        );

        Ok(peer_node)
    }

    /// get block height in block index
    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn wait_until_block_index_height(
        &self,
        target_height: u64,
        max_seconds: usize,
    ) -> eyre::Result<()> {
        self.ensure_vdf_running_for_sync("wait_until_block_index_height");
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
                "Failed to reach target index height of {} after {} retries",
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

    /// Polls block-index block-bounds lookup until the requested chunk offset
    /// is resolvable for the given ledger, or times out.
    #[diag_slow(state = format!(
        "ledger={:?} chunk_offset={} {}",
        ledger,
        u64::from(chunk_offset),
        self.diag_wait_state().await
    ))]
    pub async fn wait_until_block_bounds_available(
        &self,
        ledger: DataLedger,
        chunk_offset: LedgerChunkOffset,
        max_seconds: usize,
    ) -> eyre::Result<()> {
        self.ensure_vdf_running_for_sync("wait_until_block_bounds_available");
        let chunk_offset_u64: u64 = chunk_offset.into();
        let mut last_error = "none".to_string();

        for attempt in 1..=max_seconds {
            let lookup = self
                .node_ctx
                .block_index_guard
                .read()
                .get_block_bounds(ledger, LedgerChunkOffset::from(chunk_offset_u64));

            match lookup {
                Ok(bounds) => {
                    info!(
                        "block bounds available for {:?} offset {} after {} attempt(s): [{}, {}) at height {}",
                        ledger,
                        chunk_offset_u64,
                        attempt,
                        bounds.start_chunk_offset,
                        bounds.end_chunk_offset,
                        bounds.height
                    );
                    return Ok(());
                }
                Err(err) => {
                    last_error = format!("{err:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }

        let state = self.diag_wait_state().await;
        Err(eyre::eyre!(
            "Failed waiting for block bounds for ledger {:?} offset {} after {} seconds; last_error={}; state: {}",
            ledger,
            chunk_offset_u64,
            max_seconds,
            last_error,
            state
        ))
    }

    /// Polls `get_block_by_height_from_index` (which checks BOTH the index
    /// entry AND the DB header) until it succeeds or the timeout expires.
    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn wait_for_block_in_index(
        &self,
        height: u64,
        include_chunk: bool,
        max_seconds: usize,
    ) -> eyre::Result<IrysBlockHeader> {
        self.ensure_vdf_running_for_sync("wait_for_block_in_index");
        for _attempt in 1..=max_seconds {
            if let Ok(block) = self.get_block_by_height_from_index(height, include_chunk) {
                return Ok(block);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Err(eyre::eyre!(
            "block at height {} not found in index after {}s",
            height,
            max_seconds
        ))
    }

    /// Like [`wait_for_block_in_index`] but only waits for the block to
    /// appear — does not fetch the chunk or return the header.
    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn wait_for_block_in_index_height(
        &self,
        height: u64,
        max_seconds: usize,
    ) -> eyre::Result<()> {
        self.wait_for_block_in_index(height, false, max_seconds)
            .await?;
        Ok(())
    }

    /// Returns a future that resolves once no [`BlockStateUpdated`] events
    /// arrive for `idle` duration, bounded by `deadline`.
    ///
    /// **Call this before the action that produces events** so the
    /// subscription captures everything. Perform the action, then `.await`
    /// the returned future.
    ///
    /// ```ignore
    /// let idle = node.wait_until_block_events_idle(
    ///     Duration::from_millis(500),
    ///     Duration::from_secs(10),
    /// );
    /// genesis.gossip_block_to_peers(&block)?;
    /// idle.await;
    /// ```
    pub fn wait_until_block_events_idle(
        &self,
        idle: Duration,
        deadline: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let mut rx = self
            .node_ctx
            .service_senders
            .subscribe_block_state_updates();
        Box::pin(async move {
            let deadline = tokio::time::Instant::now() + deadline;
            irys_actors::services::wait_until_broadcast_idle(&mut rx, idle, deadline).await;
        })
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn wait_for_packing(&self, seconds_to_wait: usize) {
        self.ensure_vdf_running_for_sync("wait_for_packing");
        self.node_ctx
            .packing_waiter
            .wait_for_idle(Some(Duration::from_secs(seconds_to_wait as u64)))
            .await
            .expect("for packing to complete in the wait period");
    }

    pub fn start_mining(&self) {
        if self.node_ctx.start_mining().is_err() {
            panic!("Expected to start mining")
        }
    }

    pub fn stop_mining(&self) {
        if self.node_ctx.stop_mining().is_err() {
            panic!("Expected to stop mining")
        }
    }

    #[must_use]
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

    pub async fn start_mock_gossip_server(
        &self,
    ) -> impl Service<Request, Response = ServiceResponse<BoxBody>, Error = Error> {
        let gossip_server = self.node_ctx.get_gossip_server();

        actix_web::test::init_service(
            App::new()
                // Remove the logger middleware
                .app_data(actix_web::web::Data::new(gossip_server))
                .service(GossipServer::<
                    MempoolServiceFacadeImpl,
                    BlockDiscoveryFacadeImpl,
                >::routes()),
        )
        .await
    }

    #[tracing::instrument(level = "trace", skip_all)]
    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn wait_until_height(
        &self,
        target_height: u64,
        max_seconds: usize,
    ) -> eyre::Result<H256> {
        self.ensure_vdf_running_for_sync("wait_until_height");
        let mut retries = 0;
        let max_retries = max_seconds; // 1 second per retry

        loop {
            let canonical_chain = get_canonical_chain(self.node_ctx.block_tree_guard.clone())
                .await
                .unwrap();
            let latest_block = canonical_chain.0.last().unwrap();

            if latest_block.height() >= target_height {
                // Get the specific block at target height, not the latest
                if let Some(target_block) = canonical_chain
                    .0
                    .iter()
                    .find(|b| b.height() == target_height)
                {
                    info!(
                        "reached height {} after {} retries",
                        target_height, &retries
                    );
                    return Ok(target_block.block_hash());
                } else {
                    return Err(eyre::eyre!(
                        "Block at height {} not found in canonical chain",
                        target_height
                    ));
                }
            }

            eyre::ensure!(
                retries < max_retries,
                "Failed to reach target height {} after {} retries",
                target_height,
                retries
            );

            sleep(Duration::from_secs(1)).await;
            retries += 1;
        }
    }

    /// Wait for a canonical block at `target_height` using a hybrid strategy:
    /// short-interval canonical polling plus BlockStateUpdated subscription.
    #[tracing::instrument(level = "trace", skip_all)]
    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn wait_for_block_at_height(
        &self,
        target_height: u64,
        max_seconds: usize,
    ) -> eyre::Result<H256> {
        self.ensure_vdf_running_for_sync("wait_for_block_at_height");
        // Subscribe to block state updates
        let mut block_state_rx = self
            .node_ctx
            .service_senders
            .subscribe_block_state_updates();

        // Set timeout
        let timeout = Duration::from_secs(max_seconds as u64);
        let deadline = Instant::now() + timeout;

        // Hybrid wait strategy: poll canonical chain plus event stream.
        // Relying only on events is brittle because a target-height event can arrive
        // before the block becomes canonical, and no later event may be emitted.
        loop {
            let canonical_chain =
                get_canonical_chain(self.node_ctx.block_tree_guard.clone()).await?;
            if let Some(block) = canonical_chain
                .0
                .iter()
                .find(|b| b.height() == target_height)
            {
                info!("Canonical block at height {} is available", target_height);
                return Ok(block.block_hash());
            }

            // Check timeout
            if Instant::now() > deadline {
                let state = self.diag_wait_state().await;
                return Err(eyre::eyre!(
                    "Timeout waiting for block at height {} after {} seconds; state: {}",
                    target_height,
                    max_seconds,
                    state
                ));
            }

            // Wait briefly for block-state updates so we can re-check canonical
            // state even if no further events arrive.
            let now = Instant::now();
            let remaining = deadline.saturating_duration_since(now);
            let wait_slice = remaining.min(Duration::from_secs(1));

            match tokio::time::timeout(wait_slice, block_state_rx.recv()).await {
                Ok(Ok(event)) => {
                    if event.height >= target_height && !event.discarded {
                        info!(
                            "Observed block_state update at/above target height: event_height={} target_height={} block_hash={}",
                            event.height, target_height, event.block_hash
                        );
                    }
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                    warn!("block_state receiver lagged; skipped {skipped} events, re-polling");
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    return Err(eyre::eyre!("Block state channel closed"));
                }
                // No event in this short slice, loop and poll canonical state again.
                Err(_) => {}
            }
        }
    }

    #[diag_slow(state = format!(
        "target_step={} {}",
        target_step,
        self.diag_wait_state().await
    ))]
    pub async fn wait_for_vdf_step(
        &self,
        target_step: u64,
        max_seconds: usize,
    ) -> eyre::Result<()> {
        self.ensure_vdf_running_for_sync("wait_for_vdf_step");
        let deadline = Instant::now() + Duration::from_secs(max_seconds as u64);
        loop {
            let current_step = self.node_ctx.vdf_steps_guard.read().global_step;
            if current_step >= target_step {
                info!("VDF step {} reached target {}", current_step, target_step);
                return Ok(());
            }

            if Instant::now() > deadline {
                let state = self.diag_wait_state().await;
                return Err(eyre::eyre!(
                    "Timeout waiting for vdf step >= {} after {} seconds (current_step={}); state: {}",
                    target_step,
                    max_seconds,
                    current_step,
                    state
                ));
            }

            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn wait_until_height_confirmed(
        &self,
        target_height: u64,
        max_seconds: usize,
    ) -> eyre::Result<H256> {
        self.ensure_vdf_running_for_sync("wait_until_height_confirmed");
        let mut retries = 0;
        let max_retries = max_seconds; // 1 second per retry

        loop {
            let canonical_chain = get_canonical_chain(self.node_ctx.block_tree_guard.clone())
                .await
                .unwrap();
            let latest_block = canonical_chain.0.last().unwrap();

            let latest_height = latest_block.height();
            let not_onchain_count = canonical_chain.1 as u64;
            if (latest_height - not_onchain_count) >= target_height {
                info!(
                    "reached height {} after {} retries",
                    target_height, &retries
                );

                return Ok(latest_block.block_hash());
            }

            if retries >= max_retries {
                return Err(eyre::eyre!(
                    "Failed to reach target confirmed height {} after {} retries",
                    target_height,
                    retries
                ));
            }

            sleep(Duration::from_secs(1)).await;
            retries += 1;
        }
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn wait_for_chunk<T, B>(
        &self,
        app: &T,
        ledger: DataLedger,
        offset: i32,
        seconds: usize,
    ) -> eyre::Result<()>
    where
        T: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
        B: MessageBody,
    {
        let delay = Duration::from_secs(1);
        for attempt in 1..=seconds {
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

    pub fn get_storage_module_intervals(
        &self,
        ledger: DataLedger,
        slot_index: usize,
        chunk_type: ChunkType,
    ) -> Vec<Interval<PartitionChunkOffset>> {
        let sms = self.node_ctx.storage_modules_guard.read();
        for sm in sms.iter() {
            let Some(pa) = sm.partition_assignment() else {
                continue;
            };
            if pa.ledger_id == Some(ledger.into()) && pa.slot_index == Some(slot_index) {
                return sm.get_intervals(chunk_type);
            }
        }
        Vec::new()
    }

    /// check number of chunks in the CachedChunks table
    /// return Ok(()) once it matches the expected value
    #[diag_slow(state = self.diag_wait_state().await)]
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
    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn wait_for_migrated_txs(
        &self,
        mut unconfirmed_txs: Vec<DataTransactionHeader>,
        seconds: usize,
    ) -> eyre::Result<()> {
        let delay = Duration::from_secs(1);
        for attempt in 1..=seconds {
            if unconfirmed_txs.is_empty() {
                return Ok(());
            }

            // Check all remaining transactions
            let ro_tx = self
                .node_ctx
                .db
                .as_ref()
                .tx()
                .map_err(|e| {
                    tracing::error!("Failed to create mdbx transaction: {}", e);
                })
                .unwrap();
            let mut found_ids: HashSet<IrysTransactionId> = HashSet::new();
            for tx in unconfirmed_txs.iter() {
                if let Ok(Some(header)) = tx_header_by_txid(&ro_tx, &tx.id) {
                    // the proofs may be added to the tx during promotion
                    // and so we cant do a direct comparison
                    // we can however check some key fields are equal
                    assert_eq!(tx.id, header.id);
                    assert_eq!(tx.anchor, header.anchor);
                    tracing::info!(
                        "Transaction {:?} was retrieved ok after {} attempts",
                        tx.id,
                        attempt
                    );
                    found_ids.insert(tx.id);
                }
            }
            drop(ro_tx);

            // Remove all found transactions
            if !found_ids.is_empty() {
                unconfirmed_txs.retain(|t| !found_ids.contains(&t.id));
            }

            if !unconfirmed_txs.is_empty() {
                self.mine_block().await?;
                sleep(delay).await;
            }
        }
        Err(eyre::eyre!(
            "Failed waiting for migrated txs. Waited {} seconds",
            seconds,
        ))
    }

    /// wait for data tx to be in mempool and it's IngressProofs to be in database
    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn wait_for_ingress_proofs(
        &self,
        unconfirmed_promotions: Vec<H256>,
        seconds: usize,
    ) -> eyre::Result<()> {
        self.wait_for_ingress_proofs_inner(unconfirmed_promotions, seconds, true, 1)
            .await
    }

    /// wait for data tx to be in mempool and its IngressProofs to be in database. does this without mining new blocks.
    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn wait_for_ingress_proofs_no_mining(
        &self,
        unconfirmed_promotions: Vec<H256>,
        seconds: usize,
    ) -> eyre::Result<()> {
        self.wait_for_ingress_proofs_inner(unconfirmed_promotions, seconds, false, 1)
            .await
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn wait_for_multiple_ingress_proofs_no_mining(
        &self,
        unconfirmed_promotions: Vec<H256>,
        num_proofs: usize,
        seconds: usize,
    ) -> eyre::Result<()> {
        self.wait_for_ingress_proofs_inner(unconfirmed_promotions, seconds, false, num_proofs)
            .await
    }

    /// wait for data tx to be in mempool and it's IngressProofs to be in database
    async fn wait_for_ingress_proofs_inner(
        &self,
        mut unconfirmed_promotions: Vec<H256>,
        seconds: usize,
        mine_blocks: bool,
        num_proofs: usize,
    ) -> eyre::Result<()> {
        tracing::info!(
            "waiting up to {} seconds for unconfirmed_promotions: {:?}",
            seconds,
            unconfirmed_promotions
        );
        for _ in 1..=seconds {
            // Do we have any unconfirmed promotions?
            if unconfirmed_promotions.is_empty() {
                // if not return as we are done
                return Ok(());
            }

            // Snapshot ingress proofs from DB into a map by data_root and drop the read transaction
            let ingress_proofs_by_root = {
                let ro_tx = self
                    .node_ctx
                    .db
                    .as_ref()
                    .tx()
                    .map_err(|e| {
                        tracing::error!("Failed to create mdbx transaction: {}", e);
                    })
                    .unwrap();
                let proofs = walk_all::<IngressProofs, _>(&ro_tx).unwrap();
                let mut map: HashMap<_, Vec<_>> = HashMap::new();
                for (data_root, proof) in proofs {
                    map.entry(data_root).or_default().push(proof);
                }
                map
            };

            // Retrieve the transaction headers for all pending txids in a single batch
            let to_check: Vec<H256> = unconfirmed_promotions.clone();
            let headers = {
                let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
                self.node_ctx.service_senders.mempool.send_traced(
                    MempoolServiceMessage::GetDataTxs(to_check.clone(), oneshot_tx),
                )?;
                oneshot_rx.await.unwrap()
            };

            // Track which txids have met the required number of proofs
            let mut to_remove: HashSet<H256> = HashSet::new();

            for (idx, maybe_header) in headers.iter().enumerate() {
                if let Some(tx_header) = maybe_header {
                    if let Some(tx_proofs) = ingress_proofs_by_root.get(&tx_header.data_root) {
                        if tx_proofs.len() >= num_proofs {
                            for ingress_proof in tx_proofs.iter() {
                                assert_eq!(ingress_proof.proof.data_root, tx_header.data_root);
                                tracing::info!(
                                    "proof {} signer: {}",
                                    ingress_proof.proof.id(),
                                    ingress_proof.address
                                );
                            }
                            to_remove.insert(to_check[idx]);
                        }
                    }
                }
            }

            // Remove satisfied txids and early-exit if none remain
            if !to_remove.is_empty() {
                unconfirmed_promotions.retain(|id| !to_remove.contains(id));
            }
            if unconfirmed_promotions.is_empty() {
                return Ok(());
            }
            if mine_blocks {
                self.mine_block().await?;
            }
            sleep(Duration::from_secs(1)).await;
        }

        Err(eyre::eyre!(
            "Failed waiting {} for ingress proofs. Waited {} seconds",
            num_proofs,
            seconds,
        ))
    }

    pub fn get_block_index_height(&self) -> u64 {
        self.node_ctx.block_index_guard.read().latest_height()
    }

    pub async fn get_canonical_chain_height(&self) -> u64 {
        get_canonical_chain(self.node_ctx.block_tree_guard.clone())
            .await
            .unwrap()
            .0
            .last()
            .unwrap()
            .height()
    }

    pub fn get_max_difficulty_block(&self) -> IrysBlockHeader {
        let block = self
            .node_ctx
            .block_tree_guard
            .read()
            .get_max_cumulative_difficulty_block()
            .1;
        self.node_ctx
            .block_tree_guard
            .read()
            .get_block(&block)
            .unwrap()
            .clone()
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    async fn wait_for_reorg_subscribed(
        &self,
        mut reorg_rx: tokio::sync::broadcast::Receiver<ReorgEvent>,
        seconds_to_wait: usize,
        start_tip: H256,
    ) -> eyre::Result<ReorgEvent> {
        let timeout_duration = Duration::from_secs(seconds_to_wait as u64);
        match tokio::time::timeout(timeout_duration, reorg_rx.recv()).await {
            Ok(Ok(reorg_event)) => {
                info!(
                    "Reorg detected: {} blocks in old fork, {} in new fork, fork at height {}, new tip: {}",
                    reorg_event.old_fork.len(),
                    reorg_event.new_fork.len(),
                    reorg_event.fork_parent.height,
                    reorg_event.new_tip
                );
                Ok(reorg_event)
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => Err(eyre::eyre!(
                "Reorg broadcast receiver lagged and skipped {} events",
                skipped
            )),
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                Err(eyre::eyre!("Reorg broadcast channel closed"))
            }
            Err(_) => {
                let current_tip = self.get_max_difficulty_block().block_hash;
                let state = self.diag_wait_state().await;
                Err(eyre::eyre!(
                    "Timeout: No reorg event received within {} seconds (start_tip={}, current_tip={}). State: {}",
                    seconds_to_wait,
                    start_tip,
                    current_tip,
                    state
                ))
            }
        }
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
    #[instrument(skip_all)]
    pub fn wait_for_reorg(
        &self,
        seconds_to_wait: usize,
    ) -> impl Future<Output = eyre::Result<ReorgEvent>> + '_ {
        self.ensure_vdf_running_for_sync("wait_for_reorg");
        // Subscribe immediately so callers can safely await later without missing events.
        let reorg_rx = self.node_ctx.service_senders.subscribe_reorgs();
        let start_tip = self.get_max_difficulty_block().block_hash;
        self.wait_for_reorg_subscribed(reorg_rx, seconds_to_wait, start_tip)
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn mine_block(&self) -> eyre::Result<IrysBlockHeader> {
        let height = self.get_max_difficulty_block().height;
        self.mine_blocks(1).await?;
        let hash = self.wait_for_block_at_height(height + 1, 10).await?;
        self.get_block_by_hash(&hash)
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn mine_blocks(&self, num_blocks: usize) -> eyre::Result<()> {
        self.node_ctx
            .service_senders
            .block_producer
            .send_traced(BlockProducerCommand::SetTestBlocksRemaining(Some(
                num_blocks as u64,
            )))
            .unwrap();
        let height = self.get_max_difficulty_block().height;
        self.node_ctx.start_mining()?;
        let _block_hash = self
            .wait_for_block_at_height(height + num_blocks as u64, 60 * num_blocks)
            .await?;
        // stop mining immediately after reaching the correct height
        let stop_mining_result = self.node_ctx.stop_mining();
        // Keep the test mining guard active at Some(0) to avoid a queued SolutionFound
        // being accepted after stop_mining but before the guard is reset. The next call
        // to mine_blocks() will override this with Some(n).
        self.node_ctx
            .service_senders
            .block_producer
            .send_traced(BlockProducerCommand::SetTestBlocksRemaining(Some(0)))
            .unwrap();
        stop_mining_result
    }

    pub async fn mine_blocks_without_gossip(&self, num_blocks: usize) -> eyre::Result<()> {
        self.with_gossip_disabled(self.mine_blocks(num_blocks))
            .await
    }

    pub async fn mine_block_with_payload(
        &self,
    ) -> eyre::Result<(Arc<IrysBlockHeader>, EthBuiltPayload, BlockTransactions)> {
        // Ensure exactly one block is allowed even if a previous call set the guard to Some(0)
        self.node_ctx
            .service_senders
            .block_producer
            .send_traced(BlockProducerCommand::SetTestBlocksRemaining(Some(1)))
            .unwrap();

        let poa_solution = solution_context(&self.node_ctx).await?;
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.node_ctx
            .service_senders
            .block_producer
            .send_traced(BlockProducerCommand::SolutionFound {
                solution: poa_solution,
                response: response_tx,
            })
            .unwrap();
        let res = response_rx.await?;
        let maybe = res?;
        // Reset the guard to Some(0) to avoid unintended mining beyond this call
        self.node_ctx
            .service_senders
            .block_producer
            .send_traced(BlockProducerCommand::SetTestBlocksRemaining(Some(0)))
            .unwrap();
        let (sealed_block, eth_payload) = maybe.ok_or_eyre("block not returned")?;
        Ok((
            sealed_block.header().clone(),
            eth_payload,
            BlockTransactions::clone(sealed_block.transactions()),
        ))
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn mine_block_without_gossip(
        &self,
    ) -> eyre::Result<(Arc<IrysBlockHeader>, EthBuiltPayload, BlockTransactions)> {
        self.with_gossip_disabled(self.mine_block_with_payload())
            .await
    }

    pub async fn mine_block_and_wait_for_validation(
        &self,
    ) -> eyre::Result<(
        Arc<IrysBlockHeader>,
        EthBuiltPayload,
        BlockTransactions,
        BlockValidationOutcome,
    )> {
        let event_rx = self
            .node_ctx
            .service_senders
            .subscribe_block_state_updates();
        let (block, reth_payload, block_transactions) = self.mine_block_with_payload().await?;
        let block_hash = &block.block_hash;
        let res = read_block_from_state(&self.node_ctx, block_hash, event_rx).await;
        Ok((block, reth_payload, block_transactions, res))
    }

    /// Mine blocks until the next epoch boundary is reached.
    /// Returns the number of blocks mined and the final height.
    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn mine_until_next_epoch(&self) -> eyre::Result<(usize, u64)> {
        let num_blocks_in_epoch = self.node_ctx.config.consensus.epoch.num_blocks_in_epoch;
        let current_height = self.get_canonical_chain_height().await;

        // Calculate how many blocks we need to mine to reach the next epoch
        let blocks_until_next_epoch = num_blocks_in_epoch - (current_height % num_blocks_in_epoch);

        info!(
            "Mining {} blocks to reach next epoch (current height: {}, epoch size: {})",
            blocks_until_next_epoch, current_height, num_blocks_in_epoch
        );

        // Mine blocks until we reach the next epoch boundary
        for i in 0..blocks_until_next_epoch {
            self.mine_block().await?;
            info!("Mined block {} of {}", i + 1, blocks_until_next_epoch);
        }

        let final_height = self.get_canonical_chain_height().await;
        Ok((blocks_until_next_epoch as usize, final_height))
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

        let epoch_snapshot = self
            .node_ctx
            .block_tree_guard
            .read()
            .canonical_epoch_snapshot();
        commitment_snapshot.get_commitment_status(commitment_tx, &epoch_snapshot)
    }

    /// wait for specific block to be available via block tree guard
    ///   i.e. in the case of a fork, check a specific block has been gossiped between peers,
    ///        even though it may not become part of the canonical chain.
    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn wait_for_block(
        &self,
        hash: &H256,
        seconds_to_wait: usize,
    ) -> eyre::Result<IrysBlockHeader> {
        self.ensure_vdf_running_for_sync("wait_for_block");
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

    #[diag_slow(state = format!(
        "txid={} ledger={:?} {}",
        txid,
        ledger,
        self.diag_wait_state().await
    ))]
    pub async fn wait_for_block_containing_tx(
        &self,
        txid: H256,
        ledger: DataLedger,
        max_seconds: usize,
    ) -> eyre::Result<IrysBlockHeader> {
        self.ensure_vdf_running_for_sync("wait_for_block_containing_tx");
        for attempt in 1..=max_seconds {
            let canonical_chain =
                get_canonical_chain(self.node_ctx.block_tree_guard.clone()).await?;
            for entry in canonical_chain.0.iter().rev() {
                let block_hash = entry.block_hash();
                if let Ok(block) = self.get_block_by_hash(&block_hash) {
                    if block.data_ledgers[ledger].tx_ids.0.contains(&txid) {
                        tracing::info!(
                            "found block containing tx {} on {} after {} attempt(s)",
                            txid,
                            self.name.clone().unwrap_or_else(|| "genesis".to_string()),
                            attempt
                        );
                        return Ok(block);
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        Err(eyre::eyre!(
            "block containing tx {} not found on {} after {} seconds",
            txid,
            self.name.clone().unwrap_or_else(|| "genesis".to_string()),
            max_seconds
        ))
    }

    pub fn get_evm_block_by_hash(
        &self,
        hash: alloy_core::primitives::B256,
    ) -> eyre::Result<reth_ethereum_primitives::Block> {
        use reth::providers::BlockReader as _;

        self.node_ctx
            .reth_handle
            .provider
            .block_by_hash(hash)?
            .ok_or_eyre("Got None")
    }

    // kept for future work so we can query EVM info from nodes remotely (via HTTP)
    pub async fn get_evm_block_by_hash2(
        &self,
        hash: alloy_core::primitives::B256,
    ) -> eyre::Result<alloy_rpc_types_eth::Block> {
        let client = self
            .node_ctx
            .reth_node_adapter
            .rpc_client()
            .ok_or_eyre("Unable to get RPC client")?;
        use alloy_primitives::Bytes;
        use alloy_rpc_types_eth::{Block, Header, Receipt, Transaction, TransactionRequest};
        reth::rpc::api::EthApiClient::<TransactionRequest, Transaction, Block, Receipt, Header, Bytes>::block_by_hash(
            &client, hash, true,
        )
        .await?
        .ok_or_eyre("Got None")
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn wait_for_evm_block(
        &self,
        hash: alloy_core::primitives::BlockHash,
        seconds_to_wait: usize,
    ) -> eyre::Result<reth_ethereum_primitives::Block> {
        self.ensure_vdf_running_for_sync("wait_for_evm_block");
        let retries_per_second = 50;
        let max_retries = seconds_to_wait * retries_per_second;
        for retry in 0..max_retries {
            if let Ok(block) = self.get_evm_block_by_hash(hash) {
                info!(
                    "block found in {:?} reth after {} retries",
                    &self.name, &retry
                );
                return Ok(block);
            }
            sleep(Duration::from_millis((1000 / retries_per_second) as u64)).await;
        }

        Err(eyre::eyre!(
            "Failed to locate block in {:?} reth after {} retries",
            &self.name,
            max_retries
        ))
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn assert_evm_block_absent(
        &self,
        hash: alloy_core::primitives::BlockHash,
        seconds_to_wait: usize,
    ) -> eyre::Result<()> {
        let retries_per_second = 50;
        let max_retries = seconds_to_wait * retries_per_second;
        for retry in 0..max_retries {
            if self.get_evm_block_by_hash(hash).is_ok() {
                let state = self.diag_wait_state().await;
                return Err(eyre::eyre!(
                    "Block {} unexpectedly appeared in {:?} reth after {} retries. State: {}",
                    hash,
                    &self.name,
                    retry,
                    state
                ));
            }
            sleep(Duration::from_millis((1000 / retries_per_second) as u64)).await;
        }
        Ok(())
    }

    async fn evm_tx_wait_diag_state(&self, hash: &alloy_core::primitives::B256) -> String {
        let state = self.diag_wait_state().await;
        format!("tx={} {}", hash, state)
    }

    #[diag_slow(interval = 2, state = self.evm_tx_wait_diag_state(hash).await)]
    pub async fn wait_for_evm_tx(
        &self,
        hash: &alloy_core::primitives::B256,
        seconds_to_wait: usize,
    ) -> eyre::Result<alloy_rpc_types_eth::Transaction> {
        self.ensure_vdf_running_for_sync("wait_for_evm_tx");
        use alloy_primitives::Bytes;
        use alloy_rpc_types_eth::{Block, Header, Receipt, Transaction, TransactionRequest};
        let retries_per_second = 50;
        let max_retries = seconds_to_wait * retries_per_second;

        // wait until the tx shows up
        let rpc = self
            .node_ctx
            .reth_node_adapter
            .rpc_client()
            .ok_or_eyre("Unable to get RPC client")?;
        let mut last_rpc_error: Option<String> = None;

        for retry in 0..max_retries {
            match reth::rpc::api::EthApiClient::<
                TransactionRequest,
                Transaction,
                Block,
                Receipt,
                Header,
                Bytes,
            >::transaction_by_hash(&rpc, *hash)
            .await
            {
                Ok(Some(tx)) => {
                    info!(
                        "tx {} found in {:?} reth after {} retries",
                        &hash, &self.name, &retry
                    );
                    return Ok(tx);
                }
                Ok(None) => {}
                Err(err) => {
                    let err_msg = format!("{err:#}");
                    if retry % retries_per_second == 0 {
                        tracing::warn!(
                            "failed to query tx {} in {:?} reth at retry {}: {}",
                            hash,
                            self.name,
                            retry,
                            err_msg
                        );
                    }
                    last_rpc_error = Some(err_msg);
                }
            }
            sleep(Duration::from_millis((1000 / retries_per_second) as u64)).await;
        }

        let final_state = self.evm_tx_wait_diag_state(hash).await;
        Err(eyre::eyre!(
            "Failed to locate tx {} in {:?} reth after {} retries. Last RPC error: {}. Final state: {}",
            &hash,
            &self.name,
            max_retries,
            last_rpc_error.unwrap_or_else(|| "none".to_string()),
            final_state
        ))
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn wait_for_reth_marker(
        &self,
        tag: BlockNumberOrTag,
        expected_hash: EvmBlockHash,
        seconds_to_wait: u64,
    ) -> eyre::Result<EvmBlockHash> {
        self.ensure_vdf_running_for_sync("wait_for_reth_marker");
        let deadline = Instant::now() + Duration::from_secs(seconds_to_wait);
        let mut attempt: u32 = 0;

        loop {
            if Instant::now() >= deadline {
                return Err(eyre::eyre!(
                    "Reth {:?} block did not reach expected hash {:?} within {}s",
                    tag,
                    expected_hash,
                    seconds_to_wait
                ));
            }

            let eth_api = self.node_ctx.reth_node_adapter.reth_node.inner.eth_api();
            match eth_api.block_by_number(tag, false).await {
                Ok(Some(block)) if block.header.hash == expected_hash => {
                    return Ok(block.header.hash);
                }
                Ok(Some(block)) => {
                    tracing::error!(
                        custom.target = "test.reth",
                        custom.tag = ?tag,
                        custom.expected = %expected_hash,
                        custom.actual = %block.header.hash,
                        custom.attempt = attempt,
                        "reth tag mismatch while waiting"
                    );
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!("error polling reth {:?} block: {:?}", tag, err);
                }
            }

            sleep(Duration::from_millis(100)).await;
            attempt = attempt.saturating_add(1);
        }
    }

    /// wait for tx to appear in the mempool or be found in the database
    #[diag_slow(state = self.diag_wait_state().await)]
    #[tracing::instrument(level = "trace", skip_all, fields(tx_id), err)]
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
            mempool_service.send_traced(MempoolServiceMessage::DataTxExists(tx_id, oneshot_tx))?;

            //if transaction exists
            if oneshot_rx
                .await
                .expect("to process ChunkIngressMessage")
                .expect("boolean response to transaction existence")
                .is_known_and_valid()
            {
                break;
            }

            sleep(Duration::from_secs(1)).await;
            retries += 1;
        }

        if retries == max_retries {
            tracing::error!("failed to locate tx");
            Err(eyre::eyre!(
                "Failed to locate tx in mempool after {} retries",
                retries
            ))
        } else {
            info!("transaction found in mempool after {} retries", &retries);
            Ok(())
        }
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn wait_for_mempool_commitment_txs(
        &self,
        tx_ids: Vec<H256>,
        seconds_to_wait: usize,
    ) -> eyre::Result<()> {
        let mempool_service = self.node_ctx.service_senders.mempool.clone();
        let max_retries = seconds_to_wait * 5; // 200ms per retry
        let mut tx_ids: HashSet<H256> = tx_ids.clone().into_iter().collect();

        for retry in 0..max_retries {
            let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
            let to_fetch = tx_ids.iter().copied().collect_vec();
            debug!("Fetching {:?}", &to_fetch);
            mempool_service.send_traced(MempoolServiceMessage::GetCommitmentTxs {
                commitment_tx_ids: to_fetch,
                response: oneshot_tx,
            })?;
            let fetched = oneshot_rx.await?;

            for found in fetched.keys() {
                debug!("Fetched tx {} from mempool in {} retries", &found, &retry);
                tx_ids.remove(found);
            }

            if tx_ids.is_empty() {
                debug!("Fetched all txs from mempool in {} retries", &retry);
                return Ok(());
            }
            sleep(Duration::from_millis(200)).await;
        }
        eyre::bail!(
            "Unable to get txs {:?} from the mempool",
            &tx_ids.iter().collect_vec()
        )
    }

    // waits until mempool
    // all filters are AND conditions (e.g., submit_txs=1, publish_txs=1 requires both).
    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn wait_for_mempool_best_txs_shape(
        &self,
        submit_txs: usize,
        publish_txs: usize,
        commitment_txs: usize,
        seconds_to_wait: u32,
    ) -> eyre::Result<(
        Vec<DataTransactionHeader>,
        PublishLedgerWithTxs,
        Vec<CommitmentTransaction>,
    )> {
        let mempool_service = self.node_ctx.service_senders.mempool.clone();
        let mut retries = 0;
        let max_retries = seconds_to_wait; // 1 second per retry
        debug!(
            "Waiting for {} submit, {} publish and {} commitment",
            &submit_txs, &publish_txs, &commitment_txs
        );
        let mut prev = (0, 0, 0);
        let expected = (submit_txs, publish_txs, commitment_txs);
        for _ in 0..max_retries {
            let canonical_tip = self.get_canonical_chain().last().unwrap().block_hash();
            let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
            mempool_service.send_traced(MempoolServiceMessage::GetBestMempoolTxs(
                canonical_tip,
                oneshot_tx,
            ))?;

            let txs: MempoolTxs = oneshot_rx.await??;
            let MempoolTxs {
                commitment_tx,
                submit_tx,
                publish_tx,
            } = txs.clone();
            prev = (submit_tx.len(), publish_tx.txs.len(), commitment_tx.len());

            if prev == expected {
                info!("mempool state valid after {} retries", &retries);
                return Ok((submit_tx, publish_tx, commitment_tx));
            }
            debug!("got {:?} expected {:?} - txs: {:?}", &prev, expected, &txs);

            tokio::time::sleep(Duration::from_secs(1)).await;
            retries += 1;
        }
        Err(eyre::eyre!(
            "Failed to validate mempool state after {} retries (state (submit, publish, commitment) {:?}, expected: {:?})",
            retries,
            &prev,
            &expected
        ))
    }

    // Get the best txs from the mempool, based off the account state at the parent Irys block
    pub async fn get_best_mempool_tx(
        &self,
        parent_block_hash: BlockHash,
    ) -> eyre::Result<MempoolTxs> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.node_ctx
            .service_senders
            .mempool
            .send_traced(MempoolServiceMessage::GetBestMempoolTxs(
                parent_block_hash,
                tx,
            ))
            .expect("to send MempoolServiceMessage");
        rx.await.expect("to receive best transactions from mempool")
    }

    // get account reth balance at specific block
    pub async fn get_balance(&self, address: IrysAddress, evm_block_hash: FixedBytes<32>) -> U256 {
        let block = Some(BlockId::Hash(RpcBlockHash {
            block_hash: evm_block_hash,
            require_canonical: Some(false),
        }));
        self.node_ctx
            .reth_node_adapter
            .rpc
            .get_balance_irys(address, block)
            .await
    }

    /// Get the price for storing data via the price API endpoint
    pub async fn get_data_price(
        &self,
        ledger: DataLedger,
        data_size: u64,
    ) -> eyre::Result<PriceInfo> {
        let client = self.get_api_client();
        client
            .get_data_price(self.get_peer_addr(), ledger, data_size)
            .await
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn post_publish_data_tx(
        &self,
        account: &IrysSigner,
        data: Vec<u8>,
    ) -> Result<DataTransaction, AddTxError> {
        // Get data size before moving data
        let data_size = data.len() as u64;

        // Query the price endpoint to get required fees for Publish ledger
        let price_info = self
            .get_data_price(DataLedger::Publish, data_size)
            .await
            .map_err(AddTxError::CreateTx)?;

        // Create transaction with proper fees using the new publish method
        let tx = account
            .create_publish_transaction(
                data,
                self.get_anchor().await.map_err(AddTxError::CreateTx)?, // anchor
                price_info.perm_fee.into(),
                price_info.term_fee.into(),
            )
            .map_err(AddTxError::CreateTx)?;

        let tx = account.sign_transaction(tx).map_err(AddTxError::CreateTx)?;

        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let response = self.node_ctx.service_senders.mempool.send_traced(
            MempoolServiceMessage::IngestDataTxFromApi(tx.header.clone(), oneshot_tx),
        );
        if let Err(e) = response {
            tracing::error!("channel closed, unable to send to mempool: {:?}", e);
        }

        match oneshot_rx.await {
            Ok(Ok(())) => Ok(tx),
            Ok(Err(tx_error)) => Err(AddTxError::TxIngress(tx_error)),
            Err(e) => Err(AddTxError::Mailbox(e)),
        }
    }

    pub async fn create_signed_data_tx(
        &self,
        account: &IrysSigner,
        data: Vec<u8>,
    ) -> Result<DataTransaction, AddTxError> {
        // Get data size before moving data
        let data_size = data.len() as u64;

        // Query the price endpoint to get required fees for Publish ledger
        let price_info = self
            .get_data_price(DataLedger::Publish, data_size)
            .await
            .map_err(AddTxError::CreateTx)?;

        // Create transaction with proper fees
        let tx = account
            .create_publish_transaction(
                data,
                self.get_anchor().await.map_err(AddTxError::CreateTx)?, // anchor
                price_info.perm_fee.into(),
                price_info.term_fee.into(),
            )
            .map_err(AddTxError::CreateTx)?;

        account.sign_transaction(tx).map_err(AddTxError::CreateTx)
    }

    /// read storage tx from mbdx i.e. block index
    pub fn get_tx_header(&self, tx_id: &H256) -> eyre::Result<DataTransactionHeader> {
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

    pub async fn get_is_promoted(&self, tx_id: &H256) -> eyre::Result<bool> {
        let mempool_sender = &self.node_ctx.service_senders.mempool;
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        if let Err(e) =
            mempool_sender.send_traced(MempoolServiceMessage::GetDataTxs(vec![*tx_id], oneshot_tx))
        {
            tracing::info!("Unable to send mempool message: {}", e);
        } else {
            match oneshot_rx.await {
                Ok(txs) => {
                    if let Some(tx_header) = &txs[0] {
                        return Ok(tx_header.promoted_height().is_some());
                    }
                }
                Err(e) => tracing::info!("receive error for mempool {}", e),
            }
        }

        match self
            .node_ctx
            .db
            .view_eyre(|tx| tx_header_by_txid(tx, tx_id))
        {
            Ok(Some(tx_header)) => {
                debug!("{:?}", tx_header);
                Ok(tx_header.promoted_height().is_some())
            }
            Ok(None) => Err(eyre::eyre!("No tx header found for txid {:?}", tx_id)),
            Err(e) => Err(eyre::eyre!("Failed to collect tx header: {}", e)),
        }
    }

    /// read storage tx from mempool
    pub async fn get_storage_tx_header_from_mempool(
        &self,
        tx_id: &H256,
    ) -> eyre::Result<DataTransactionHeader> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let tx_ingress_msg = MempoolServiceMessage::GetDataTxs(vec![*tx_id], oneshot_tx);
        if let Err(err) = self
            .node_ctx
            .service_senders
            .mempool
            .send_traced(tx_ingress_msg)
        {
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

    /// Polls the mempool until the transaction's `included_height` is set.
    /// The mempool updates `included_height` asynchronously after `BlockConfirmed`.
    pub async fn wait_for_tx_included(
        &self,
        tx_id: &H256,
        max_seconds: usize,
    ) -> eyre::Result<DataTransactionHeader> {
        self.ensure_vdf_running_for_sync("wait_for_tx_included");
        for _ in 0..(max_seconds * 10) {
            match self.get_storage_tx_header_from_mempool(tx_id).await {
                Ok(header) if header.metadata().included_height.is_some() => {
                    return Ok(header);
                }
                Ok(_) => {}  // included_height not set yet, keep polling
                Err(_) => {} // tx not yet visible in mempool, keep polling
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        Err(eyre::eyre!(
            "included_height not set for tx {} after {} seconds",
            tx_id,
            max_seconds
        ))
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
        if let Err(err) = self
            .node_ctx
            .service_senders
            .mempool
            .send_traced(tx_ingress_msg)
        {
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

    pub fn get_block_by_height_from_index(
        &self,
        height: u64,
        include_chunk: bool,
    ) -> eyre::Result<IrysBlockHeader> {
        let block = self
            .node_ctx
            .block_index_guard
            .read()
            .get_item(height)
            .ok_or_else(|| eyre::eyre!("Block at height {} not found", height))?;
        self.node_ctx
            .db
            .view_eyre(|tx| {
                irys_database::block_header_by_hash(tx, &block.block_hash, include_chunk)
            })?
            .ok_or_else(|| eyre::eyre!("Block at height {} not found", height))
    }

    pub async fn get_block_by_height(&self, height: u64) -> eyre::Result<IrysBlockHeader> {
        get_canonical_chain(self.node_ctx.block_tree_guard.clone())
            .await
            .unwrap()
            .0
            .iter()
            .find(|e| e.height() == height)
            .and_then(|e| {
                self.node_ctx
                    .block_tree_guard
                    .read()
                    .get_block(&e.block_hash())
                    .cloned()
            })
            .ok_or_else(|| eyre::eyre!("Block at height {} not found", height))
    }

    pub async fn get_blocks(
        &self,
        start_height: u64,
        end_height: u64,
    ) -> eyre::Result<Vec<IrysBlockHeader>> {
        let mut blocks = Vec::new();
        for height in start_height..=end_height {
            blocks.push(self.get_block_by_height(height).await?);
        }
        Ok(blocks)
    }

    pub fn gossip_block_to_peers(&self, block_header: &Arc<IrysBlockHeader>) -> eyre::Result<()> {
        self.node_ctx
            .service_senders
            .gossip_broadcast
            .send_traced(GossipBroadcastMessageV2::from(Arc::clone(block_header)))?;

        Ok(())
    }

    pub fn gossip_eth_block_to_peers(
        &self,
        block: &reth::primitives::SealedBlock<reth_ethereum_primitives::Block>,
    ) -> eyre::Result<()> {
        self.node_ctx
            .service_senders
            .gossip_broadcast
            .send_traced(GossipBroadcastMessageV2::from((block).clone()))?;

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

    #[diag_slow(state = "stop".to_string())]
    pub async fn stop(self) -> IrysNodeTest<()> {
        let pre_stop_state = self.diag_wait_state().await;
        info!("Stopping node with state: {}", pre_stop_state);
        // Internal timeouts in stop() now handle hung subsystems, so no outer timeout needed.
        self.node_ctx
            .stop(irys_types::ShutdownReason::TestComplete)
            .await;
        // Runtime::drop() blocks (joins worker threads), which is not allowed
        // inside an async context. Move it to a blocking thread.
        if let Some(rt) = self.runtime {
            tokio::task::spawn_blocking(move || drop(rt))
                .await
                .expect("runtime shutdown should not panic");
        }
        let cfg = self.cfg;
        IrysNodeTest {
            node_ctx: (),
            cfg,
            temp_dir: self.temp_dir,
            name: self.name,
            restart_http_ports: self.restart_http_ports,
            restart_gossip_ports: self.restart_gossip_ports,
            restart_reth_ports: self.restart_reth_ports,
            runtime: None,
        }
    }

    /// Sends only the Irys block header directly to a specific peer, bypassing gossip.
    ///
    /// Important:
    /// - This does NOT transfer:
    ///   - Any data chunks for those transactions
    ///   - The EVM execution payload for this block
    /// - In contrast, `send_full_block`:
    ///   - Optionally transfers chunks (when the peer has full ingress-proof validation enabled)
    ///   - Pushes the EVM execution payload into the peer's cache before delivering the header
    ///
    /// When to use:
    /// - Prefer `send_block_to_peer` when you only need to deliver the header and do not require
    ///   proof/chunk verification during validation (e.g., full ingress-proof validation is disabled),
    ///   or when the receiver already has all required chunks/payloads.
    /// - If `enable_full_ingress_proof_validation` is true on the receiving node and the block's
    ///   Publish ledger contains transactions with proofs, use `send_full_block` (or pre-ingest chunks)
    ///   so validation can verify proofs against actual chunk bytes.
    ///
    /// Execution payload note:
    /// - `send_full_block` requires that the sender has the EVM execution payload available locally
    ///   (otherwise it will panic). For blocks produced "without gossip," prefer:
    ///   - Using `send_block_to_peer` for the header; and, if needed,
    ///   - Pushing the EVM payload to the peer separately (e.g., gossiping the EVM block) before
    ///     attempting a full transfer.
    pub async fn send_block_to_peer(
        &self,
        peer: &Self,
        sealed_block: Arc<SealedBlock>,
    ) -> eyre::Result<()> {
        match BlockDiscoveryFacadeImpl::new(peer.node_ctx.service_senders.block_discovery.clone())
            .handle_block(Arc::clone(&sealed_block), false)
            .await
        {
            Ok(_) => Ok(()),
            Err(res) => {
                tracing::error!(
                    "Sent block to peer. Block {:?} ({}) failed pre-validation: {:?}",
                    &sealed_block.header().block_hash.0,
                    &sealed_block.header().height,
                    res
                );
                Err(eyre!(
                    "Sent block to peer. Block {:?} ({}) failed pre-validation: {:?}",
                    &sealed_block.header().block_hash.0,
                    &sealed_block.header().height,
                    res
                ))
            }
        }
    }

    /// Sends a full block to the provided peer bypassing the gossip network.
    ///
    /// This method is useful in tests where gossip is disabled. It delivers all
    /// transaction headers contained in the block as well as the block header and execution payload
    /// itself directly to the peer's actors/services.
    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn send_full_block(
        &self,
        peer: &Self,
        irys_block_header: &IrysBlockHeader,
        eth_payload: EthBuiltPayload,
        block_transactions: BlockTransactions,
    ) -> eyre::Result<()> {
        // Ingest data txs into peer's mempool
        for (ledger, txs) in block_transactions.data_txs.iter() {
            for tx_header in txs {
                let (tx, rx) = tokio::sync::oneshot::channel();
                peer.node_ctx
                    .service_senders
                    .mempool
                    .send_traced(MempoolServiceMessage::IngestDataTxFromGossip(
                        tx_header.clone(),
                        tx,
                    ))
                    .map_err(|_| eyre::eyre!("failed to send mempool message"))?;
                // Ignore possible ingestion errors in tests
                let _ = rx.await?;

                // Before sending the header, only transfer chunks if full validation is enabled
                // and this is the Publish ledger
                if *ledger == DataLedger::Publish
                    && peer
                        .node_ctx
                        .config
                        .node_config
                        .consensus_config()
                        .enable_full_ingress_proof_validation
                {
                    // Transfer chunks for this tx to the peer so block validation can verify proofs
                    let chunk_size = self
                        .node_ctx
                        .config
                        .node_config
                        .consensus_config()
                        .chunk_size;
                    let expected_chunks = tx_header.data_size.div_ceil(chunk_size);
                    for i in 0..expected_chunks {
                        // Read chunk directly from sender's DB cache by data_root + tx-relative offset
                        let tx_chunk_offset = irys_types::TxChunkOffset::from(i as u32);
                        if let Ok(Some((_meta, cached_chunk))) = self.node_ctx.db.view_eyre(|tx| {
                            irys_database::cached_chunk_by_chunk_offset(
                                tx,
                                tx_header.data_root,
                                tx_chunk_offset,
                            )
                        }) {
                            if let Some(bytes) = cached_chunk.chunk {
                                let unpacked = irys_types::UnpackedChunk {
                                    data_root: tx_header.data_root,
                                    data_size: tx_header.data_size,
                                    data_path: irys_types::Base64(cached_chunk.data_path.0.clone()),
                                    bytes,
                                    tx_offset: tx_chunk_offset,
                                };
                                let verify_data_root = unpacked.data_root;
                                let verify_tx_offset = unpacked.tx_offset;

                                let (ctx, crx) = tokio::sync::oneshot::channel();
                                peer.node_ctx
                                    .service_senders
                                    .chunk_ingress
                                    .send_traced(irys_actors::ChunkIngressMessage::IngestChunk(
                                        unpacked,
                                        Some(ctx),
                                    ))
                                    .expect("failed to send chunk to chunk_ingress");
                                crx.await
                                    .expect("chunk_ingress oneshot dropped")
                                    .expect("chunk ingress failed");

                                // Verify the chunk is present on the peer DB (small retry loop)
                                {
                                    let mut attempts = 0_usize;
                                    loop {
                                        let got = peer
                                            .node_ctx
                                            .db
                                            .view_eyre(|tx| {
                                                irys_database::cached_chunk_by_chunk_offset(
                                                    tx,
                                                    verify_data_root,
                                                    verify_tx_offset,
                                                )
                                            })
                                            .unwrap_or(None);
                                        if got.is_some() || attempts >= 5 {
                                            break;
                                        }
                                        attempts += 1;
                                        tokio::time::sleep(std::time::Duration::from_millis(50))
                                            .await;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Ingest commitment txs into peer's mempool
        for commitment_tx in block_transactions.get_ledger_system_txs(SystemLedger::Commitment) {
            tracing::error!("{}", commitment_tx.id());

            let (tx, rx) = tokio::sync::oneshot::channel();
            peer.node_ctx
                .service_senders
                .mempool
                .send_traced(MempoolServiceMessage::IngestCommitmentTxFromGossip(
                    commitment_tx.clone(),
                    tx,
                ))
                .map_err(|_| eyre::eyre!("failed to send mempool message"))?;
            if let Err(e) = rx.await {
                tracing::error!(
                    "Error sending message IngestCommitmentTxFromGossip to mempool: {e:?}"
                );
            }
        }

        // IMPORTANT: Add execution payload to cache BEFORE sending block header
        // This prevents a race condition where validation starts before the payload is available
        peer.node_ctx
            .block_pool
            .add_execution_payload_to_cache(eth_payload.block().clone())
            .await;

        let sealed_block = build_sealed_block(irys_block_header.clone(), &block_transactions)?;

        // Deliver block header (this triggers validation)
        BlockDiscoveryFacadeImpl::new(peer.node_ctx.service_senders.block_discovery.clone())
            .handle_block(sealed_block, false)
            .await
            .map_err(|e| eyre::eyre!("{e:?}"))?;

        Ok(())
    }

    pub async fn post_data_tx_without_gossip(
        &self,
        anchor: H256,
        data: Vec<u8>,
        signer: &IrysSigner,
    ) -> DataTransaction {
        self.with_gossip_disabled(self.post_data_tx(anchor, data, signer))
            .await
    }

    pub async fn post_data_tx(
        &self,
        anchor: H256,
        data: Vec<u8>,
        signer: &IrysSigner,
    ) -> DataTransaction {
        // Get data size before moving data
        let data_size = data.len() as u64;

        // Query the price endpoint to get required fees
        let price_info = self
            .get_data_price(DataLedger::Publish, data_size)
            .await
            .expect("Failed to get price");

        let tx = signer
            .create_publish_transaction(
                data,
                anchor,
                price_info.perm_fee.into(),
                price_info.term_fee.into(),
            )
            .expect("Expect to create a storage transaction from the data");
        let tx = signer
            .sign_transaction(tx)
            .expect("to sign the storage transaction");

        let client = reqwest::Client::new();
        let api_uri = self.node_ctx.config.node_config.local_api_url();
        let url = format!("{}/v1/tx", api_uri);
        let response = client
            .post(&url)
            .json(&tx.header)
            .send()
            .await
            .expect("client post failed");

        let status = response.status();
        if status != 200 {
            // Read the response body
            let body_str = response
                .text()
                .await
                .unwrap_or_else(|_| String::from("<failed to read body>"));

            panic!(
                "Response status: {} - {}\nRequest Body: {}",
                status,
                body_str,
                serde_json::to_string_pretty(&tx.header).unwrap(),
            );
        } else {
            info!(
                "Response status: {}\n{}",
                status,
                serde_json::to_string_pretty(&tx).unwrap()
            );
        }
        tx
    }

    pub async fn post_data_tx_raw(&self, tx: &DataTransactionHeader) {
        let client = reqwest::Client::new();
        let api_uri = self.node_ctx.config.node_config.local_api_url();
        let url = format!("{}/v1/tx", api_uri);
        let response = client
            .post(&url)
            .json(&tx)
            .send()
            .await
            .expect("client post failed");

        let status = response.status();
        if status != 200 {
            // Read the response body
            let body_str = response
                .text()
                .await
                .unwrap_or_else(|_| String::from("<failed to read body>"));

            panic!(
                "Response status: {} - {}\nRequest Body: {}",
                status,
                body_str,
                serde_json::to_string_pretty(&tx).unwrap(),
            );
        } else {
            info!(
                "Response status: {}\n{}",
                status,
                serde_json::to_string_pretty(&tx).unwrap()
            );
        }
    }

    pub async fn post_chunk_32b_with_status(
        &self,
        tx: &DataTransaction,
        chunk_index: usize,
        chunks: &[[u8; 32]],
    ) -> (reqwest::StatusCode, String) {
        let chunk = UnpackedChunk {
            data_root: tx.header.data_root,
            data_size: tx.header.data_size,
            data_path: Base64(tx.proofs[chunk_index].proof.clone()),
            bytes: Base64(chunks[chunk_index].to_vec()),
            tx_offset: TxChunkOffset::from(chunk_index as u32),
        };

        let client = reqwest::Client::new();
        let api_uri = self.node_ctx.config.node_config.local_api_url();
        let url = format!("{}/v1/chunk", api_uri);
        let response = client
            .post(&url)
            .json(&chunk)
            .send()
            .await
            .expect("client post failed");

        (response.status(), response.text().await.unwrap())
    }

    pub async fn post_chunk_32b(
        &self,
        tx: &DataTransaction,
        chunk_index: usize,
        chunks: &[[u8; 32]],
    ) {
        let (status, _body) = self
            .post_chunk_32b_with_status(tx, chunk_index, chunks)
            .await;

        debug!("chunk_index: {:?}", chunk_index);
        assert_eq!(status, reqwest::StatusCode::OK);
    }

    pub async fn get_chunk(
        &self,
        ledger: DataLedger,
        chunk_offset: LedgerChunkOffset,
    ) -> Option<PackedChunk> {
        let client = reqwest::Client::new();
        let api_uri = self.node_ctx.config.node_config.local_api_url();
        let url = format!(
            "{}/v1/chunk/ledger/{}/{}",
            api_uri, ledger as usize, chunk_offset
        );

        let response = client.get(&url).send().await;
        info!("{:#?}", response);

        if let Ok(resp) = response {
            // Only attempt to parse JSON if we got a successful HTTP status
            if resp.status().is_success() {
                if let Ok(packed_chunk) = resp.json::<PackedChunk>().await {
                    return Some(packed_chunk);
                }
            }
        }
        None
    }

    pub async fn verify_migrated_chunk_32b(
        &self,
        ledger: DataLedger,
        chunk_offset: LedgerChunkOffset,
        expected_bytes: &[u8; 32],
        expected_data_size: u64,
    ) {
        if let Some(packed_chunk) = self.get_chunk(ledger, chunk_offset).await {
            let unpacked_chunk = unpack(
                &packed_chunk,
                self.node_ctx.config.consensus.entropy_packing_iterations,
                self.node_ctx.config.consensus.chunk_size as usize,
                self.node_ctx.config.consensus.chain_id,
            );
            if unpacked_chunk.bytes.0 != expected_bytes {
                println!(
                    "ledger_chunk_offset: {}\nfound: {:?}\nexpected: {:?}",
                    chunk_offset, unpacked_chunk.bytes.0, expected_bytes
                )
            }
            assert_eq!(unpacked_chunk.bytes.0, expected_bytes);

            assert_eq!(unpacked_chunk.data_size, expected_data_size);
        } else {
            panic!(
                "Chunk not found! {} ledger chunk_offset: {}",
                ledger, chunk_offset
            );
        }
    }

    pub async fn verify_chunk_not_present(
        &self,
        ledger: DataLedger,
        chunk_offset: LedgerChunkOffset,
    ) {
        assert!(
            self.get_chunk(ledger, chunk_offset).await.is_none(),
            "Expected no chunk at {} ledger chunk_offset: {}, but found one",
            ledger,
            chunk_offset
        );
    }

    pub fn get_api_client(&self) -> IrysApiClient {
        IrysApiClient::new()
    }

    pub fn get_gossip_client(&self) -> GossipClient {
        GossipClient::new(
            Duration::from_secs(5),
            self.node_ctx.config.node_config.miner_address(),
            self.node_ctx.config.peer_id(),
        )
    }

    pub fn get_peer_addr(&self) -> SocketAddr {
        self.node_ctx.config.node_config.peer_address().api
    }

    pub fn get_gossip_addr(&self) -> SocketAddr {
        self.node_ctx.config.node_config.peer_address().gossip
    }

    // Build a signed HandshakeRequest describing this node (V1 version for compatibility)
    pub fn build_handshake_request(&self) -> HandshakeRequest {
        let mut handshake = HandshakeRequest {
            chain_id: self.node_ctx.config.consensus.chain_id,
            address: self.node_ctx.config.node_config.peer_address(),
            mining_address: self.node_ctx.config.node_config.reward_address,
            ..HandshakeRequest::default()
        };
        self.node_ctx
            .config
            .irys_signer()
            .sign_p2p_handshake_v1(&mut handshake)
            .expect("sign p2p handshake");
        handshake
    }

    // Build a signed HandshakeRequestV2 describing this node
    pub fn build_handshake_request_v2(&self) -> HandshakeRequestV2 {
        let mut handshake = HandshakeRequestV2 {
            chain_id: self.node_ctx.config.consensus.chain_id,
            address: self.node_ctx.config.node_config.peer_address(),
            mining_address: self.node_ctx.config.node_config.reward_address,
            peer_id: self.node_ctx.config.peer_id(),
            consensus_config_hash: self.node_ctx.config.consensus.keccak256_hash(),
            ..HandshakeRequestV2::default()
        };
        self.node_ctx
            .config
            .irys_signer()
            .sign_p2p_handshake_v2(&mut handshake)
            .expect("sign p2p handshake v2");
        handshake
    }

    // Announce this node to another node via gossip handshake
    pub async fn announce_to(&self, dst: &Self) -> eyre::Result<()> {
        let protocol_version = ProtocolVersion::current();
        match protocol_version {
            ProtocolVersion::V1 => {
                let vr = self.build_handshake_request();
                self.get_gossip_client()
                    .post_handshake_v1(dst.get_gossip_addr(), vr)
                    .await?;
            }
            ProtocolVersion::V2 => {
                let vr = self.build_handshake_request_v2();
                self.get_gossip_client()
                    .post_handshake_v2(dst.get_gossip_addr(), vr)
                    .await?;
            }
        }
        Ok(())
    }

    // Announce both ways between two nodes
    pub async fn announce_between(a: &Self, b: &Self) -> eyre::Result<()> {
        a.announce_to(b).await?;
        b.announce_to(a).await?;
        Ok(())
    }

    // Announce full mesh among a list of nodes
    pub async fn announce_mesh(nodes: &[&Self]) -> eyre::Result<()> {
        for i in 0..nodes.len() {
            for j in (i + 1)..nodes.len() {
                Self::announce_between(nodes[i], nodes[j]).await?;
            }
        }
        Ok(())
    }

    // Wait until this node's peer list includes the target peer address
    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn wait_until_sees_peer(
        &self,
        target: &PeerAddress,
        max_attempts: usize,
    ) -> eyre::Result<()> {
        for _ in 0..max_attempts {
            tokio::time::sleep(Duration::from_millis(100)).await;

            let client = reqwest::Client::new();
            let api_uri = self.node_ctx.config.node_config.local_api_url();
            let url = format!("{}/v1/peer-list", api_uri);

            if let Ok(resp) = client.get(&url).send().await {
                if let Ok(list) = resp.json::<Vec<PeerAddress>>().await {
                    if list.contains(target) {
                        return Ok(());
                    }
                }
            }
        }
        eyre::bail!(
            "peer {:?} not visible after {} attempts",
            target,
            max_attempts
        )
    }

    pub async fn upload_chunks(&self, tx: &DataTransaction) -> eyre::Result<()> {
        let client = self.get_api_client();
        client.upload_chunks(self.get_peer_addr(), tx).await
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn post_commitment_tx(
        &self,
        commitment_tx: &CommitmentTransaction,
    ) -> eyre::Result<()> {
        let api_uri = self.node_ctx.config.node_config.local_api_url();
        self.post_commitment_tx_request(&api_uri, commitment_tx)
            .await
    }

    /// Helper to post an UpdateRewardAddress commitment transaction.
    pub async fn post_update_reward_address(
        &self,
        signer: &IrysSigner,
        new_reward_address: IrysAddress,
    ) -> eyre::Result<CommitmentTransaction> {
        self.post_update_reward_address_with_fee(
            signer,
            new_reward_address,
            self.node_ctx.config.consensus.mempool.commitment_fee,
        )
        .await
    }

    /// Helper to post an UpdateRewardAddress commitment transaction with a custom fee.
    pub async fn post_update_reward_address_with_fee(
        &self,
        signer: &IrysSigner,
        new_reward_address: IrysAddress,
        fee: u64,
    ) -> eyre::Result<CommitmentTransaction> {
        let consensus = &self.node_ctx.config.consensus;
        let anchor = self.get_anchor().await.expect("anchor should be available");

        let mut update_tx = CommitmentTransaction::V2(CommitmentV2WithMetadata {
            tx: CommitmentTransactionV2 {
                commitment_type: CommitmentTypeV2::UpdateRewardAddress { new_reward_address },
                anchor,
                fee,
                value: U256::zero(),
                ..CommitmentTransactionV2::new(consensus)
            },
            metadata: Default::default(),
        });

        signer.sign_commitment(&mut update_tx)?;
        tracing::info!("Generated update_reward_address_tx.id: {}", update_tx.id());

        self.post_commitment_tx(&update_tx).await?;

        Ok(update_tx)
    }

    pub async fn get_stake_price(&self) -> eyre::Result<CommitmentPriceInfo> {
        let client = reqwest::Client::new();
        let api_uri = self.node_ctx.config.node_config.local_api_url();
        let url = format!("{}/v1/price/commitment/stake", api_uri);

        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Failed to get stake price: {}", e))?;

        let price_info: CommitmentPriceInfo = response
            .json()
            .await
            .map_err(|e| eyre::eyre!("Failed to parse stake price response: {}", e))?;

        Ok(price_info)
    }

    pub async fn get_pledge_price(
        &self,
        user_address: IrysAddress,
    ) -> eyre::Result<CommitmentPriceInfo> {
        let client = reqwest::Client::new();
        let api_uri = self.node_ctx.config.node_config.local_api_url();
        let url = format!("{}/v1/price/commitment/pledge/{}", api_uri, user_address);

        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Failed to get pledge price: {}", e))?;

        let price_info: CommitmentPriceInfo = response
            .json()
            .await
            .map_err(|e| eyre::eyre!("Failed to parse pledge price response: {}", e))?;

        Ok(price_info)
    }

    pub async fn ingest_ingress_proof(&self, ingress_proof: IngressProof) -> eyre::Result<()> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        self.node_ctx.service_senders.chunk_ingress.send_traced(
            irys_actors::ChunkIngressMessage::IngestIngressProof(ingress_proof, oneshot_tx),
        )?;

        Ok(oneshot_rx.await??)
    }

    pub async fn post_commitment_tx_raw_without_gossip(
        &self,
        commitment_tx: &CommitmentTransaction,
    ) -> eyre::Result<()> {
        let api_uri = self.node_ctx.config.node_config.local_api_url();
        self.with_gossip_disabled(self.post_commitment_tx_request(&api_uri, commitment_tx))
            .await
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn ingest_data_tx(&self, data_tx: DataTransactionHeader) -> Result<(), AddTxError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let result = self.node_ctx.service_senders.mempool.send_traced(
            MempoolServiceMessage::IngestDataTxFromApi(data_tx, oneshot_tx),
        );
        if let Err(e) = result {
            tracing::error!("channel closed, unable to send to mempool: {:?}", e);
        }

        match oneshot_rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(tx_error)) => Err(AddTxError::TxIngress(tx_error)),
            Err(e) => Err(AddTxError::Mailbox(e)),
        }
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn ingest_commitment_tx(
        &self,
        commitment_tx: CommitmentTransaction,
    ) -> Result<(), AddTxError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let result = self.node_ctx.service_senders.mempool.send_traced(
            MempoolServiceMessage::IngestCommitmentTxFromApi(commitment_tx, oneshot_tx),
        );
        if let Err(e) = result {
            tracing::error!("channel closed, unable to send to mempool: {:?}", e);
        }

        match oneshot_rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(tx_error)) => Err(AddTxError::TxIngress(tx_error)),
            Err(e) => Err(AddTxError::Mailbox(e)),
        }
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn post_pledge_commitment(
        &self,
        anchor: Option<H256>,
    ) -> eyre::Result<CommitmentTransaction> {
        let config = &self.node_ctx.config.consensus;
        let signer = self.cfg.signer();
        let anchor = match anchor {
            Some(anchor) => anchor,
            None => self.get_anchor().await?,
        };
        let mut pledge_tx = CommitmentTransaction::new_pledge(
            config,
            anchor,
            self.node_ctx.mempool_pledge_provider.as_ref(),
            signer.address(),
        )
        .await;

        signer.sign_commitment(&mut pledge_tx).unwrap();
        info!("Generated pledge_tx.id: {}", pledge_tx.id());

        // Submit pledge commitment via API
        let api_uri = self.node_ctx.config.node_config.local_api_url();
        self.post_commitment_tx_request(&api_uri, &pledge_tx)
            .await?;

        Ok(pledge_tx)
    }

    pub async fn post_pledge_commitment_with_signer(
        &self,
        signer: &IrysSigner,
    ) -> CommitmentTransaction {
        let consensus = &self.node_ctx.config.consensus;
        let anchor = self
            .get_anchor()
            .await
            .expect("failed to get anchor for pledge commitment");

        let mut pledge_tx = CommitmentTransaction::new_pledge(
            consensus,
            anchor,
            self.node_ctx.mempool_pledge_provider.as_ref(),
            signer.address(),
        )
        .await;
        signer.sign_commitment(&mut pledge_tx).unwrap();
        info!("Generated pledge_tx.id: {}", pledge_tx.id());

        // Submit pledge commitment via API
        self.post_commitment_tx(&pledge_tx)
            .await
            .expect("posted commitment tx");

        pledge_tx
    }

    pub async fn post_pledge_commitment_without_gossip(
        &self,
        anchor: Option<H256>,
    ) -> eyre::Result<CommitmentTransaction> {
        self.with_gossip_disabled(self.post_pledge_commitment(anchor))
            .await
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn get_anchor(&self) -> eyre::Result<H256> {
        self.get_api_client().get_anchor(self.get_peer_addr()).await
    }

    #[diag_slow(state = self.diag_wait_state().await)]
    pub async fn post_stake_commitment(
        &self,
        anchor: Option<H256>,
    ) -> eyre::Result<CommitmentTransaction> {
        let config = &self.node_ctx.config.consensus;
        let anchor = match anchor {
            Some(anchor) => anchor,
            None => self.get_anchor().await?,
        };
        let mut stake_tx = CommitmentTransaction::new_stake(config, anchor);
        let signer = self.cfg.signer();
        signer.sign_commitment(&mut stake_tx).unwrap();
        info!("Generated stake_tx.id: {}", stake_tx.id());

        // Submit stake commitment via public API
        let api_uri = self.node_ctx.config.node_config.local_api_url();
        self.post_commitment_tx_request(&api_uri, &stake_tx)
            .await
            .expect("posted commitment tx");

        Ok(stake_tx)
    }

    pub async fn post_stake_commitment_with_signer(
        &self,
        signer: &IrysSigner,
    ) -> eyre::Result<CommitmentTransaction> {
        let config = &self.node_ctx.config.consensus;
        let anchor = self.get_anchor().await?;
        let mut stake_tx = CommitmentTransaction::new_stake(config, anchor);
        signer.sign_commitment(&mut stake_tx).unwrap();
        info!("Generated stake_tx.id: {}", stake_tx.id());

        // Submit stake commitment via public API
        let api_uri = self.node_ctx.config.node_config.local_api_url();
        self.post_commitment_tx_request(&api_uri, &stake_tx)
            .await
            .expect("posted commitment tx");

        Ok(stake_tx)
    }

    pub async fn post_stake_commitment_without_gossip(
        &self,
        anchor: Option<H256>,
    ) -> eyre::Result<CommitmentTransaction> {
        self.with_gossip_disabled(self.post_stake_commitment(anchor))
            .await
    }

    pub fn get_partition_assignments(
        &self,
        miner_address: IrysAddress,
    ) -> Vec<PartitionAssignment> {
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
    ) -> eyre::Result<()> {
        info!("Posting Commitment TX: {}", commitment_tx.id());

        let client = reqwest::Client::new();
        let url = format!("{}/v1/commitment-tx", api_uri);
        let result = client.post(&url).json(commitment_tx).send().await;

        let response = match result {
            Ok(r) => r,
            Err(e) => {
                error!("Failed to post commitment transaction: {e}");
                return Err(eyre::eyre!(
                    "Failed to post commitment transaction {}: {e}",
                    &commitment_tx.id()
                ));
            }
        };

        let status = response.status();
        if status != 200 {
            // Read the response body for logging
            let body_str = match response.text().await {
                Ok(text) => text,
                Err(e) => {
                    error!("Failed to read error response body: {e}");
                    String::new()
                }
            };

            error!(
                "Response status: {} - {}\nRequest Body: {}",
                status,
                body_str,
                serde_json::to_string_pretty(&commitment_tx).unwrap(),
            );
            Err(eyre::eyre!(
                "Posted commitment transaction {} but got HTTP response code: {:?}",
                &commitment_tx.id(),
                status
            ))
        } else {
            info!(
                "Response status: {}\n{}",
                status,
                serde_json::to_string_pretty(&commitment_tx).unwrap()
            );
            Ok(())
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
        self.ensure_vdf_running_for_sync("gossip_enable");
        self.node_ctx.sync_state.set_gossip_reception_enabled(true);
        self.node_ctx.sync_state.set_gossip_broadcast_enabled(true);
    }

    // disable node ability to gossip until enabled
    pub fn gossip_disable(&self) {
        self.node_ctx.sync_state.set_gossip_reception_enabled(false);
        self.node_ctx.sync_state.set_gossip_broadcast_enabled(false);
    }

    /// Execute the provided future with gossip temporarily disabled.
    async fn with_gossip_disabled<F>(&self, fut: F) -> F::Output
    where
        F: std::future::Future,
    {
        // save state so we can set back to it
        let was_broadcast_enabled = self.node_ctx.sync_state.is_gossip_broadcast_enabled();
        let was_reception_enabled = self.node_ctx.sync_state.is_gossip_reception_enabled();

        self.gossip_disable();
        let res = fut.await;

        // return to original state
        self.node_ctx
            .sync_state
            .set_gossip_broadcast_enabled(was_broadcast_enabled);
        self.node_ctx
            .sync_state
            .set_gossip_reception_enabled(was_reception_enabled);

        res
    }

    /// Get the full canonical chain as BlockTreeEntry items
    pub fn get_canonical_chain(&self) -> Vec<BlockTreeEntry> {
        self.node_ctx
            .block_tree_guard
            .read()
            .get_canonical_chain()
            .0
    }

    /// Get the EMA snapshot for a given block hash
    pub fn get_ema_snapshot(&self, block_hash: &H256) -> Option<Arc<EmaSnapshot>> {
        self.node_ctx
            .block_tree_guard
            .read()
            .get_ema_snapshot(block_hash)
    }

    pub fn get_canonical_epoch_snapshot(&self) -> Arc<EpochSnapshot> {
        self.node_ctx
            .block_tree_guard
            .read()
            .canonical_epoch_snapshot()
    }

    /// Mine blocks until a condition is met
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn mine_until_condition<F>(
        &self,
        mut condition: F,
        blocks_per_batch: usize,
        max_blocks: usize,
        max_seconds: usize,
    ) -> eyre::Result<usize>
    where
        F: FnMut(&[IrysBlockHeader]) -> bool,
    {
        let mut total_blocks_mined = 0;

        while total_blocks_mined < max_blocks {
            // Mine a batch of blocks
            self.mine_blocks(blocks_per_batch).await?;
            total_blocks_mined += blocks_per_batch;

            // Wait for blocks to be indexed
            self.wait_until_height(total_blocks_mined as u64, max_seconds)
                .await?;

            // Get all blocks mined so far
            let blocks = self.get_blocks(0, total_blocks_mined as u64).await?;

            // Check if condition is met
            if condition(&blocks) {
                return Ok(total_blocks_mined);
            }
        }

        Err(eyre::eyre!(
            "Condition not met after mining {} blocks",
            total_blocks_mined
        ))
    }

    /// Get all blocks that contain VDF resets
    pub async fn get_blocks_with_vdf_resets(
        &self,
        start_height: u64,
        end_height: u64,
    ) -> eyre::Result<Vec<IrysBlockHeader>> {
        let blocks = self.get_blocks(start_height, end_height).await?;
        let reset_frequency = self.node_ctx.config.vdf.reset_frequency;

        Ok(blocks
            .into_iter()
            .filter(|block| {
                block
                    .vdf_limiter_info
                    .reset_step(reset_frequency as u64)
                    .is_some()
            })
            .collect())
    }

    /// Verify that blocks match between two nodes
    pub async fn verify_blocks_match(
        &self,
        other: &Self,
        start_height: u64,
        end_height: u64,
    ) -> eyre::Result<()> {
        let self_blocks = self.get_blocks(start_height, end_height).await?;
        let other_blocks = other.get_blocks(start_height, end_height).await?;

        for (index, (self_block, other_block)) in
            self_blocks.iter().zip_eq(other_blocks.iter()).enumerate()
        {
            // Compare full headers for completeness and clarity
            eyre::ensure!(
                self_block == other_block,
                "Block mismatch at index {} (height {}): block hashes {:?} vs {:?}",
                index,
                self_block.height,
                self_block.block_hash,
                other_block.block_hash
            );
        }

        Ok(())
    }

    pub fn chunk_bytes_gen(
        count: u64,
        chunk_size: usize,
        seed: u64,
    ) -> impl Iterator<Item = eyre::Result<ChunkBytes>> {
        chunk_bytes_gen(count, chunk_size, seed)
    }

    pub async fn gossip_commitment_to_node(
        &self,
        commitment: &CommitmentTransaction,
    ) -> eyre::Result<()> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.node_ctx.service_senders.mempool.send_traced(
            MempoolServiceMessage::IngestCommitmentTxFromGossip(commitment.clone(), resp_tx),
        )?;

        resp_rx.await??;
        Ok(())
    }

    pub async fn gossip_data_tx_to_node(&self, tx: &DataTransactionHeader) -> eyre::Result<()> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.node_ctx.service_senders.mempool.send_traced(
            MempoolServiceMessage::IngestDataTxFromGossip(tx.clone(), resp_tx),
        )?;

        resp_rx.await??;
        Ok(())
    }
}

/// Construct a SolutionContext using a provided PoA chunk for the current step.
/// Computes solution_hash = sha256(poa_chunk || offset_le || vdf_output) and returns immediately
/// with a consistent cryptographic link, without attempting to satisfy difficulty or iterate offsets.
/// This avoids timeouts and nondeterminism when running the full test suite.
///
/// Note: This is only used in tests that disable validation when producing the "evil" block.
/// The block will later be rejected when validation is re-enabled due to PoA verification.
pub async fn solution_context_with_poa_chunk(
    node_ctx: &IrysNodeCtx,
    poa_chunk: Vec<u8>,
) -> Result<SolutionContext, eyre::Error> {
    let was_vdf_enabled = node_ctx
        .is_vdf_mining_enabled
        .load(std::sync::atomic::Ordering::Relaxed);
    if !was_vdf_enabled {
        node_ctx.start_vdf();
    }

    let result = async {
        // Ensure the VDF has at least two steps materialized (N-1, N)
        let vdf_steps_guard = node_ctx.vdf_steps_guard.clone();
        let start = std::time::Instant::now();
        let max_wait = std::time::Duration::from_secs(5);
        let (step, steps) = loop {
            if start.elapsed() > max_wait {
                return Err(eyre::eyre!(
                    "VDF steps unavailable: timed out waiting for (prev,current) pair"
                ));
            }
            let s = vdf_steps_guard.read().global_step;
            if s >= 1 {
                if let Ok(steps) = vdf_steps_guard.read().get_steps(ii(s - 1, s)) {
                    if steps.len() >= 2 {
                        break (s, steps);
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };

        // Compute checkpoints for (step-1)
        let mut hasher = Sha256::new();
        let mut salt =
            irys_types::U256::from(step_number_to_salt_number(&node_ctx.config.vdf, step - 1));
        let mut seed = steps[0];
        let mut checkpoints: Vec<H256> =
            vec![H256::default(); node_ctx.config.vdf.num_checkpoints_in_vdf_step];
        vdf_sha(
            &mut hasher,
            &mut salt,
            &mut seed,
            node_ctx.config.vdf.num_checkpoints_in_vdf_step,
            node_ctx.config.vdf.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );

        // For deterministic linkage without recall-range dependency, use offset 0
        let partition_hash = H256::zero();
        let partition_chunk_offset: u32 = 0;

        // Compute solution_hash = sha256(poa_chunk || offset_le || vdf_output)
        let mut hasher_sol = Sha256::new();
        hasher_sol.update(&poa_chunk);
        hasher_sol.update(partition_chunk_offset.to_le_bytes());
        hasher_sol.update(steps[1].as_bytes());
        let solution_hash = H256::from_slice(hasher_sol.finalize().as_slice());

        Ok(SolutionContext {
            partition_hash,
            chunk_offset: partition_chunk_offset,
            mining_address: node_ctx.config.node_config.miner_address(),
            tx_path: None,
            data_path: None,
            chunk: poa_chunk,
            vdf_step: step,
            checkpoints: H256List(checkpoints),
            seed: Seed(steps[1]),
            solution_hash,
        })
    }
    .await;

    if !was_vdf_enabled {
        node_ctx.stop_vdf();
    }

    result
}

pub async fn solution_context(node_ctx: &IrysNodeCtx) -> Result<SolutionContext, eyre::Error> {
    // Fetch previous (parent) block difficulty
    // Get parent block directly from in-memory block tree
    let prev_block = {
        let read = node_ctx.block_tree_guard.read();
        let parent_hash = read.get_max_cumulative_difficulty_block().1;
        read.get_block(&parent_hash)
            .cloned()
            .ok_or_else(|| eyre!("Parent block header not found in block tree"))?
    };

    let vdf_steps_guard = node_ctx.vdf_steps_guard.clone();
    let was_vdf_enabled = node_ctx
        .is_vdf_mining_enabled
        .load(std::sync::atomic::Ordering::Relaxed);
    if !was_vdf_enabled {
        node_ctx.start_vdf();
    }
    let poa_solution = capacity_chunk_solution(
        node_ctx.config.node_config.miner_address(),
        vdf_steps_guard.clone(),
        &node_ctx.config,
        prev_block.diff,
    )
    .await;
    if !was_vdf_enabled {
        node_ctx.stop_vdf();
    }
    Ok(poa_solution)
}

/// Outcome of block validation for testing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockValidationOutcome {
    /// Block was validated and stored with the given chain state.
    StoredOnNode(ChainState),
    /// Block was discarded with validation error details.
    Discarded(irys_actors::block_validation::ValidationError),
}

pub fn assert_validation_error(
    outcome: BlockValidationOutcome,
    error_matcher: impl Fn(&block_validation::ValidationError) -> bool,
    context: &str,
) {
    match outcome {
        BlockValidationOutcome::Discarded(ref err) if error_matcher(err) => {}
        other => panic!("{} - expected validation error, got: {:?}", context, other),
    }
}

#[diag_slow(state = format!("block_hash={}", block_hash))]
pub async fn read_block_from_state(
    node_ctx: &IrysNodeCtx,
    block_hash: &H256,
    mut event_receiver: tokio::sync::broadcast::Receiver<BlockStateUpdated>,
) -> BlockValidationOutcome {
    let mut was_validation_scheduled = false;

    // Poll for up to 50 seconds (500 iterations * 100ms)
    for _ in 0..500 {
        // Check for block state events (non-blocking)
        while let Ok(event) = event_receiver.try_recv() {
            if event.block_hash == *block_hash && event.discarded {
                // Block was discarded, extract validation error from result
                if let irys_actors::block_tree_service::ValidationResult::Invalid(error) =
                    event.validation_result
                {
                    return BlockValidationOutcome::Discarded(error);
                }
            }
        }

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
                return BlockValidationOutcome::Discarded(
                    irys_actors::block_validation::ValidationError::Other(
                        "Block was discarded without validation error event".to_string(),
                    ),
                );
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
    BlockValidationOutcome::Discarded(irys_actors::block_validation::ValidationError::Other(
        "Timeout waiting for block validation".to_string(),
    ))
}

/// Wait for a [`BlockStateUpdated`] event matching `predicate`, with a timeout.
///
/// Consumes events from the broadcast receiver until one matches or the deadline
/// is reached.  Reuses the *same* receiver across calls so callers that iterate
/// over multiple blocks can share one subscription.
#[diag_slow(state = format!("timeout_secs={}", timeout_secs))]
pub async fn wait_for_block_event(
    rx: &mut tokio::sync::broadcast::Receiver<BlockStateUpdated>,
    timeout_secs: u64,
    predicate: impl Fn(&BlockStateUpdated) -> bool,
) -> eyre::Result<BlockStateUpdated> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(event)) if predicate(&event) => return Ok(event),
            Ok(Ok(_)) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                tracing::warn!("block_event receiver lagged; skipped {skipped} events, continuing");
                continue;
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                return Err(eyre!(
                    "broadcast channel closed while waiting for block event"
                ));
            }
            Err(_) => {
                return Err(eyre!(
                    "timed out after {timeout_secs}s waiting for block event"
                ));
            }
        }
    }
}

/// Helper function for testing chunk uploads. Posts a single chunk of transaction data
/// to the /v1/chunk endpoint and verifies successful response.
pub async fn post_chunk<T, B>(
    app: &T,
    tx: &DataTransaction,
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
pub async fn post_data_tx<T, B>(app: &T, tx: &DataTransaction)
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

#[deprecated]
pub async fn post_commitment_tx<T, B>(app: &T, tx: &CommitmentTransaction)
where
    T: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: MessageBody + Unpin,
{
    let req = TestRequest::post()
        .uri("/v1/commitment-tx")
        .set_json(tx.clone())
        .to_request();

    let resp = call_service(&app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    debug!("Response body: {:#?}", body);
    assert_eq!(status, StatusCode::OK);
}

pub fn new_stake_tx(
    anchor: &H256,
    signer: &IrysSigner,
    config: &ConsensusConfig,
) -> CommitmentTransaction {
    let mut stake_tx = CommitmentTransaction::new_stake(config, *anchor);
    signer.sign_commitment(&mut stake_tx).unwrap();
    stake_tx
}

pub async fn new_pledge_tx<P: irys_types::transaction::PledgeDataProvider>(
    anchor: &H256,
    signer: &IrysSigner,
    config: &ConsensusConfig,
    pledge_provider: &P,
) -> CommitmentTransaction {
    let mut pledge_tx =
        CommitmentTransaction::new_pledge(config, *anchor, pledge_provider, signer.address()).await;
    signer.sign_commitment(&mut pledge_tx).unwrap();
    pledge_tx
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

/// Finds and returns the block header containing a given transaction ID.
/// Takes a transaction ID, ledger type, and database connection.
/// Returns None if the transaction isn't found in any block.
pub fn get_block_containing_tx(
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

/// Polls `get_block_containing_tx` until the block header appears in `IrysBlockHeaders`.
/// The table is populated asynchronously after block migration, so a direct read
/// may return `None` even though the block has already been mined.
#[diag_slow(state = format!("txid={} ledger={:?}", txid, ledger))]
pub async fn wait_for_block_containing_tx(
    txid: H256,
    ledger: DataLedger,
    db: &DatabaseProvider,
    max_seconds: usize,
) -> eyre::Result<IrysBlockHeader> {
    for attempt in 1..=max_seconds {
        if let Some(block) = get_block_containing_tx(txid, ledger, db) {
            tracing::info!(
                "found block containing tx {} after {} attempt(s)",
                txid,
                attempt
            );
            return Ok(block);
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    Err(eyre::eyre!(
        "block containing tx {} not found in IrysBlockHeaders after {} seconds",
        txid,
        max_seconds
    ))
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

pub async fn gossip_commitment_to_node(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    commitment: &CommitmentTransaction,
) -> eyre::Result<()> {
    let (resp_tx, resp_rx) = oneshot::channel();
    node.node_ctx.service_senders.mempool.send_traced(
        MempoolServiceMessage::IngestCommitmentTxFromGossip(commitment.clone(), resp_tx),
    )?;

    resp_rx.await??;
    Ok(())
}

pub async fn gossip_data_tx_to_node(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    tx: &DataTransactionHeader,
) -> eyre::Result<()> {
    let (resp_tx, resp_rx) = oneshot::channel();
    node.node_ctx.service_senders.mempool.send_traced(
        MempoolServiceMessage::IngestDataTxFromGossip(tx.clone(), resp_tx),
    )?;

    resp_rx.await??;
    Ok(())
}

/// Helper function to construct a SealedBlock from a header and transactions.
/// This centralizes the BlockBody construction and SealedBlock::new validation
/// for consistent usage across tests.
pub fn build_sealed_block(
    header: IrysBlockHeader,
    txs: &BlockTransactions,
) -> eyre::Result<Arc<SealedBlock>> {
    let block_body = BlockBody {
        block_hash: header.block_hash,
        data_transactions: txs.all_data_txs().cloned().collect(),
        commitment_transactions: txs.all_system_txs().cloned().collect(),
    };
    Ok(Arc::new(SealedBlock::new(header, block_body)?))
}
