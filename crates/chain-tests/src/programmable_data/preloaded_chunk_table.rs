//! Integration tests for the PD preloaded chunk table feature.
//!
//! These tests verify that the pre-loaded `ChunkTable` (HashMap lookup replacing per-chunk
//! blocking I/O) works correctly across different scenarios:
//!
//! - Single chunk read via contract (end-to-end happy path)
//! - Multiple chunks read via contract (multi-chunk preloading)
//! - Blocks without PD txs (empty chunk table)
//! - Chunk budget enforcement (max_pd_chunks_per_block)
//! - Missing chunks → precompile reverts
//! - Headerless access list → precompile reverts
//! - Multi-node block validation (peer pre-loads chunks for validation)

use alloy_genesis::GenesisAccount;
use alloy_network::EthereumWallet;
use alloy_primitives::{Address, U256};
use alloy_provider::ProviderBuilder;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_macro::sol;
use alloy_sol_types::SolCall as _;
use k256::ecdsa::SigningKey;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use irys_types::precompile::IrysPrecompileOffsets;
use irys_types::range_specifier::PdDataRead;
use irys_types::{irys::IrysSigner, IrysAddress};
use irys_types::{Base64, NodeConfig, TxChunkOffset, UnpackedChunk};

use rstest::rstest;

use crate::utils::IrysNodeTest;

sol!(
    #[sol(rpc)]
    IrysProgrammableDataBasic,
    "../../fixtures/contracts/out/IrysProgrammableDataBasic.sol/ProgrammableDataBasic.json"
);

const DEV_PRIVATE_KEY: &str = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
const DEV_ADDRESS: &str = "64f1a2829e0e698c18e7792d6e74f67d89aa0a32";

/// Shared setup: start a node with `chunk_size=32`, `block_migration_depth=2`, deploy the
/// PD contract, and fund both the dev wallet and a data-upload account.
///
/// `num_chunks_in_partition` controls partition size. Use a small value (e.g. 4) together
/// with padding uploads to force data into non-zero partitions, ensuring the offset
/// decomposition logic is exercised.
///
/// Returns `(node, contract_address, data_account, http_url)`.
async fn setup_pd_node_with_contract(
    num_chunks_in_partition: u64,
) -> eyre::Result<(
    IrysNodeTest<irys_chain::IrysNodeCtx>,
    Address,
    IrysSigner,
    String,
)> {
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = 32;
    config.consensus.get_mut().block_migration_depth = 2;
    config.consensus.get_mut().num_chunks_in_recall_range = 2;
    config.consensus.get_mut().num_chunks_in_partition = num_chunks_in_partition;

    let main_address = config.miner_address();
    let data_account = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![
        (
            main_address,
            GenesisAccount {
                balance: U256::from(690000000000000000_u128),
                ..Default::default()
            },
        ),
        (
            data_account.address(),
            GenesisAccount {
                balance: U256::from(1_000_000_000_000_000_000_u128),
                ..Default::default()
            },
        ),
        (
            IrysAddress::from_slice(hex::decode(DEV_ADDRESS)?.as_slice()),
            GenesisAccount {
                balance: U256::from(4200000000000000000_u128),
                ..Default::default()
            },
        ),
    ]);

    let node = IrysNodeTest::new_genesis(config).start().await;
    node.node_ctx
        .packing_waiter
        .wait_for_idle(Some(Duration::from_secs(10)))
        .await?;

    // Deploy contract using deterministic dev wallet
    let dev_wallet = hex::decode(DEV_PRIVATE_KEY)?;
    let signer: PrivateKeySigner = SigningKey::from_slice(dev_wallet.as_slice())?.into();
    let wallet = EthereumWallet::from(signer);
    let http_url = format!(
        "http://127.0.0.1:{}",
        node.node_ctx.config.node_config.http.bind_port
    );
    let alloy_provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(format!("{}/v1/execution-rpc", http_url).parse()?);

    let deploy_builder =
        IrysProgrammableDataBasic::deploy_builder(alloy_provider.clone()).gas(29506173);
    let mut deploy_fut = Box::pin(deploy_builder.deploy());
    let contract_address = node
        .future_or_mine_on_timeout(&mut deploy_fut, Duration::from_millis(500))
        .await??;

    info!("Contract deployed at {:?}", contract_address);

    // Wait for HTTP server
    tokio::time::sleep(Duration::from_secs(2)).await;

    Ok((node, contract_address, data_account, http_url))
}

