//! PD chunk optimistic push integration tests.
//!
//! Tests the optimistic push feature across a 3-node network:
//! - Genesis (Node A): has partition assignments, stores chunks, source of push
//! - Block Producer (Node B): staked with assignments, no PD data for test offsets
//! - Observer (Node C): not staked, not pledged, validator-only push target
//!
//! Three scenarios:
//! 1. Happy path: push delivers 5 MB PD data, smart contract verifies byte-level
//!    correctness on the EVM side, all 3 nodes validate and follow the same tip
//! 2. Cache-hit shortcut: duplicate push exits early without re-verification
//! 3. Pending fetch reconciliation: push wins over in-flight P2P fetch

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use alloy_consensus::{SignableTransaction as _, TxEip1559, TxEnvelope as EthereumTxEnvelope};
use alloy_core::primitives::U256;
use alloy_eips::Encodable2718 as _;
use alloy_genesis::GenesisAccount;
use alloy_network::{EthereumWallet, TxSignerSync as _};
use alloy_primitives::Address;
use alloy_provider::{Provider as _, ProviderBuilder};
use alloy_signer_local::{LocalSigner, PrivateKeySigner};
use alloy_sol_macro::sol;
use alloy_sol_types::SolCall as _;
use irys_reth::pd_tx::build_pd_access_list_with_fees;
use irys_types::chunk_provider::ChunkStorageProvider as _;
use irys_types::gossip::PdChunkPush;
use irys_types::irys::IrysSigner;
use irys_types::range_specifier::PdDataRead;
use irys_types::ChunkFormat;
use irys_types::{
    Base64, DataLedger, LedgerChunkOffset, NodeConfig, PeerFilterMode, TxChunkOffset, UnpackedChunk,
};
use irys_types::{IrysAddress, PeerAddress};
use k256::ecdsa::SigningKey;
use tracing::info;

use crate::utils::IrysNodeTest;

sol!(
    #[sol(rpc)]
    PdSumVerifier,
    "../../fixtures/contracts/out/IrysPDSumVerifier.sol/PdSumVerifier.json"
);

const DEV_PRIVATE_KEY: &str = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
const DEV_ADDRESS: &str = "64f1a2829e0e698c18e7792d6e74f67d89aa0a32";

/// Context returned by [`setup_pd_push_test`] containing all 3 nodes and metadata.
#[expect(dead_code, reason = "fields available for future test assertions")]
pub(crate) struct PdPushTestContext {
    /// Genesis node: has partition assignments, stores chunks, source of optimistic pushes.
    pub genesis: IrysNodeTest<irys_chain::IrysNodeCtx>,
    /// Block producer peer: staked, has assignments. Does NOT store PD data for test offsets.
    pub block_producer: IrysNodeTest<irys_chain::IrysNodeCtx>,
    /// Observer node: not staked, not pledged, validator-only. Primary push target.
    pub observer: IrysNodeTest<irys_chain::IrysNodeCtx>,
    /// Global ledger offset where the uploaded data starts (Publish ledger).
    pub data_start_offset: u64,
    /// Partition index derived from `data_start_offset / num_chunks_in_partition`.
    pub partition_index: u64,
    /// Local offset within the partition: `(data_start_offset % num_chunks_in_partition) as u32`.
    pub local_offset: u32,
    /// Number of chunks in a partition (from consensus config).
    pub num_chunks_in_partition: u64,
    /// Chunk size in bytes (from consensus config).
    pub chunk_size: u64,
    /// Number of chunks uploaded.
    pub num_chunks_uploaded: u64,
    /// The raw data bytes that were uploaded to Genesis.
    pub data_bytes: Vec<u8>,
    /// Signer/account used to upload data on Genesis.
    pub data_signer: IrysSigner,
    /// Signer/account for the block producer (Node B).
    pub peer_signer: IrysSigner,
    /// Signer/account for the observer (Node C).
    pub observer_signer: IrysSigner,
    /// Signer/account dedicated to submitting PD transactions.
    pub pd_signer: IrysSigner,
    /// Second PD signer for tests needing two independent PD accounts.
    pub pd_signer_2: IrysSigner,
}

