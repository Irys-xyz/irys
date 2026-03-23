use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;
use alloy_network::EthereumWallet;
use alloy_provider::{Provider as _, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_macro::sol;
use alloy_sol_types::SolCall as _;
use irys_types::range_specifier::PdDataRead;
use irys_types::{irys::IrysSigner, IrysAddress};
use irys_types::{Base64, NodeConfig, TxChunkOffset, UnpackedChunk};
use k256::ecdsa::SigningKey;
use std::time::Duration;
use tracing::info;

use crate::utils::IrysNodeTest;

sol!(
    #[sol(rpc)]
    IrysProgrammableDataBasic,
    "../../fixtures/contracts/out/IrysProgrammableDataBasic.sol/ProgrammableDataBasic.json"
);

const DEV_PRIVATE_KEY: &str = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
const DEV_ADDRESS: &str = "64f1a2829e0e698c18e7792d6e74f67d89aa0a32";

/// Verifies end-to-end that the PD precompile returns the actual chunk bytes from the
/// pre-loaded chunk table. Uploads data, mines it into the chain, calls the PD precompile
/// via a contract with a PD header prepended to calldata, then asserts the stored bytes
/// match the original data.
///
/// Uses padding data to push the test data to a non-zero offset within partition 0,
/// ensuring the partition_index/offset decomposition is exercised.
#[test_log::test(tokio::test)]
async fn heavy_test_pd_content_verification() -> eyre::Result<()> {
    let num_chunks_in_partition: u64 = 10;
    let chunk_size: u64 = 32;
    let padding_chunks: u64 = 4;

    let mut testing_config = NodeConfig::testing();
    testing_config.consensus.get_mut().chunk_size = chunk_size;
    testing_config.consensus.get_mut().block_migration_depth = 2;
    testing_config
        .consensus
        .get_mut()
        .num_chunks_in_recall_range = 2;
    testing_config.consensus.get_mut().num_chunks_in_partition = num_chunks_in_partition;
    let main_address = testing_config.miner_address();
    let account1 = IrysSigner::random_signer(&testing_config.consensus_config());
    testing_config.consensus.extend_genesis_accounts(vec![
        (
            main_address,
            GenesisAccount {
                balance: U256::from(690000000000000000_u128),
                ..Default::default()
            },
        ),
        (
            account1.address(),
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
    let node = IrysNodeTest::new_genesis(testing_config).start().await;
    node.node_ctx
        .packing_waiter
        .wait_for_idle(Some(Duration::from_secs(10)))
        .await?;

    // Deploy contract using deterministic dev wallet
    let dev_wallet = hex::decode(DEV_PRIVATE_KEY)?;
    let signer: PrivateKeySigner = SigningKey::from_slice(dev_wallet.as_slice())?.into();
    let wallet = EthereumWallet::from(signer);
    let alloy_provider = ProviderBuilder::new().wallet(wallet).connect_http(
        format!(
            "http://127.0.0.1:{}/v1/execution-rpc",
            node.node_ctx.config.node_config.http.bind_port
        )
        .parse()?,
    );

    let deploy_builder =
        IrysProgrammableDataBasic::deploy_builder(alloy_provider.clone()).gas(29506173);
    let mut deploy_fut = Box::pin(deploy_builder.deploy());
    let contract_address = node
        .future_or_mine_on_timeout(&mut deploy_fut, Duration::from_millis(500))
        .await??;
    let contract = IrysProgrammableDataBasic::new(contract_address, alloy_provider.clone());
    info!("Contract deployed at {:?}", contract.address());

    let http_url = format!(
        "http://127.0.0.1:{}",
        node.node_ctx.config.node_config.http.bind_port
    );
    let client = reqwest::Client::new();

    // Wait for the HTTP server to start
    tokio::time::sleep(Duration::from_secs(2)).await;

    let response = client.get(format!("{}/v1/info", http_url)).send().await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    // Upload padding data so test data starts at a non-zero offset within partition 0.
    let padding: Vec<u8> = (0..padding_chunks as u8)
        .flat_map(|i| vec![0xA0 | i; chunk_size as usize])
        .collect();
    let padding_tx = node
        .post_publish_data_tx(&account1, padding.clone())
        .await
        .map_err(|e| eyre::eyre!("Failed to post padding tx: {:?}", e))?;
    // Upload padding chunks
    for (tx_chunk_offset, chunk_node) in padding_tx.chunks.iter().enumerate() {
        let min = chunk_node.min_byte_range;
        let max = chunk_node.max_byte_range;
        let chunk = UnpackedChunk {
            data_root: padding_tx.header.data_root,
            data_size: padding_tx.header.data_size,
            data_path: Base64(padding_tx.proofs[tx_chunk_offset].proof.clone()),
            bytes: Base64(padding[min..max].to_vec()),
            tx_offset: TxChunkOffset::from(
                TryInto::<u32>::try_into(tx_chunk_offset).expect("Value exceeds u32::MAX"),
            ),
        };
        let resp = client
            .post(format!("{}/v1/chunk", http_url))
            .json(&chunk)
            .send()
            .await?;
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
    }
    // Mine until padding tx is in the block index
    node.wait_for_migrated_txs(vec![padding_tx.header.clone()], 30)
        .await?;
    // ChunkMigrationService writes chunk data to storage modules asynchronously
    tokio::time::sleep(Duration::from_secs(2)).await;
    info!("Padding data migrated");

    // Record current Publish ledger total_chunks — this is the data_start_offset.
    let data_start_offset = {
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

    // Upload test data
    let message = "Hirys, world!";
    let data_bytes = message.as_bytes().to_vec();
    let tx = node
        .post_publish_data_tx(&account1, data_bytes.clone())
        .await
        .map_err(|e| eyre::eyre!("Failed to post test data tx: {:?}", e))?;

    // Upload chunks so they're in the cache for migration
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
            .post(format!("{}/v1/chunk", http_url))
            .json(&chunk)
            .send()
            .await?;
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
    }

    // Mine until test data tx is in the block index
    node.wait_for_migrated_txs(vec![tx.header.clone()], 30)
        .await?;
    // ChunkMigrationService writes chunk data to storage modules asynchronously
    tokio::time::sleep(Duration::from_secs(2)).await;
    info!(
        "Test data migrated, data_start_offset={}",
        data_start_offset
    );

    assert_eq!(
        data_start_offset, padding_chunks,
        "data_start_offset should equal padding_chunks on a fresh node \
         (got {}, expected {})",
        data_start_offset, padding_chunks,
    );

    // Decompose global ledger offset into partition-relative addressing
    let partition_index = data_start_offset / num_chunks_in_partition;
    let local_offset = (data_start_offset % num_chunks_in_partition) as u32;
    info!(
        "data_start_offset={}, partition_index={}, local_offset={}",
        data_start_offset, partition_index, local_offset
    );

    // Call contract to read PD chunk via inject_pd_contract_call, which prepends the
    // PD header so the preloading gate triggers. The EVM strips the header before the
    // contract sees the calldata, so the contract receives clean ABI calldata.
    let abi_calldata: alloy_primitives::Bytes =
        IrysProgrammableDataBasic::readPdChunkIntoStorageCall {}
            .abi_encode()
            .into();

    let tx_hash = node
        .inject_pd_contract_call(
            &account1,
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

    info!("PD contract call injected: {:?}", tx_hash);

    // Wait for PD monitor to detect the tx and provision chunks from storage
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Mine the block containing the PD contract call
    let _ = node.mine_block_without_gossip().await?;

    // Wait for reth to commit the block (reth imports asynchronously after mine returns)
    let receipt = {
        let mut receipt = None;
        for attempt in 1..=20 {
            match alloy_provider.get_transaction_receipt(tx_hash).await? {
                Some(r) => {
                    info!(
                        "PD tx receipt found on attempt {}: status={:?}, gas_used={:?}",
                        attempt,
                        r.status(),
                        r.gas_used
                    );
                    receipt = Some(r);
                    break;
                }
                None if attempt < 20 => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                None => {
                    panic!(
                        "PD tx receipt not found after {} attempts — reth did not commit the block",
                        attempt
                    );
                }
            }
        }
        receipt.unwrap()
    };
    assert!(
        receipt.status(),
        "PD tx should have succeeded (not reverted)"
    );

    // Verify stored bytes match the original message
    let stored_bytes = contract.getStorage().call().await?;
    let stored_message = String::from_utf8(stored_bytes.to_vec())?;
    info!("Original: {}, stored: {}", &message, &stored_message);
    assert_eq!(&message, &stored_message);

    node.stop().await;
    Ok(())
}