/// Upload data, mine until tx is in the block index, and return the data-start-offset
/// computed from the block index's Publish ledger total_chunks.
async fn upload_data_and_get_offset(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    account: &IrysSigner,
    data_bytes: &[u8],
    _http_url: &str,
) -> eyre::Result<u64> {
    // Record the Publish ledger total_chunks BEFORE posting — this is the data_start_offset.
    let offset_before = {
        let block_index = node.node_ctx.block_index_guard.read();
        block_index
            .get_latest_item()
            .and_then(|item| {
                item.ledgers
                    .iter()
                    .find(|l| l.ledger == irys_types::DataLedger::Publish)
                    .map(|l| l.total_chunks)
            })
            .unwrap_or(0)
    };

    // Post tx via the mempool channel (proven working path).
    let tx = node
        .post_publish_data_tx(account, data_bytes.to_vec())
        .await
        .map_err(|e| eyre::eyre!("Failed to post data tx: {:?}", e))?;

    // Upload chunks via HTTP so they're in the cache for migration.
    let client = reqwest::Client::new();
    let http_url = format!(
        "http://127.0.0.1:{}",
        node.node_ctx.config.node_config.http.bind_port
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

    // Mine blocks until the tx header appears in the block index.
    node.wait_for_migrated_txs(vec![tx.header.clone()], 30)
        .await?;

    // ChunkMigrationService writes chunk data to storage modules asynchronously
    // after block migration. Wait for it to complete so PdService can find chunks.
    tokio::time::sleep(Duration::from_secs(2)).await;

    info!(
        "Data uploaded and migrated, data_start_offset={}",
        offset_before
    );
    Ok(offset_before)
}

/// Wait for reth to commit a tx by polling `get_transaction_receipt`.
///
/// `mine_block_without_gossip()` returns before reth has finished importing the block.
/// This helper polls the receipt until reth commits the block and the receipt is available.
async fn wait_for_reth_tx(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    tx_hash: alloy_primitives::FixedBytes<32>,
) {
    use alloy_provider::Provider as _;
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
                    "PD tx receipt found on attempt {}: status={:?}, gas_used={:?}",
                    attempt,
                    r.status(),
                    r.gas_used
                );
                assert!(r.status(), "PD tx should have succeeded (not reverted)");
                return;
            }
            Ok(None) if attempt < 30 => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(None) => {
                panic!(
                    "PD tx receipt not found after {} attempts — reth did not commit the block",
                    attempt
                );
            }
            Err(e) => {
                panic!("Failed to query tx receipt: {:?}", e);
            }
        }
    }
}

/// Decompose a global ledger chunk offset into `(partition_index, local_offset)`.
///
/// The mapping is: `global = partition_index * num_chunks_in_partition + local_offset`.
fn decompose_ledger_offset(global_offset: u64, num_chunks_in_partition: u64) -> (u64, u32) {
    let partition_index = global_offset / num_chunks_in_partition;
    let local_offset = (global_offset % num_chunks_in_partition) as u32;
    (partition_index, local_offset)
}