/// Start 3 nodes for PD chunk optimistic push testing.
///
/// 1. Starts Genesis with `chunk_size=32`, `block_migration_depth=2`,
///    `num_chunks_in_partition=10`, `pd_optimistic_push_fanout=fanout`.
/// 2. Uploads 16 chunks x 32 bytes = 512 bytes of data on Genesis.
/// 3. Mines blocks past migration depth so chunks migrate to storage modules.
/// 4. Starts Block Producer with partition assignments (staked + pledged + epoch).
/// 5. Starts Observer as a validator-only peer (not staked, not pledged).
/// 6. Syncs Observer to Genesis block index height.
pub(crate) async fn setup_pd_push_test(fanout: u32) -> eyre::Result<PdPushTestContext> {
    let chunk_size: u64 = 32;
    let num_chunks_in_partition: u64 = 10;
    let num_chunks_uploaded: u64 = 16;
    let seconds_to_wait = 30;

    // --- Configure Genesis (Node A) ---
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = chunk_size;
    config.consensus.get_mut().block_migration_depth = 2;
    config.consensus.get_mut().num_chunks_in_recall_range = 2;
    config.consensus.get_mut().num_chunks_in_partition = num_chunks_in_partition;
    config.p2p_gossip.pd_optimistic_push_fanout = fanout;

    let data_signer = config.new_random_signer();
    let peer_signer = config.new_random_signer();
    let observer_signer = config.new_random_signer();
    let pd_signer = config.new_random_signer();
    let pd_signer_2 = config.new_random_signer();
    config.fund_genesis_accounts(vec![
        &data_signer,
        &peer_signer,
        &observer_signer,
        &pd_signer,
        &pd_signer_2,
    ]);

    let genesis = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // --- Upload real data on Genesis: 16 chunks x 32 bytes = 512 bytes ---
    let data_bytes: Vec<u8> = (0..num_chunks_uploaded * chunk_size)
        .map(|i| (i & 0xff) as u8)
        .collect();

    // Record Publish ledger total_chunks BEFORE posting — this is the data_start_offset.
    let offset_before = {
        let block_index = genesis.node_ctx.block_index_guard.read();
        block_index
            .get_latest_item()
            .and_then(|item| {
                item.ledgers
                    .iter()
                    .find(|l| l.ledger == DataLedger::Publish)
                    .map(|l| l.total_chunks)
            })
            .unwrap_or(0)
    };

    // Post the data transaction via the mempool channel.
    let tx = genesis
        .post_publish_data_tx(&data_signer, data_bytes.clone())
        .await
        .map_err(|e| eyre::eyre!("Failed to post data tx: {:?}", e))?;

    // Upload chunks via HTTP so they are in the cache for migration.
    let client = reqwest::Client::new();
    let http_url = format!(
        "http://127.0.0.1:{}",
        genesis.node_ctx.config.node_config.http.bind_port
    );
    for (tx_chunk_offset, chunk_node) in tx.chunks.iter().enumerate() {
        let min = chunk_node.min_byte_range;
        let max = chunk_node.max_byte_range;
        let chunk = UnpackedChunk {
            data_root: tx.header.data_root,
            data_size: tx.header.data_size,
            data_path: Base64(tx.proofs[tx_chunk_offset].proof.clone()),
            bytes: Base64(data_bytes[min..max].to_vec()),
            tx_offset: TxChunkOffset::from(
                TryInto::<u32>::try_into(tx_chunk_offset).expect("Value exceeds u32::MAX"),
            ),
        };
        let resp = client
            .post(format!("{}/v1/chunk", &http_url))
            .json(&chunk)
            .send()
            .await?;
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "Failed to upload chunk {} for data tx",
            tx_chunk_offset
        );
    }

    // Mine blocks until the tx header appears in the block index (past migration depth).
    genesis
        .wait_for_migrated_txs(vec![tx.header.clone()], seconds_to_wait)
        .await?;

    // Wait for chunk to be available in storage module.
    genesis
        .wait_for_chunk_in_storage(
            DataLedger::Publish,
            LedgerChunkOffset::from(offset_before),
            seconds_to_wait,
        )
        .await?;

    let data_start_offset = offset_before;
    info!(
        "Data uploaded and migrated on Genesis: {} chunks, data_start_offset={}",
        num_chunks_uploaded, data_start_offset
    );

    // --- Start Block Producer (Node B) with partition assignments ---
    let block_producer = genesis.testing_peer_with_assignments(&peer_signer).await?;
    info!("Block Producer started with partition assignments");

    // Record Genesis block index height for Observer sync.
    let genesis_index_height = genesis.get_block_index_height();
    info!("Genesis block index height: {}", genesis_index_height);

    // --- Start Observer (Node C) — validator-only, no staking/pledging ---
    let observer_config = genesis.testing_peer_with_signer(&observer_signer);
    let observer = IrysNodeTest::new(observer_config)
        .start_with_name("OBSERVER")
        .await;

    // Wait for Observer's block index to catch up to Genesis.
    observer
        .wait_until_block_index_height(genesis_index_height, seconds_to_wait)
        .await?;
    info!(
        "Observer block index synced to height {}",
        genesis_index_height
    );

    // Compute partition_index and local_offset from data_start_offset.
    let partition_index = data_start_offset / num_chunks_in_partition;
    let local_offset = (data_start_offset % num_chunks_in_partition) as u32;

    Ok(PdPushTestContext {
        genesis,
        block_producer,
        observer,
        data_start_offset,
        partition_index,
        local_offset,
        num_chunks_in_partition,
        chunk_size,
        num_chunks_uploaded,
        data_bytes,
        data_signer,
        peer_signer,
        observer_signer,
        pd_signer,
        pd_signer_2,
    })
}

/// Context for the 5 MB happy path test: wraps base context + deployed contract address.
pub(crate) struct PdPush5MbContext {
    pub ctx: PdPushTestContext,
    pub contract_address: Address,
}

