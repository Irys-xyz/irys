//! Integration tests for PdContext isolation.
//!
//! These tests verify that each EVM instance has its own isolated PdContext
//! (no cross-EVM contamination of access lists).

use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_evm::Evm as _;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, aliases::U200};
use irys_reth::evm::IrysEvmFactory;
use irys_types::chunk_provider::MockChunkProvider;
use irys_types::precompile::PD_PRECOMPILE_ADDRESS;
use irys_types::range_specifier::{
    ByteRangeSpecifier, ChunkRangeSpecifier, PdAccessListArg, U18, U34,
};
use reth_evm::{EvmEnv, EvmFactory as _};
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::database::EmptyDB;
use revm::primitives::hardfork::SpecId;
use std::sync::Arc;

fn create_access_list_with_chunks(num_chunks: u64) -> AccessList {
    let chunk_range = ChunkRangeSpecifier {
        partition_index: U200::ZERO,
        offset: 0,
        chunk_count: num_chunks as u16,
    };

    let byte_range = ByteRangeSpecifier {
        index: 0,
        chunk_offset: 0,
        byte_offset: U18::ZERO,
        length: U34::try_from(100).unwrap(),
    };

    AccessList(vec![AccessListItem {
        address: PD_PRECOMPILE_ADDRESS,
        storage_keys: vec![
            B256::from(PdAccessListArg::ChunkRead(chunk_range).encode()),
            B256::from(PdAccessListArg::ByteRead(byte_range).encode()),
        ],
    }])
}

fn create_pd_transaction(access_list: AccessList) -> TxEnv {
    TxEnv {
        caller: Address::random(),
        kind: TxKind::Call(PD_PRECOMPILE_ADDRESS),
        nonce: 0,
        gas_limit: 30_000_000,
        value: U256::ZERO,
        data: Bytes::from(vec![0, 0]), // Function ID 0, index 0
        gas_price: 0,
        chain_id: Some(1),
        gas_priority_fee: None,
        access_list,
        blob_hashes: Vec::new(),
        max_fee_per_blob_gas: 0,
        tx_type: 1,
        authorization_list: Default::default(),
    }
}

#[test]
fn test_evm_instances_have_isolated_pd_contexts() {
    // Create a single factory with test hardfork config (Sprite enabled from genesis)
    let mock_chunk_provider = Arc::new(MockChunkProvider::new());
    let hardfork_config = Arc::new(irys_types::config::ConsensusConfig::testing().hardforks);
    let factory = IrysEvmFactory::new(mock_chunk_provider, hardfork_config);

    // Create two EVMs from the same factory
    let mut cfg_env = CfgEnv::default();
    cfg_env.spec = SpecId::CANCUN;
    cfg_env.chain_id = 1;
    let block_env = BlockEnv {
        gas_limit: 30_000_000,
        basefee: 0,
        ..Default::default()
    };

    let mut evm1 = factory.create_evm(
        EmptyDB::default(),
        EvmEnv {
            cfg_env: cfg_env.clone(),
            block_env: block_env.clone(),
        },
    );
    let mut evm2 = factory.create_evm(EmptyDB::default(), EvmEnv { cfg_env, block_env });

    // Initially, both EVMs should have empty access lists
    assert_eq!(
        evm1.pd_context().read_access_list().len(),
        0,
        "EVM1 should start with empty access list"
    );
    assert_eq!(
        evm2.pd_context().read_access_list().len(),
        0,
        "EVM2 should start with empty access list"
    );

    // Execute a PD transaction on evm1 with 5 chunks
    let access_list1 = create_access_list_with_chunks(5);
    let expected_list1 = access_list1.clone();
    let tx1 = create_pd_transaction(access_list1);
    let _result1 = evm1.transact_raw(tx1);

    // After evm1 transaction, evm1 should have the access list, but evm2 should still be empty
    let evm1_list = evm1.pd_context().read_access_list();
    assert_eq!(
        evm1_list.len(),
        expected_list1.0.len(),
        "EVM1 should have populated access list after transaction"
    );
    assert_eq!(
        evm2.pd_context().read_access_list().len(),
        0,
        "EVM2 should still have empty access list (isolation verified)"
    );

    // Execute a PD transaction on evm2 with 10 chunks
    let access_list2 = create_access_list_with_chunks(10);
    let expected_list2 = access_list2.clone();
    let tx2 = create_pd_transaction(access_list2);
    let _result2 = evm2.transact_raw(tx2);

    // After evm2 transaction, both should have their own access lists (not shared)
    let evm1_final = evm1.pd_context().read_access_list();
    let evm2_final = evm2.pd_context().read_access_list();

    assert_eq!(
        evm1_final.len(),
        expected_list1.0.len(),
        "EVM1 access list should remain unchanged"
    );
    assert_eq!(
        evm2_final.len(),
        expected_list2.0.len(),
        "EVM2 should have its own access list"
    );

    // Verify they have different content (5 chunks vs 10 chunks)
    assert_ne!(
        evm1_final, evm2_final,
        "EVM1 and EVM2 should have different access lists"
    );
}