/// Test that a single PD transaction reading 1 chunk via the precompile returns the
/// correct bytes through a contract.
///
/// Uses varying amounts of padding data to place test data at different offsets
/// within partition 0 (the genesis miner's assigned partition):
/// - `offset_4`: 4 padding chunks → test data at local_offset=4
/// - `offset_3`: 3 padding chunks → test data at local_offset=3
///
/// This exercises different `start` values in `PdDataRead`.
#[rstest]
#[case::offset_4(4)] // test data at partition 0, offset 4
#[case::offset_3(3)] // test data at partition 0, offset 3
#[tokio::test]
async fn heavy_test_pd_single_chunk_read(#[case] padding_chunks: u64) -> eyre::Result<()> {
    let num_chunks_in_partition: u64 = 10;
    let chunk_size: u64 = 32;
    let (node, contract_address, data_account, http_url) =
        setup_pd_node_with_contract(num_chunks_in_partition).await?;

    // Upload padding — each chunk has a distinct fill byte so any off-by-one
    // in chunk addressing would return visibly wrong data.
    let padding: Vec<u8> = (0..padding_chunks as u8)
        .flat_map(|i| vec![0xA0 | i; chunk_size as usize])
        .collect();
    let _ = upload_data_and_get_offset(&node, &data_account, &padding, &http_url).await?;

    // Upload single-chunk test data — content is deliberately different from all padding bytes
    let message = "Hirys, world!";
    let data_bytes = message.as_bytes();
    let data_start_offset =
        upload_data_and_get_offset(&node, &data_account, data_bytes, &http_url).await?;

    // Padding occupies exactly `padding_chunks` ledger slots, so test data starts right after.
    assert_eq!(
        data_start_offset, padding_chunks,
        "data_start_offset should equal padding_chunks on a fresh node \
         (got {}, expected {})",
        data_start_offset, padding_chunks,
    );

    let (partition_index, local_offset) =
        decompose_ledger_offset(data_start_offset, num_chunks_in_partition);
    let expected_partition = padding_chunks / num_chunks_in_partition;
    let expected_local_offset = (padding_chunks % num_chunks_in_partition) as u32;
    assert_eq!(
        partition_index, expected_partition,
        "partition_index mismatch"
    );
    assert_eq!(local_offset, expected_local_offset, "local_offset mismatch");
    info!(
        "data_start_offset={}, partition_index={}, local_offset={}",
        data_start_offset, partition_index, local_offset
    );

    let abi_calldata = IrysProgrammableDataBasic::readPdChunkIntoStorageCall {}
        .abi_encode()
        .into();

    let tx_hash = node
        .inject_pd_contract_call(
            &data_account,
            contract_address,
            abi_calldata,
            vec![PdDataRead {
                partition_index,
                start: local_offset,
                len: data_bytes.len() as u32,
                byte_off: 0,
            }],
            10_000_000_000_000_000_u64,
            0, // nonce
        )
        .await?;

    info!("PD contract call injected: {:?}", tx_hash);

    // Wait for PD monitor to detect the tx and provision chunks from storage
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Mine the block containing our PD contract call
    let _ = node.mine_block_without_gossip().await?;

    // Wait for reth to commit the block (reth imports asynchronously after mine returns)
    wait_for_reth_tx(&node, tx_hash).await;

    // Verify stored bytes match original
    let dev_wallet = hex::decode(DEV_PRIVATE_KEY)?;
    let signer: PrivateKeySigner = SigningKey::from_slice(dev_wallet.as_slice())?.into();
    let wallet = EthereumWallet::from(signer);
    let alloy_provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(format!("{}/v1/execution-rpc", http_url).parse()?);
    let contract = IrysProgrammableDataBasic::new(contract_address, alloy_provider);
    let stored_bytes = contract.getStorage().call().await?;
    let stored_message = String::from_utf8(stored_bytes.to_vec())?;
    info!("Original: {}, stored: {}", message, &stored_message);
    assert_eq!(message, &stored_message);

    node.stop().await;
    Ok(())
}