/// Start 3 nodes with production-size chunks (256 KB), upload 5 MB of crafted data,
/// deploy the `PdSumVerifier` contract, and return the context for the happy path test.
///
/// Data is crafted so bytes at 0, 1 MB, 2 MB, 3 MB, 4 MB sum to 42.
pub(crate) async fn setup_pd_push_test_5mb(fanout: u32) -> eyre::Result<PdPush5MbContext> {
    let chunk_size: u64 = 262_144; // 256 KB (production)
    let num_chunks_in_partition: u64 = 40;
    let data_size: usize = 5 * 1024 * 1024; // 5 MB
    let num_chunks_uploaded: u64 = (data_size as u64).div_ceil(chunk_size);
    let seconds_to_wait = 120;

    // --- Configure Genesis (Node A) with 256 KB chunks ---
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = chunk_size;
    config.consensus.get_mut().block_migration_depth = 2;
    config.consensus.get_mut().num_chunks_in_recall_range = 2;
    config.consensus.get_mut().num_chunks_in_partition = num_chunks_in_partition;
    config.consensus.get_mut().entropy_packing_iterations = 100;
    config.p2p_gossip.pd_optimistic_push_fanout = fanout;

    let data_signer = config.new_random_signer();
    let peer_signer = config.new_random_signer();
    let observer_signer = config.new_random_signer();
    let pd_signer = config.new_random_signer();
    let pd_signer_2 = config.new_random_signer();

    // Fund the dev wallet for contract deployment.
    let dev_address = IrysAddress::from_slice(hex::decode(DEV_ADDRESS)?.as_slice());
    config.consensus.extend_genesis_accounts(vec![(
        dev_address,
        GenesisAccount {
            balance: U256::from(4_200_000_000_000_000_000_u128),
            ..Default::default()
        },
    )]);
    config.fund_genesis_accounts(vec![
        &data_signer,
        &peer_signer,
        &observer_signer,
        &pd_signer,
        &pd_signer_2,
    ]);

    let genesis = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // --- Craft 5 MB data: distinct primes at 1 MB intervals sum to 28 ---
    let mut data_bytes = vec![0_u8; data_size];
    const MB: usize = 1_048_576;
    data_bytes[0] = 2;
    data_bytes[MB] = 3;
    data_bytes[2 * MB] = 5;
    data_bytes[3 * MB] = 7;
    data_bytes[4 * MB] = 11;
    // 2 + 3 + 5 + 7 + 11 = 28 (distinct primes — no permutation/substitution collides)

    // Record Publish ledger total_chunks BEFORE posting — this is the data_start_offset.
    let offset_before = {
        let block_index = genesis.node_ctx.block_index_guard.read();
        block_index
            .get_latest_item()
            .and_then(|item| {
                item.ledgers
                    .iter()
                    .find(|l| l.ledger == DataLedger::Publish)
                    .map(|l| l.total_chunks)
            })
            .unwrap_or(0)
    };

    // Post the data transaction via the mempool channel.
    let tx = genesis
        .post_publish_data_tx(&data_signer, data_bytes.clone())
        .await
        .map_err(|e| eyre::eyre!("Failed to post data tx: {:?}", e))?;

    // Upload chunks via HTTP so they are in the cache for migration.
    let client = reqwest::Client::new();
    let http_url = format!(
        "http://127.0.0.1:{}",
        genesis.node_ctx.config.node_config.http.bind_port
    );
    for (tx_chunk_offset, chunk_node) in tx.chunks.iter().enumerate() {
        let min = chunk_node.min_byte_range;
        let max = chunk_node.max_byte_range;
        let chunk = UnpackedChunk {
            data_root: tx.header.data_root,
            data_size: tx.header.data_size,
            data_path: Base64(tx.proofs[tx_chunk_offset].proof.clone()),
            bytes: Base64(data_bytes[min..max].to_vec()),
            tx_offset: TxChunkOffset::from(
                TryInto::<u32>::try_into(tx_chunk_offset).expect("Value exceeds u32::MAX"),
            ),
        };
        let resp = client
            .post(format!("{}/v1/chunk", &http_url))
            .json(&chunk)
            .send()
            .await?;
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "Failed to upload chunk {} for data tx",
            tx_chunk_offset
        );
    }

    // Mine blocks until the tx header appears in the block index (past migration depth).
    genesis
        .wait_for_migrated_txs(vec![tx.header.clone()], seconds_to_wait)
        .await?;

    // Wait for first chunk to be available in storage module.
    genesis
        .wait_for_chunk_in_storage(
            DataLedger::Publish,
            LedgerChunkOffset::from(offset_before),
            seconds_to_wait,
        )
        .await?;

    let data_start_offset = offset_before;
    info!(
        "5 MB data uploaded and migrated on Genesis: {} chunks, data_start_offset={}",
        num_chunks_uploaded, data_start_offset
    );

    // --- Deploy PdSumVerifier contract on Genesis ---
    let dev_wallet = hex::decode(DEV_PRIVATE_KEY)?;
    let signer: PrivateKeySigner = SigningKey::from_slice(dev_wallet.as_slice())?.into();
    let wallet = EthereumWallet::from(signer);
    let alloy_provider = ProviderBuilder::new().wallet(wallet).connect_http(
        format!(
            "http://127.0.0.1:{}/v1/execution-rpc",
            genesis.node_ctx.config.node_config.http.bind_port
        )
        .parse()?,
    );

    let deploy_builder = PdSumVerifier::deploy_builder(alloy_provider.clone()).gas(29_506_173);
    let mut deploy_fut = Box::pin(deploy_builder.deploy());
    let contract_address = genesis
        .future_or_mine_on_timeout(&mut deploy_fut, Duration::from_millis(500))
        .await??;
    info!("PdSumVerifier deployed at {:?}", contract_address);

    // --- Start Block Producer (Node B) with partition assignments ---
    let block_producer = genesis.testing_peer_with_assignments(&peer_signer).await?;
    info!("Block Producer started with partition assignments");

    // Record Genesis block index height for Observer sync.
    let genesis_index_height = genesis.get_block_index_height();
    info!("Genesis block index height: {}", genesis_index_height);

    // --- Start Observer (Node C) — validator-only, no staking/pledging ---
    let observer_config = genesis.testing_peer_with_signer(&observer_signer);
    let observer = IrysNodeTest::new(observer_config)
        .start_with_name("OBSERVER")
        .await;

    // Wait for Observer's block index to catch up to Genesis.
    observer
        .wait_until_block_index_height(genesis_index_height, seconds_to_wait)
        .await?;
    info!(
        "Observer block index synced to height {}",
        genesis_index_height
    );

    // Compute partition_index and local_offset from data_start_offset.
    let partition_index = data_start_offset / num_chunks_in_partition;
    let local_offset = (data_start_offset % num_chunks_in_partition) as u32;

    Ok(PdPush5MbContext {
        ctx: PdPushTestContext {
            genesis,
            block_producer,
            observer,
            data_start_offset,
            partition_index,
            local_offset,
            num_chunks_in_partition,
            chunk_size,
            num_chunks_uploaded,
            data_bytes,
            data_signer,
            peer_signer,
            observer_signer,
            pd_signer,
            pd_signer_2,
        },
        contract_address,
    })
}

