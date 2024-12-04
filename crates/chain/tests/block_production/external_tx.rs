use std::{fs::remove_dir_all, time::Duration};

use alloy_core::primitives::{TxHash, U256};
use irys_chain::chain::start_for_testing;
use irys_config::IrysNodeConfig;
use irys_reth_node_bridge::adapter::node::RethNodeContext;
use irys_types::{
    block_production::SolutionContext, irys::IrysSigner, Address, H256, IRYS_CHAIN_ID,
};
use k256::ecdsa::SigningKey;
use reth::{providers::BlockReader, transaction_pool::TransactionPool as _};
use reth_db::Database as _;
use reth_primitives::GenesisAccount;
use tokio::time::sleep;
use tracing::info;

// hardcoded wallets
const DEV_PRIVATE_KEY: &str = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
const DEV_ADDRESS: &str = "64f1a2829e0e698c18e7792d6e74f67d89aa0a32";
const DEV2_PRIVATE_KEY: &str = "9687cbc3f9f3a0b6dffbf01ec30143e33b7c49f789257d7836eb54b9ae5d27e2";
const DEV2_ADDRESS: &str = "Bea4f456A5801cf9Af196a582D6Ec425c970c2C6";

#[tokio::test]
/// WARNING DO NOT RUN AUTOMATICALLY
/// THIS TEST BLOCKS EXPECTING AN EXTERNAL TX TO BE SUBMITTED, THROUGH A TOOL LIKE METAMASK
async fn test_basic_blockprod_extern_tx_src() -> eyre::Result<()> {
    let DEV_WALLET = hex::decode(DEV_PRIVATE_KEY)?;
    let expected_addr = hex::decode(DEV_ADDRESS)?;
    let mut config = IrysNodeConfig {
        mining_signer: IrysSigner {
            signer: SigningKey::from_slice(DEV_WALLET.as_slice())?,
            chain_id: IRYS_CHAIN_ID,
        },
        ..Default::default()
    };

    assert_eq!(
        config.mining_signer.address(),
        Address::from_slice(expected_addr.as_slice())
    );
    // let account1 = IrysSigner::random_signer();
    let account1_address = hex::decode(DEV2_ADDRESS)?;
    let account1 = IrysSigner {
        signer: SigningKey::from_slice(hex::decode(DEV2_PRIVATE_KEY)?.as_slice())?,
        chain_id: IRYS_CHAIN_ID,
    };
    assert_eq!(
        account1.address(),
        Address::from_slice(account1_address.as_slice())
    );

    config.extend_genesis_accounts(vec![
        (
            config.mining_signer.address(),
            GenesisAccount {
                balance: U256::from(690000000000000000 as u128),
                ..Default::default()
            },
        ),
        (
            config.mining_signer.address(),
            GenesisAccount {
                balance: U256::from(690000000000000000 as u128),
                ..Default::default()
            },
        ),
        (
            account1.address(),
            GenesisAccount {
                balance: U256::from(1),
                ..Default::default()
            },
        ),
    ]);

    if config.base_directory.exists() {
        remove_dir_all(&config.base_directory)?;
    }

    let node = start_for_testing(config).await?;

    let reth_context = RethNodeContext::new(node.reth_handle.into()).await?;

    while reth_context.inner.pool.pending_transactions().is_empty() {
        info!("waiting for tx...");
        sleep(Duration::from_millis(1500)).await;
    }

    let txs = reth_context.inner.pool.pending_transactions();

    info!(
        "received pending txs: {:?}",
        txs.iter()
            .map(|tx| tx.hash().clone())
            .collect::<Vec<TxHash>>()
    );

    loop {
        let (block, _) = node
            .actor_addresses
            .block_producer
            .send(SolutionContext {
                partition_hash: H256::random(),
                chunk_offset: 0,
                mining_address: Address::random(),
            })
            .await?
            .unwrap();

        //check reth for built block
        let reth_block = reth_context
            .inner
            .provider
            .block_by_hash(block.evm_block_hash)?
            .unwrap();

        // check irys DB for built block
        let db_irys_block = &node
            .db
            .view_eyre(|tx| irys_database::block_header_by_hash(tx, &block.block_hash))?
            .unwrap();

        assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());
        sleep(Duration::from_millis(10_000)).await;
    }

    Ok(())
}