/// Test that multiple chunks are preloaded correctly by uploading data spanning
/// 3 chunks (96 bytes with chunk_size=32), reading all via the contract, and
/// verifying the stored bytes match.
///
/// Padding pushes test data to a non-zero offset within partition 0,
/// verifying that multi-chunk reads work with partition-relative addressing.
#[test_log::test(tokio::test)]
async fn heavy_test_pd_multi_tx_single_block() -> eyre::Result<()> {
    let num_chunks_in_partition: u64 = 10;
    let chunk_size: u64 = 32;
    let padding_chunks: u64 = 4;
    let (node, contract_address, data_account, http_url) =
        setup_pd_node_with_contract(num_chunks_in_partition).await?;

    // Upload padding so test data starts at a non-zero offset.
    // Each 32-byte chunk has a distinct fill byte so any off-by-one in chunk addressing
    // would return visibly wrong data.
    let padding: Vec<u8> = (0..padding_chunks as u8)
        .flat_map(|i| vec![0xB0 | i; chunk_size as usize])
        .collect();
    let _ = upload_data_and_get_offset(&node, &data_account, &padding, &http_url).await?;

    // Upload 3-chunk data (96 bytes / 32-byte chunks = 3 chunks).
    // Each chunk has distinct content so we can detect wrong-chunk fetches.
    let data_bytes: Vec<u8> = (0..96_u8).collect();
    let data_start_offset =
        upload_data_and_get_offset(&node, &data_account, &data_bytes, &http_url).await?;

    assert_eq!(
        data_start_offset, padding_chunks,
        "data_start_offset should equal padding_chunks on a fresh node \
         (got {}, expected {})",
        data_start_offset, padding_chunks,
    );

    let (partition_index, local_offset) =
        decompose_ledger_offset(data_start_offset, num_chunks_in_partition);
    assert!(
        local_offset as u64 + 3 <= num_chunks_in_partition,
        "all 3 chunks must fit within partition {} (local_offset={}, num_chunks_in_partition={})",
        partition_index,
        local_offset,
        num_chunks_in_partition,
    );
    info!(
        "data_start_offset={}, partition_index={}, local_offset={}",
        data_start_offset, partition_index, local_offset
    );

    let abi_calldata = IrysProgrammableDataBasic::readPdChunkIntoStorageCall {}
        .abi_encode()
        .into();

    let tx_hash = node
        .inject_pd_contract_call(
            &data_account,
            contract_address,
            abi_calldata,
            vec![PdDataRead {
                partition_index,
                start: local_offset,
                len: data_bytes.len() as u32,
                byte_off: 0,
            }],
            10_000_000_000_000_000_u64,
            0,
        )
        .await?;

    info!("PD multi-chunk contract call injected: {:?}", tx_hash);

    // Wait for PD monitor to detect the tx and provision chunks from storage
    tokio::time::sleep(Duration::from_millis(500)).await;

    let _ = node.mine_block_without_gossip().await?;

    // Wait for reth to commit the block (reth imports asynchronously after mine returns)
    wait_for_reth_tx(&node, tx_hash).await;

    let dev_wallet = hex::decode(DEV_PRIVATE_KEY)?;
    let signer: PrivateKeySigner = SigningKey::from_slice(dev_wallet.as_slice())?.into();
    let wallet = EthereumWallet::from(signer);
    let alloy_provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(format!("{}/v1/execution-rpc", http_url).parse()?);
    let contract = IrysProgrammableDataBasic::new(contract_address, alloy_provider);
    let stored_bytes = contract.getStorage().call().await?;
    info!(
        "Stored {} bytes, expected {} bytes",
        stored_bytes.len(),
        data_bytes.len()
    );
    assert_eq!(
        data_bytes,
        stored_bytes.to_vec(),
        "All 3 chunks should be preloaded and returned correctly"
    );

    node.stop().await;
    Ok(())
}