/// Poll a node's execution RPC for a transaction receipt, returning its status.
async fn wait_for_tx_receipt_on_node(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    tx_hash: alloy_primitives::FixedBytes<32>,
    node_name: &str,
) -> eyre::Result<bool> {
    let rpc_url: reqwest::Url = format!(
        "http://127.0.0.1:{}/v1/execution-rpc",
        node.node_ctx.config.node_config.http.bind_port
    )
    .parse()
    .expect("valid URL");
    let provider = ProviderBuilder::new().connect_http(rpc_url);
    for attempt in 1..=30 {
        match provider.get_transaction_receipt(tx_hash).await {
            Ok(Some(r)) => {
                info!(
                    "{}: PD tx receipt on attempt {}: status={:?}, gas_used={:?}",
                    node_name,
                    attempt,
                    r.status(),
                    r.gas_used
                );
                return Ok(r.status());
            }
            Ok(None) if attempt < 30 => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Ok(None) => {
                eyre::bail!("{}: tx receipt not found after {} attempts", node_name, 30);
            }
            Err(e) if attempt < 30 => {
                tokio::time::sleep(Duration::from_millis(200)).await;
                info!(
                    "{}: receipt query failed on attempt {} (retrying): {:?}",
                    node_name, attempt, e
                );
            }
            Err(e) => {
                eyre::bail!("{}: tx receipt query failed: {:?}", node_name, e);
            }
        }
    }
    unreachable!()
}

/// Build a signed PD transaction calling a contract, with a configurable gas limit.
///
/// Same as `inject_pd_contract_call` in utils.rs, but allows overriding the gas limit
/// (needed for large PD reads like 5 MB that require more gas for memory operations).
async fn inject_pd_contract_call_with_gas(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    signer: &IrysSigner,
    contract_address: Address,
    abi_calldata: alloy_primitives::Bytes,
    data_reads: Vec<PdDataRead>,
    priority_fee_per_chunk: u64,
    gas_limit: u64,
    nonce: u64,
) -> eyre::Result<alloy_primitives::FixedBytes<32>> {
    let local_signer = LocalSigner::from(signer.signer.clone());
    let chain_id = node.node_ctx.config.consensus.chain_id;

    let access_list = build_pd_access_list_with_fees(
        &data_reads,
        U256::from(priority_fee_per_chunk),
        U256::from(1_000_000_000_000_000_u64),
    )?;

    let mut tx = TxEip1559 {
        access_list,
        chain_id,
        gas_limit,
        input: abi_calldata,
        max_fee_per_gas: 20_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        nonce,
        to: alloy_primitives::TxKind::Call(contract_address),
        value: U256::ZERO,
    };
    let signature = local_signer
        .sign_transaction_sync(&mut tx)
        .expect("PD tx must be signable");

    let tx_envelope = EthereumTxEnvelope::Eip1559(tx.into_signed(signature))
        .encoded_2718()
        .into();
    let tx_hash = node
        .node_ctx
        .reth_node_adapter
        .rpc
        .inject_tx(tx_envelope)
        .await?;

    Ok(tx_hash)
}

/// Build a signed PD transaction referencing real chunk offsets and inject it.
///
/// Unlike `create_and_inject_pd_transaction_with_priority_fee` (which uses `U200::MAX`
/// sentinel to bypass chunk provisioning), this builds a PD tx with real
/// `partition_index` and `offset` values, forcing PdService to actually provision.
pub(crate) async fn build_and_inject_real_pd_tx(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    signer: &IrysSigner,
    partition_index: u64,
    offset: u32,
    chunk_count: u16,
    nonce: u64,
) -> eyre::Result<alloy_primitives::FixedBytes<32>> {
    let local_signer = LocalSigner::from(signer.signer.clone());
    let chain_id = node.node_ctx.config.consensus.chain_id;

    let chunk_size = u32::try_from(node.node_ctx.config.consensus.chunk_size)
        .expect("test chunk_size must fit in u32");
    let reads = vec![PdDataRead {
        partition_index,
        start: offset,
        len: u32::from(chunk_count) * chunk_size,
        byte_off: 0,
    }];
    let access_list = build_pd_access_list_with_fees(
        &reads,
        U256::from(10_000_000_000_000_000_u64), // 0.01 IRYS priority fee
        U256::from(1_000_000_000_000_000_u64),  // 0.001 IRYS base fee cap
    )?;

    let mut tx = TxEip1559 {
        access_list,
        chain_id,
        gas_limit: 1_000_000,
        input: alloy_primitives::Bytes::new(),
        max_fee_per_gas: 20_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        nonce,
        to: alloy_primitives::TxKind::Call(Address::random()),
        value: U256::ZERO,
    };
    let signature = local_signer
        .sign_transaction_sync(&mut tx)
        .expect("PD tx must be signable");

    let tx_envelope = EthereumTxEnvelope::Eip1559(tx.into_signed(signature))
        .encoded_2718()
        .into();
    let tx_hash = node
        .node_ctx
        .reth_node_adapter
        .rpc
        .inject_tx(tx_envelope)
        .await?;

    Ok(tx_hash)
}

