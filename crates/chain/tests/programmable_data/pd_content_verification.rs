use alloy_core::primitives::aliases::U200;
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;
use alloy_network::EthereumWallet;
use alloy_provider::ProviderBuilder;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_macro::sol;
use alloy_sol_types::SolCall as _;
use k256::ecdsa::SigningKey;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info};

use irys_api_server::routes::tx::TxOffset;
use irys_types::range_specifier::ChunkRangeSpecifier;
use irys_types::range_specifier::{ByteRangeSpecifier, U18, U34};
use irys_types::{irys::IrysSigner, IrysAddress};
use irys_types::{Base64, NodeConfig, TxChunkOffset, UnpackedChunk};

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
/// Uses small partitions (4 chunks) with padding data to force a non-zero
/// `data_start_offset`, ensuring the partition_index/offset decomposition is exercised.
#[test_log::test(tokio::test)]
async fn heavy_test_pd_content_verification() -> eyre::Result<()> {
    let num_chunks_in_partition: u64 = 4;
    let chunk_size: u64 = 32;

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

    // Upload padding data to fill partition 0, forcing test data into partition 1+.
    // Each 32-byte chunk has a distinct fill byte so any off-by-one in chunk addressing
    // would return visibly wrong data.
    let padding: Vec<u8> = (0..num_chunks_in_partition as u8)
        .flat_map(|i| vec![0xA0 | i; chunk_size as usize])
        .collect();
    let padding_price = node
        .get_data_price(irys_types::DataLedger::Publish, padding.len() as u64)
        .await?;
    let padding_tx = account1
        .create_publish_transaction(
            padding.clone(),
            node.get_anchor().await?,
            padding_price.perm_fee.into(),
            padding_price.term_fee.into(),
        )
        .unwrap();
    let padding_tx = account1.sign_transaction(padding_tx).unwrap();
    let resp = client
        .post(format!("{}/v1/tx", http_url))
        .json(&padding_tx.header)
        .send()
        .await?;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let padding_id = padding_tx.header.id.to_string();
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
    // Wait for padding to be mined and migrated
    let mut padding_offset_fut = Box::pin(async {
        let delay = Duration::from_secs(1);
        for _attempt in 1..20 {
            let response = client
                .get(format!(
                    "{}/v1/tx/{}/local/data-start-offset",
                    http_url, &padding_id
                ))
                .send()
                .await;
            let Some(response) = response.ok() else {
                sleep(delay).await;
                continue;
            };
            if response.status() == reqwest::StatusCode::OK {
                return;
            }
            sleep(delay).await;
        }
        panic!("Failed to migrate padding data after 20 attempts");
    });
    node.future_or_mine_on_timeout(&mut padding_offset_fut, Duration::from_millis(500))
        .await?;

    // Upload test data
    let message = "Hirys, world!";
    let data_bytes = message.as_bytes().to_vec();
    let price_info = node
        .get_data_price(irys_types::DataLedger::Publish, data_bytes.len() as u64)
        .await?;
    let tx = account1
        .create_publish_transaction(
            data_bytes.clone(),
            node.get_anchor().await?,
            price_info.perm_fee.into(),
            price_info.term_fee.into(),
        )
        .unwrap();
    let tx = account1.sign_transaction(tx).unwrap();

    // Post tx header
    let resp = client
        .post(format!("{}/v1/tx", http_url))
        .json(&tx.header)
        .send()
        .await?;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let id: String = tx.header.id.to_string();

    // Upload chunks immediately after header so they're available when the migration
    // service processes the block containing this tx.
    for (tx_chunk_offset, chunk_node) in tx.chunks.iter().enumerate() {
        let data_root = tx.header.data_root;
        let data_size = tx.header.data_size;
        let min = chunk_node.min_byte_range;
        let max = chunk_node.max_byte_range;
        let data_path = Base64(tx.proofs[tx_chunk_offset].proof.clone());
        let chunk = UnpackedChunk {
            data_root,
            data_size,
            data_path,
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

    // Wait for tx inclusion + migration and get data-start-offset
    let mut start_offset_fut = Box::pin(async {
        let delay = Duration::from_secs(1);
        for attempt in 1..20 {
            let response = client
                .get(format!(
                    "{}/v1/tx/{}/local/data-start-offset",
                    http_url, &id
                ))
                .send()
                .await;
            let Some(response) = response.ok() else {
                sleep(delay).await;
                continue;
            };
            if response.status() == reqwest::StatusCode::OK {
                let res: TxOffset = response.json().await.unwrap();
                debug!("start offset: {:?}", &res);
                info!("Start offset retrieved ok after {} attempts", attempt);
                return Some(res);
            }
            sleep(delay).await;
        }
        panic!("Failed to retrieve data-start-offset after 20 attempts");
    });
    let start_offset = node
        .future_or_mine_on_timeout(&mut start_offset_fut, Duration::from_millis(500))
        .await?
        .unwrap();

    let data_start_offset = start_offset.data_start_offset;

    // Exact boundary: padding fills partition 0, so test data starts at partition 1.
    assert_eq!(
        data_start_offset, num_chunks_in_partition,
        "data_start_offset should equal num_chunks_in_partition on a fresh node \
         (got {}, expected {})",
        data_start_offset, num_chunks_in_partition,
    );

    // Decompose global ledger offset into partition-relative addressing
    let partition_index = data_start_offset / num_chunks_in_partition;
    let local_offset = (data_start_offset % num_chunks_in_partition) as u32;
    assert_eq!(
        local_offset, 0,
        "test data should start at the beginning of partition 1"
    );
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
            vec![ChunkRangeSpecifier {
                partition_index: U200::from(partition_index),
                offset: local_offset,
                chunk_count: 1,
            }],
            vec![ByteRangeSpecifier {
                index: 0,
                chunk_offset: 0,
                byte_offset: U18::from(0),
                length: U34::from(data_bytes.len()),
            }],
            10_000_000_000_000_000_u64,
            0,
        )
        .await?;

    info!("PD contract call injected: {:?}", tx_hash);

    // Mine the block containing the PD contract call
    let _ = node.mine_block_without_gossip().await?;

    // Verify stored bytes match the original message
    let stored_bytes = contract.getStorage().call().await?;
    let stored_message = String::from_utf8(stored_bytes.to_vec())?;
    info!("Original: {}, stored: {}", &message, &stored_message);
    assert_eq!(&message, &stored_message);

    node.stop().await;
    Ok(())
}