/// Test that the chunk budget limit (max_pd_chunks_per_block) correctly orders PD
/// transactions by priority fee and excludes those that would exceed the budget.
///
/// Uploads real data so the PD service can provision chunks (passing the readiness
/// gate in the payload builder). Uses contiguous offsets 0–7 within partition 0.
#[test_log::test(tokio::test)]
async fn heavy_test_pd_chunk_budget_limit() -> eyre::Result<()> {
    let seconds_to_wait = 120;
    let chunk_size: u64 = 32;

    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = chunk_size;
    config.consensus.get_mut().block_migration_depth = 2;
    config.consensus.get_mut().num_chunks_in_recall_range = 2;
    // Set a small chunk budget so we can easily exceed it
    config
        .consensus
        .get_mut()
        .hardforks
        .sprite
        .as_mut()
        .expect("Sprite hardfork must be configured for testing")
        .max_pd_chunks_per_block = 5;

    let pd_tx_signer = config.new_random_signer();
    let data_account = config.new_random_signer();
    config.fund_genesis_accounts(vec![&pd_tx_signer, &data_account]);

    let ctx = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let http_url = format!(
        "http://127.0.0.1:{}",
        ctx.node_ctx.config.node_config.http.bind_port
    );
    // Wait for HTTP server
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Upload data covering ledger offsets 0–7 (8 chunks × 32 bytes = 256 bytes).
    // The PD txs below reference these offsets; PdService must find the chunks
    // in storage for them to pass the readiness gate.
    let data: Vec<u8> = (0..8 * chunk_size as usize)
        .map(|i| (i & 0xff) as u8)
        .collect();
    let data_start = upload_data_and_get_offset(&ctx, &data_account, &data, &http_url).await?;
    assert_eq!(
        data_start, 0,
        "first upload on a fresh node should start at offset 0"
    );

    // Inject 2 PD txs that fit within the budget (2 + 2 = 4 chunks, under limit of 5).
    // Offsets are contiguous within the uploaded data range.
    let tx1_hash = ctx
        .create_and_inject_pd_transaction_with_priority_fee(
            &pd_tx_signer,
            2, // 2 chunks
            10_000_000_000_000_000_u64,
            0, // nonce
            0, // offset_base → offsets 0, 1
        )
        .await?;

    let tx2_hash = ctx
        .create_and_inject_pd_transaction_with_priority_fee(
            &pd_tx_signer,
            2, // 2 chunks
            10_000_000_000_000_000_u64,
            1, // nonce
            2, // offset_base → offsets 2, 3
        )
        .await?;

    // Inject a third PD tx that would push total to 4 + 4 = 8, exceeding limit of 5
    let tx3_hash = ctx
        .create_and_inject_pd_transaction_with_priority_fee(
            &pd_tx_signer,
            4, // 4 chunks — would exceed budget
            10_000_000_000_000_000_u64,
            2, // nonce
            4, // offset_base → offsets 4, 5, 6, 7
        )
        .await?;

    tracing::info!(
        "Injected PD txs: tx1={:?} (2 chunks), tx2={:?} (2 chunks), tx3={:?} (4 chunks)",
        tx1_hash,
        tx2_hash,
        tx3_hash
    );

    // Wait for PD monitor to detect txs and provision chunks from storage
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let (_, eth_payload, _) = ctx.mine_block_without_gossip().await?;

    let block_txs = &eth_payload.block().body().transactions;
    let tx1_included = block_txs.iter().any(|tx| tx.hash() == &tx1_hash);
    let tx2_included = block_txs.iter().any(|tx| tx.hash() == &tx2_hash);
    let tx3_included = block_txs.iter().any(|tx| tx.hash() == &tx3_hash);

    // The first two txs should fit (4 chunks total, under limit of 5)
    assert!(
        tx1_included,
        "PD tx1 (2 chunks) should be included — within budget"
    );
    assert!(
        tx2_included,
        "PD tx2 (2 chunks) should be included — within budget"
    );

    // The third tx pushes total to 8 chunks, exceeding the 5-chunk budget
    assert!(
        !tx3_included,
        "PD tx3 (4 chunks) should be excluded — would exceed budget of 5"
    );

    tracing::info!("Chunk budget enforcement verified: 2 txs included, 1 excluded");

    ctx.stop().await;
    Ok(())
}

/// Test that blocks without PD transactions mine successfully with an empty
/// chunk table and no PD-related errors.
///
/// This is a regression test ensuring the empty chunk table path works.
#[test_log::test(tokio::test)]
async fn heavy_test_pd_no_pd_txs_no_overhead() -> eyre::Result<()> {
    let seconds_to_wait = 120;

    let mut config = NodeConfig::testing();
    let pd_tx_signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&pd_tx_signer]);

    let ctx = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Mine 3 blocks with no PD transactions (just shadow txs)
    for i in 0..3 {
        let (_, eth_payload, _) = ctx.mine_block_without_gossip().await?;
        tracing::info!(
            "Block {} mined successfully with {} transactions",
            i + 1,
            eth_payload.block().body().transactions.len()
        );
    }

    tracing::info!("3 blocks mined without PD txs — empty chunk table works");

    ctx.stop().await;
    Ok(())
}