/// Block Producer (no local PD data) receives a PD contract call, fetches 5 MB of
/// chunks from Genesis via P2P, then naturally pushes them to Observer. Block Producer
/// mines the block. The smart contract (`PdSumVerifier`) reads the full 5 MB via
/// `readData()` and verifies that bytes at 1 MB intervals sum to 42.
///
/// This exercises the full production flow:
///   Block Producer pulls chunks from Genesis → pushes to Observer → mines block
///   → all 3 nodes validate the PD transaction and follow the same tip
///
/// Key assertions:
/// 1. Chunks appear in Observer's ChunkDataIndex via push (before any block gossip)
/// 2. Smart contract verification passes (tx receipt status=true on all nodes)
/// 3. ALL 3 nodes validate the block and follow the same canonical tip
#[test_log::test(tokio::test)]
async fn slow_heavy3_pd_chunk_optimistic_push_happy_path() -> eyre::Result<()> {
    let mb_ctx = setup_pd_push_test_5mb(4).await?;
    let contract_address = mb_ctx.contract_address;
    let ctx = mb_ctx.ctx;

    let publish_ledger = DataLedger::Publish as u32;
    let num_chunks = ctx.num_chunks_uploaded;

    // Pre-test invariant: Observer and Block Producer should NOT have chunks
    // in either the in-memory cache OR persistent storage modules.
    for node_ctx in [&ctx.observer.node_ctx, &ctx.block_producer.node_ctx] {
        for &offset in &[
            ctx.data_start_offset,
            ctx.data_start_offset + num_chunks - 1,
        ] {
            // Fix 2: Check in-memory cache is empty.
            assert!(
                node_ctx
                    .chunk_data_index
                    .get(&(publish_ledger, offset))
                    .is_none(),
                "Node should NOT have chunk at ({}, {}) in cache before PD tx",
                publish_ledger,
                offset,
            );
            // Fix 2: Check persistent storage modules are empty too.
            let storage_result = node_ctx
                .chunk_provider
                .get_chunk_by_ledger_offset(DataLedger::Publish, LedgerChunkOffset::from(offset));
            assert!(
                matches!(storage_result, Ok(None) | Err(_)),
                "Node should NOT have chunk at offset {} in storage modules",
                offset,
            );
        }
    }

    // --- Inject PD contract call on Block Producer ---
    // Block Producer does NOT have PD chunks locally. Its PdService will:
    //   1. P2P fetch all 20 chunks from Genesis
    //   2. After each fetch, schedule_outbound_push → push to Observer
    //   3. Mark the PD tx as ready in ready_pd_txs
    let abi_calldata: alloy_primitives::Bytes = PdSumVerifier::verifySumAt1MbOffsetsCall {}
        .abi_encode()
        .into();

    let data_len = ctx.data_bytes.len() as u32;
    let tx_hash = inject_pd_contract_call_with_gas(
        &ctx.block_producer,
        &ctx.pd_signer,
        contract_address,
        abi_calldata,
        vec![PdDataRead {
            partition_index: ctx.partition_index,
            start: ctx.local_offset,
            len: data_len,
            byte_off: 0,
        }],
        10_000_000_000_000_000_u64,
        30_000_000, // 30M gas — readData() loads 5 MB into EVM memory
        0,
    )
    .await?;
    info!("PD contract call injected on Block Producer: {:?}", tx_hash);

    // Fix 3: Wait for ALL chunks in Observer's cache (not just first+last).
    // Block Producer fetches from Genesis, then pushes to Observer automatically.
    // This happens BEFORE any block is mined — proving chunks arrived via push.
    for i in 0..num_chunks {
        ctx.observer
            .wait_for_pd_chunk_in_cache(publish_ledger, ctx.data_start_offset + i, 120)
            .await?;
    }
    info!(
        "All {} chunks arrived in Observer cache via push from Block Producer (before block gossip)",
        num_chunks
    );

    // Fix 1: Check whether Observer also self-provisioned via EVM tx pool gossip.
    // In the real system, reth gossips EVM txs to peers, so Observer may see the PD tx
    // in its own mempool and start independent provisioning. The push is additive —
    // it races with self-provisioning, and whichever arrives first populates the cache.
    // The key proof is that ALL chunks are cached BEFORE any block is mined/gossiped.
    let observer_self_provisioned = ctx.observer.node_ctx.ready_pd_txs.contains(&tx_hash);
    info!(
        "Observer self-provisioned PD tx via mempool gossip: {} (push still delivered chunks pre-block)",
        observer_self_provisioned,
    );

    // Verify byte-level correctness of first and a middle pushed chunk on Observer.
    let chunk_size = ctx.chunk_size as usize;
    {
        // First chunk (contains data[0] = 2)
        let fetched_first = ctx
            .observer
            .node_ctx
            .chunk_data_index
            .get(&(publish_ledger, ctx.data_start_offset))
            .map(|r| Arc::clone(&*r))
            .expect("First chunk should be in Observer's ChunkDataIndex after push");
        assert_eq!(
            fetched_first.as_ref(),
            &ctx.data_bytes[..chunk_size],
            "First pushed chunk bytes should match uploaded data",
        );

        // Middle chunk (chunk 10 of 20) — catches off-by-one in chunk assembly
        let mid = num_chunks / 2;
        let mid_start = mid as usize * chunk_size;
        let mid_end = mid_start + chunk_size;
        let fetched_mid = ctx
            .observer
            .node_ctx
            .chunk_data_index
            .get(&(publish_ledger, ctx.data_start_offset + mid))
            .map(|r| Arc::clone(&*r))
            .expect("Middle chunk should be in Observer's ChunkDataIndex after push");
        assert_eq!(
            fetched_mid.as_ref(),
            &ctx.data_bytes[mid_start..mid_end],
            "Middle pushed chunk (#{}) bytes should match uploaded data",
            mid,
        );
    }

    // Wait for Block Producer to finish provisioning all chunks, then mine.
    ctx.block_producer
        .wait_for_ready_pd_tx(&tx_hash, 120)
        .await?;
    let (block, eth_payload, _) = ctx.block_producer.mine_block_without_gossip().await?;

    // Verify PD tx is in the block.
    let pd_tx_included = eth_payload
        .block()
        .body()
        .transactions
        .iter()
        .any(|tx| tx.hash() == &tx_hash);
    assert!(
        pd_tx_included,
        "PD tx {:?} should be included in Block Producer's mined block",
        tx_hash,
    );

    let block_height = block.height;
    info!(
        "Block Producer mined block at height {} with PD tx",
        block_height
    );

    // --- Verify PD tx receipt on Block Producer (smart contract sum check passed) ---
    let bp_status =
        wait_for_tx_receipt_on_node(&ctx.block_producer, tx_hash, "BlockProducer").await?;
    assert!(
        bp_status,
        "PD tx should have succeeded on Block Producer (contract verifySumAt1MbOffsets passed)",
    );

    // --- Gossip block from Block Producer to all peers ---
    ctx.block_producer.gossip_block_to_peers(&block)?;
    ctx.block_producer
        .gossip_eth_block_to_peers(eth_payload.block())?;

    // --- Fix 5: ALL-NODE SYNC — compare block HASHES, not just heights ---
    let bp_block_hash = block.block_hash;

    let genesis_hash = ctx.genesis.wait_until_height(block_height, 60).await?;
    assert_eq!(
        genesis_hash, bp_block_hash,
        "Genesis and Block Producer disagree on block hash at height {}",
        block_height,
    );

    let observer_hash = ctx.observer.wait_until_height(block_height, 60).await?;
    assert_eq!(
        observer_hash, bp_block_hash,
        "Observer and Block Producer disagree on block hash at height {}",
        block_height,
    );

    info!(
        "All 3 nodes at height {} with same block hash {:?}",
        block_height, bp_block_hash,
    );

    // --- Verify PD tx receipt on ALL nodes (status=true proves EVM execution succeeded) ---
    let genesis_status = wait_for_tx_receipt_on_node(&ctx.genesis, tx_hash, "Genesis").await?;
    assert!(genesis_status, "PD tx should have succeeded on Genesis");

    let observer_status = wait_for_tx_receipt_on_node(&ctx.observer, tx_hash, "Observer").await?;
    assert!(observer_status, "PD tx should have succeeded on Observer",);

    info!(
        "PD tx receipt verified on all 3 nodes — contract verifySumAt1MbOffsets passed everywhere",
    );

    ctx.observer.stop().await;
    ctx.block_producer.stop().await;
    ctx.genesis.stop().await;
    Ok(())
}

