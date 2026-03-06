use alloy_core::primitives::{B256, U256};
use alloy_genesis::GenesisAccount;
use reth::revm::state::Bytecode;
use std::{collections::BTreeMap, fs::remove_dir_all};

use irys_reth_node_bridge::{dump::dump_state, genesis::init_state};
use irys_testing_utils::initialize_tracing;
use irys_types::{irys::IrysSigner, IrysAddress};

use crate::utils::IrysNodeTest;

#[tokio::test]
async fn heavy_test_genesis_state_dump_and_restore() -> eyre::Result<()> {
    initialize_tracing();

    let mut node = IrysNodeTest::default_async();

    let config = node.cfg.clone();

    let user_account = IrysSigner::random_signer(&node.cfg.consensus_config());
    node.cfg.consensus.extend_genesis_accounts(vec![
        (
            node.cfg.signer().address(),
            GenesisAccount {
                balance: U256::from(1),
                ..Default::default()
            },
        ),
        (
            user_account.address(),
            GenesisAccount {
                balance: U256::from(1000),
                storage: Some(BTreeMap::from_iter([
                    (B256::random(), B256::ZERO),
                    (B256::ZERO, B256::random()),
                ])),
                ..Default::default()
            },
        ),
        (
            IrysAddress::random(),
            GenesisAccount {
                balance: U256::from(1000),
                storage: Some(BTreeMap::from_iter([
                    (B256::random(), B256::ZERO),
                    (B256::ZERO, B256::random()),
                ])),
                code: Some(Bytecode::default().bytes()),
                ..Default::default()
            },
        ),
    ]);
    let node = node.start().await;

    // get reth genesis block header
    let cs = node.node_ctx.reth_handle.chain_spec();

    // mine at least one block (minimum for the dump code to work)
    node.mine_blocks(4).await?;

    let reth_db = node.node_ctx.reth_db.clone();
    let dump_dir = node.cfg.base_directory.canonicalize()?;
    // create the dump (in the temp folder for this test)
    // We need to pass the provider to access headers from static files
    let dump_path = dump_state(reth_db, &node.node_ctx.reth_handle.provider, dump_dir)?;

    let reth_data_dir = node.cfg.reth_data_dir().clone();

    // stop the node
    // keep the node reference around so we don't drop it (and clean the temp dir) prematurely
    let _node = node.stop().await;

    // wipe the reth folder
    remove_dir_all(reth_data_dir)?;

    // init genesis with the saved state
    // this function has existing checks to make sure the state_root is correct
    // between the captured state and the actual computed state root
    init_state(config, cs.clone(), dump_path, reth::tasks::Runtime::test()).await?;

    Ok(())
}