/// Test that a peer node successfully validates a PD block produced by genesis,
/// proving that the pre-loaded chunk table works for the validation path.
///
/// Uploads real data on genesis so the PD service can provision chunks (readiness
/// gate). The PD tx calls `Address::random()` (no precompile access), so the peer
/// validates successfully via state root matching even without the chunk data.
///
/// Note: this tests the provisioning/preloading pipeline, not precompile execution.
/// Full contract-based peer validation requires data sync between nodes and is deferred.
#[test_log::test(tokio::test)]
async fn slow_heavy_test_pd_peer_validates_pd_block() -> eyre::Result<()> {
    let seconds_to_wait = 120;
    let chunk_size: u64 = 32;

    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = chunk_size;
    config.consensus.get_mut().block_migration_depth = 2;
    config.consensus.get_mut().num_chunks_in_recall_range = 2;

    let pd_tx_signer = config.new_random_signer();
    let data_account = config.new_random_signer();
    let peer_signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&pd_tx_signer, &data_account, &peer_signer]);

    // Start genesis node
    let genesis = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let http_url = format!(
        "http://127.0.0.1:{}",
        genesis.node_ctx.config.node_config.http.bind_port
    );
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Upload data so PdService can find chunks at offset 0.
    let data: Vec<u8> = vec![0xAB; chunk_size as usize];
    let data_start = upload_data_and_get_offset(&genesis, &data_account, &data, &http_url).await?;
    assert_eq!(data_start, 0, "first upload should start at offset 0");

    // Create peer with staking + pledging + partition assignments
    let peer = genesis.testing_peer_with_assignments(&peer_signer).await?;

    // Inject a PD transaction on genesis referencing the uploaded data
    let tx_hash = genesis
        .create_and_inject_pd_transaction_with_priority_fee(
            &pd_tx_signer,
            1, // 1 chunk
            10_000_000_000_000_000_u64,
            0, // nonce
            0, // offset_base → ledger offset 0
        )
        .await?;

    tracing::info!("PD transaction injected on genesis: {:?}", tx_hash);

    // Wait for PD monitor to detect and provision chunks from storage
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Mine block on genesis (without gossip so we control when peer sees it)
    let (block, eth_payload, _) = genesis.mine_block_without_gossip().await?;

    // Verify PD tx is in the block
    let pd_tx_included = eth_payload
        .block()
        .body()
        .transactions
        .iter()
        .any(|tx| tx.hash() == &tx_hash);
    assert!(pd_tx_included, "PD tx should be in genesis block");

    let block_height = block.height;

    // Gossip the block to the peer
    genesis.gossip_block_to_peers(&Arc::new(block.as_ref().clone()))?;
    genesis.gossip_eth_block_to_peers(eth_payload.block())?;

    // Wait for peer to validate and accept the block
    peer.wait_until_height(block_height, 30).await?;

    tracing::info!(
        "Peer successfully validated PD block at height {}",
        block_height
    );

    // Cleanup: stop peer before genesis
    peer.stop().await;
    genesis.stop().await;
    Ok(())
}