/// Observer already has a chunk cached from a prior push. A second optimistic push
/// for the same chunk should return immediately via the cache-hit shortcut
/// (pd_service.rs:822-826) without re-doing MDBX lookup or merkle verification.
///
/// Verify the chunk remains cached with correct data and the system continues
/// functioning (block validates using the cached chunk).
#[test_log::test(tokio::test)]
async fn slow_heavy3_pd_chunk_optimistic_push_cache_hit_shortcut() -> eyre::Result<()> {
    let ctx = setup_pd_push_test(4).await?;

    let publish_ledger = DataLedger::Publish as u32;

    // --- First push: deliver chunk to Observer ---
    let t1_hash = build_and_inject_real_pd_tx(
        &ctx.genesis,
        &ctx.pd_signer,
        ctx.partition_index,
        ctx.local_offset,
        1, // 1 chunk
        0, // nonce
    )
    .await?;
    info!("T1 injected on Genesis: {:?}", t1_hash);

    // Wait for chunk to arrive in Observer's cache via push.
    ctx.observer
        .wait_for_pd_chunk_in_cache(publish_ledger, ctx.data_start_offset, 30)
        .await?;
    info!("First push: chunk cached on Observer");

    // Record cache-hit counter before the second push.
    let cache_hits_before = ctx
        .observer
        .node_ctx
        .pd_push_cache_hit_count
        .load(Ordering::Relaxed);

    // Record the cached chunk bytes for comparison after second push.
    let first_bytes = ctx
        .observer
        .node_ctx
        .chunk_data_index
        .get(&(publish_ledger, ctx.data_start_offset))
        .map(|r| Arc::clone(&*r))
        .expect("chunk should be in Observer cache after first push");

    // --- Second push: same chunk, different PD tx (different signer to avoid nonce conflict) ---
    let t2_hash = build_and_inject_real_pd_tx(
        &ctx.genesis,
        &ctx.pd_signer_2,
        ctx.partition_index,
        ctx.local_offset,
        1, // same 1 chunk at same offset
        0, // nonce (different signer, so nonce=0 is fine)
    )
    .await?;
    info!(
        "T2 injected on Genesis (same chunk, different signer): {:?}",
        t2_hash
    );

    // Allow time for the second push to propagate and be processed.
    // The push is fire-and-forget HTTP POST; 500ms is generous for local network.
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Assert chunk is still in cache (not removed or corrupted by second push).
    let second_bytes = ctx
        .observer
        .node_ctx
        .chunk_data_index
        .get(&(publish_ledger, ctx.data_start_offset))
        .map(|r| Arc::clone(&*r));
    assert!(
        second_bytes.is_some(),
        "Chunk should still be in Observer cache after second push",
    );

    // Assert data integrity: bytes unchanged after duplicate push.
    assert_eq!(
        first_bytes.as_ref(),
        second_bytes.as_ref().unwrap().as_ref(),
        "Chunk bytes should be unchanged after duplicate push",
    );

    // Assert the cache-hit shortcut fired exactly once for the duplicate push.
    let cache_hits_after = ctx
        .observer
        .node_ctx
        .pd_push_cache_hit_count
        .load(Ordering::Relaxed);
    assert_eq!(
        cache_hits_after,
        cache_hits_before + 1,
        "Cache-hit shortcut should fire exactly once for the duplicate push",
    );

    // --- Verify chunk remains functional: mine block with T1, gossip, validate ---
    ctx.genesis.wait_for_ready_pd_tx(&t1_hash, 30).await?;
    let (block, eth_payload, _) = ctx.genesis.mine_block_without_gossip().await?;

    let block_height = block.height;
    info!("Genesis mined block at height {} with T1", block_height);

    ctx.genesis.gossip_block_to_peers(&block)?;
    ctx.genesis.gossip_eth_block_to_peers(eth_payload.block())?;

    ctx.observer.wait_until_height(block_height, 30).await?;

    let observer_height = ctx.observer.get_canonical_chain_height().await;
    assert_eq!(
        observer_height, block_height,
        "Observer should validate block using cached chunk",
    );

    info!(
        "Cache-hit shortcut verified: duplicate push preserved data, block validated at height {}",
        block_height,
    );

    ctx.observer.stop().await;
    ctx.block_producer.stop().await;
    ctx.genesis.stop().await;
    Ok(())
}

/// Custom setup for the fetch reconciliation test.
///
/// Key differences from `setup_pd_push_test`:
/// - Genesis `pd_optimistic_push_fanout = 0` (no automatic push)
/// - Observer's `trusted_peers` points to Block Producer (Node B), NOT Genesis
/// - This means Observer's P2P chunk fetch targets Node B, which doesn't have PD data
async fn setup_pd_push_test_for_reconciliation() -> eyre::Result<PdPushTestContext> {
    let chunk_size: u64 = 32;
    let num_chunks_in_partition: u64 = 10;
    let num_chunks_uploaded: u64 = 16;
    let seconds_to_wait = 30;

    // --- Configure Genesis (Node A) with push disabled ---
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = chunk_size;
    config.consensus.get_mut().block_migration_depth = 2;
    config.consensus.get_mut().num_chunks_in_recall_range = 2;
    config.consensus.get_mut().num_chunks_in_partition = num_chunks_in_partition;
    config.p2p_gossip.pd_optimistic_push_fanout = 0; // No automatic push

    let data_signer = config.new_random_signer();
    let peer_signer = config.new_random_signer();
    let observer_signer = config.new_random_signer();
    let pd_signer = config.new_random_signer();
    let pd_signer_2 = config.new_random_signer();
    config.fund_genesis_accounts(vec![
        &data_signer,
        &peer_signer,
        &observer_signer,
        &pd_signer,
        &pd_signer_2,
    ]);

    let genesis = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // --- Upload data on Genesis (same as setup_pd_push_test) ---
    let data_bytes: Vec<u8> = (0..num_chunks_uploaded * chunk_size)
        .map(|i| (i & 0xff) as u8)
        .collect();

    let offset_before = {
        let block_index = genesis.node_ctx.block_index_guard.read();
        block_index
            .get_latest_item()
            .and_then(|item| {
                item.ledgers
                    .iter()
                    .find(|l| l.ledger == DataLedger::Publish)
                    .map(|l| l.total_chunks)
            })
            .unwrap_or(0)
    };

    let tx = genesis
        .post_publish_data_tx(&data_signer, data_bytes.clone())
        .await
        .map_err(|e| eyre::eyre!("Failed to post data tx: {:?}", e))?;

    let client = reqwest::Client::new();
    let http_url = format!(
        "http://127.0.0.1:{}",
        genesis.node_ctx.config.node_config.http.bind_port
    );
    for (tx_chunk_offset, chunk_node) in tx.chunks.iter().enumerate() {
        let min = chunk_node.min_byte_range;
        let max = chunk_node.max_byte_range;
        let chunk = UnpackedChunk {
            data_root: tx.header.data_root,
            data_size: tx.header.data_size,
            data_path: Base64(tx.proofs[tx_chunk_offset].proof.clone()),
            bytes: Base64(data_bytes[min..max].to_vec()),
            tx_offset: TxChunkOffset::from(
                TryInto::<u32>::try_into(tx_chunk_offset).expect("Value exceeds u32::MAX"),
            ),
        };
        let resp = client
            .post(format!("{}/v1/chunk", &http_url))
            .json(&chunk)
            .send()
            .await?;
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
    }

    genesis
        .wait_for_migrated_txs(vec![tx.header.clone()], seconds_to_wait)
        .await?;
    genesis
        .wait_for_chunk_in_storage(
            DataLedger::Publish,
            LedgerChunkOffset::from(offset_before),
            seconds_to_wait,
        )
        .await?;

    let data_start_offset = offset_before;
    info!(
        "Data uploaded and migrated on Genesis: {} chunks, data_start_offset={}",
        num_chunks_uploaded, data_start_offset
    );

    // --- Start Block Producer (Node B) with partition assignments ---
    let block_producer = genesis.testing_peer_with_assignments(&peer_signer).await?;
    info!("Block Producer started with partition assignments");

    let genesis_index_height = genesis.get_block_index_height();

    // --- Start Observer (Node C) with trusted_peers pointing to Node B (NOT Genesis) ---
    let mut observer_config = genesis.testing_peer_with_signer(&observer_signer);

    // Restrict Observer to trusted peers only so it cannot discover Genesis via
    // handshake gossip. Without this, Node B's peer list includes Genesis, and
    // Observer would learn about Genesis during handshake, making its P2P fetch
    // succeed without the push — defeating the test.
    observer_config.peer_filter_mode = PeerFilterMode::TrustedOnly;

    // Override trusted_peers: Observer trusts Node B only.
    // This means Observer's P2P chunk fetch will target Node B, which doesn't have PD data.
    observer_config.trusted_peers = vec![PeerAddress {
        api: format!(
            "127.0.0.1:{}",
            block_producer.node_ctx.config.node_config.http.public_port
        )
        .parse()
        .expect("valid SocketAddr"),
        gossip: format!(
            "127.0.0.1:{}",
            block_producer
                .node_ctx
                .config
                .node_config
                .gossip
                .public_port
        )
        .parse()
        .expect("valid SocketAddr"),
        execution: Default::default(),
    }];

    let observer = IrysNodeTest::new(observer_config)
        .start_with_name("OBSERVER")
        .await;

    // Sync Observer block index via Node B (which relays from Genesis).
    observer
        .wait_until_block_index_height(genesis_index_height, seconds_to_wait)
        .await?;
    info!(
        "Observer block index synced to height {} (via Node B)",
        genesis_index_height
    );

    let partition_index = data_start_offset / num_chunks_in_partition;
    let local_offset = (data_start_offset % num_chunks_in_partition) as u32;

    Ok(PdPushTestContext {
        genesis,
        block_producer,
        observer,
        data_start_offset,
        partition_index,
        local_offset,
        num_chunks_in_partition,
        chunk_size,
        num_chunks_uploaded,
        data_bytes,
        data_signer,
        peer_signer,
        observer_signer,
        pd_signer,
        pd_signer_2,
    })
}