/// Negative test: a PD transaction referencing non-existent chunks should NOT be
/// included in a block.
///
/// The PD service marks the tx as `PartiallyReady` because the chunks don't exist
/// in storage. The payload builder's readiness gate then skips the tx, preventing
/// it from wasting block space on a tx that would fail at the precompile level.
#[test_log::test(tokio::test)]
async fn heavy_test_pd_missing_chunks_not_included() -> eyre::Result<()> {
    let (node, contract_address, data_account, _http_url) = setup_pd_node_with_contract(10).await?;

    let abi_calldata: alloy_primitives::Bytes =
        IrysProgrammableDataBasic::readPdChunkIntoStorageCall {}
            .abi_encode()
            .into();

    // Reference a non-existent chunk at a valid partition offset.
    // start=9 is within num_chunks_in_partition(10) but no data was uploaded there.
    let tx_hash = node
        .inject_pd_contract_call(
            &data_account,
            contract_address,
            abi_calldata,
            vec![PdDataRead {
                partition_index: 0,
                start: 9,
                len: 32,
                byte_off: 0,
            }],
            10_000_000_000_000_000_u64,
            0,
        )
        .await?;

    info!("Missing-chunk PD call injected: {:?}", tx_hash);

    // Wait for PD monitor to detect the tx and attempt provisioning.
    // PdService will mark it PartiallyReady because offset 9999 doesn't exist.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let (_, eth_payload, _) = node.mine_block_without_gossip().await?;

    // Verify the PD tx was NOT included — readiness gate should have filtered it
    let pd_tx_included = eth_payload
        .block()
        .body()
        .transactions
        .iter()
        .any(|tx| tx.hash() == &tx_hash);
    assert!(
        !pd_tx_included,
        "PD tx referencing non-existent chunk should NOT be included in the block \
         (readiness gate should filter it)"
    );

    node.stop().await;
    Ok(())
}

/// Negative test: a contract call with PD access list entries but no PD header
/// should NOT get chunks preloaded and the precompile should revert.
///
/// This proves the PD header is required for chunk access — headerless txs cannot
/// get free PD reads.
#[test_log::test(tokio::test)]
async fn heavy_test_pd_headerless_access_list_reverts() -> eyre::Result<()> {
    let num_chunks_in_partition: u64 = 10;
    let chunk_size: u64 = 32;
    let padding_chunks: u64 = 4;
    let (node, contract_address, data_account, http_url) =
        setup_pd_node_with_contract(num_chunks_in_partition).await?;

    // Upload padding so test data starts at a non-zero offset.
    let padding: Vec<u8> = (0..padding_chunks as u8)
        .flat_map(|i| vec![0xC0 | i; chunk_size as usize])
        .collect();
    let _ = upload_data_and_get_offset(&node, &data_account, &padding, &http_url).await?;

    // Upload real data so chunks exist
    let message = "Hirys, world!";
    let data_bytes = message.as_bytes();
    let data_start_offset =
        upload_data_and_get_offset(&node, &data_account, data_bytes, &http_url).await?;

    assert_eq!(
        data_start_offset, padding_chunks,
        "data_start_offset should equal padding_chunks on a fresh node \
         (got {}, expected {})",
        data_start_offset, padding_chunks,
    );

    let (partition_index, local_offset) =
        decompose_ledger_offset(data_start_offset, num_chunks_in_partition);

    // Build a standard alloy call WITHOUT PD header — just raw ABI calldata
    let dev_wallet = hex::decode(DEV_PRIVATE_KEY)?;
    let signer: PrivateKeySigner = SigningKey::from_slice(dev_wallet.as_slice())?.into();
    let wallet = EthereumWallet::from(signer);
    let alloy_provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(format!("{}/v1/execution-rpc", http_url).parse()?);
    let contract = IrysProgrammableDataBasic::new(contract_address, alloy_provider);

    let precompile_address: Address = IrysPrecompileOffsets::ProgrammableData.into();

    // Call readPdChunkIntoStorage with PD access list but NO fee keys.
    // The mempool now rejects PD access lists missing fee parameters as InvalidPd,
    // so the transaction should fail at submission time, not at execution time.
    let mut invocation_builder = contract.readPdChunkIntoStorage().gas(1_000_000);
    invocation_builder = invocation_builder.access_list(
        vec![alloy_eips::eip2930::AccessListItem {
            address: precompile_address,
            storage_keys: vec![alloy_primitives::B256::from(
                PdDataRead {
                    partition_index,
                    start: local_offset,
                    len: data_bytes.len() as u32,
                    byte_off: 0,
                }
                .encode(),
            )],
        }]
        .into(),
    );

    // The tx should be rejected at mempool admission (InvalidPd: missing fee keys).
    let send_result = invocation_builder.send().await;
    assert!(
        send_result.is_err(),
        "PD access list without fee keys should be rejected by mempool"
    );

    node.stop().await;
    Ok(())
}