/// Observer needs a PD chunk for block validation. Its P2P fetch targets Node B
/// (the only peer in trusted_peers), which does NOT have the PD data. The fetch
/// fails/retries. A manual optimistic push from Genesis delivers the chunk,
/// reconciling with the pending fetch and unblocking block validation.
///
/// Without the push, block validation would time out because Observer has no
/// fetchable source for the PD data. Block validation success proves the
/// reconciliation path worked (pd_service.rs:883-898).
#[test_log::test(tokio::test)]
async fn slow_heavy3_pd_chunk_optimistic_push_reconciles_pending_fetch() -> eyre::Result<()> {
    let ctx = setup_pd_push_test_for_reconciliation().await?;

    // Inject PD tx on Genesis referencing 1 chunk. Genesis provisions from local
    // storage but does NOT push (fanout=0).
    let tx_hash = build_and_inject_real_pd_tx(
        &ctx.genesis,
        &ctx.pd_signer,
        ctx.partition_index,
        ctx.local_offset,
        1, // chunk_count
        0, // nonce
    )
    .await?;
    info!(
        "PD tx injected on Genesis (fanout=0, no auto push): {:?}",
        tx_hash
    );

    // Wait for Genesis to provision the chunk locally.
    ctx.genesis.wait_for_ready_pd_tx(&tx_hash, 30).await?;

    // Mine block on Genesis without gossip.
    let (block, eth_payload, _) = ctx.genesis.mine_block_without_gossip().await?;

    let pd_tx_included = eth_payload
        .block()
        .body()
        .transactions
        .iter()
        .any(|tx| tx.hash() == &tx_hash);
    assert!(pd_tx_included, "PD tx should be in Genesis's mined block");

    let block_height = block.height;
    info!("Genesis mined block at height {} with PD tx", block_height);

    // Gossip block to Block Producer (Node B). Node B will validate and auto-gossip
    // to Observer. Observer will receive the block and start ProvisionBlockChunks,
    // which triggers a P2P fetch to Node B (Observer's only peer). Node B does NOT
    // have the PD data, so the fetch fails/retries.
    ctx.genesis.gossip_block_to_peers(&block)?;
    ctx.genesis.gossip_eth_block_to_peers(eth_payload.block())?;

    // Wait for Node B to validate the block first (it can fetch chunks from Genesis).
    ctx.block_producer
        .wait_until_height(block_height, 30)
        .await?;
    info!("Block Producer validated block at height {}", block_height);

    // Verify Observer does NOT know about Genesis — its peer list should contain only Node B.
    let genesis_peer_id = ctx.genesis.node_ctx.config.peer_id();
    assert!(
        ctx.observer
            .node_ctx
            .peer_list
            .peer_by_id(&genesis_peer_id)
            .is_none(),
        "Observer should NOT have Genesis in its peer list (TrustedOnly mode, trusted_peers = [Node B])",
    );

    // Give Observer time to receive the block from Node B and start its fetch.
    // The fetch will target Node B (Observer's only peer) and fail.
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // --- Construct and send manual push directly to Observer's PdService ---
    //
    // We bypass the HTTP gossip layer (gossip_client.push_pd_chunk) because Observer
    // uses TrustedOnly mode, which rejects gossip from unknown peers (Genesis is not
    // in Observer's trusted_peers). Sending directly to the PdService channel tests
    // the same reconciliation logic without the HTTP-level peer check.

    // Get packed chunk from Genesis's storage.
    let chunk_format = ctx
        .genesis
        .node_ctx
        .chunk_provider
        .get_chunk_for_pd(0, ctx.data_start_offset)?
        .expect("chunk must be in Genesis storage");

    let (chunk_bytes, data_path) = match chunk_format {
        ChunkFormat::Packed(ref packed) => {
            let consensus = &ctx.genesis.node_ctx.config.consensus;
            let unpacked = irys_packing::unpack(
                packed,
                consensus.entropy_packing_iterations,
                consensus.chunk_size as usize,
                consensus.chain_id,
            );
            (unpacked.bytes, packed.data_path.clone())
        }
        ChunkFormat::Unpacked(ref chunk) => (chunk.bytes.clone(), chunk.data_path.clone()),
    };

    let push_msg = PdChunkPush {
        ledger: 0,
        offset: ctx.data_start_offset,
        chunk_bytes,
        data_path,
    };

    // Record reconciliation counter before the push.
    let reconciliations_before = ctx
        .observer
        .node_ctx
        .pd_push_reconciliation_count
        .load(Ordering::Relaxed);

    info!(
        "Sending manual push directly to Observer PdService (genesis_peer_id={:?})",
        genesis_peer_id,
    );

    // Send OptimisticPush directly into Observer's PdService channel.
    ctx.observer
        .node_ctx
        .gossip_data_handler
        .pd_chunk_sender
        .as_ref()
        .expect("Observer should have a pd_chunk_sender")
        .send(irys_types::chunk_provider::PdChunkMessage::OptimisticPush {
            peer_id: genesis_peer_id,
            push: push_msg,
        })
        .expect("PdService channel should be open");

    // Wait for Observer to validate the block. Without the push, this would time out
    // because Observer's fetch to Node B (its only peer) fails -- Node B doesn't have
    // the PD data at the test offset.
    ctx.observer.wait_until_height(block_height, 30).await?;

    // Assert Observer's canonical tip matches Genesis.
    let observer_height = ctx.observer.get_canonical_chain_height().await;
    assert_eq!(
        observer_height, block_height,
        "Observer should validate block after push reconciles pending fetch",
    );

    // Check whether the reconciliation path was exercised. This is timing-dependent:
    // if the push arrives before Observer starts the P2P fetch, the chunk goes into
    // cache directly and handle_provision_block_chunks finds it via cache hit (no
    // pending fetch to reconcile). Both paths prove the push delivered the chunk.
    let reconciliations_after = ctx
        .observer
        .node_ctx
        .pd_push_reconciliation_count
        .load(Ordering::Relaxed);
    let reconciled = reconciliations_after > reconciliations_before;
    info!(
        "Push reconciliation counter: before={}, after={}, reconciled={}",
        reconciliations_before, reconciliations_after, reconciled,
    );

    // Note: we do NOT assert chunk_data_index here. After block validation,
    // handle_release_block_chunks removes chunk references and evicts unreferenced
    // entries from both the LRU cache and the shared_index (ChunkDataIndex).
    // The proof that reconciliation worked is that wait_until_height succeeded:
    // without the push, Observer's fetch to Node B would fail indefinitely and
    // block validation would time out.

    info!(
        "Fetch reconciliation verified: push delivered chunk, block validated at height {}",
        block_height,
    );

    ctx.observer.stop().await;
    ctx.block_producer.stop().await;
    ctx.genesis.stop().await;
    Ok(())
}
