//! Integration tests for PdContext isolation.
//!
//! These tests verify that each EVM instance has its own isolated PdContext
//! (no cross-EVM contamination of access lists).

use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_evm::Evm as _;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_sol_types::SolCall as _;
use dashmap::DashMap;
use irys_reth::evm::IrysEvmFactory;
use irys_types::precompile::PD_PRECOMPILE_ADDRESS;
use irys_types::range_specifier::{PdAccessListArgsTypeId, PdDataRead, encode_pd_fee};
use reth_evm::EvmEnv;
use reth_evm::EvmFactory as _;
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::database::EmptyDB;
use revm::primitives::hardfork::SpecId;
use std::sync::Arc;

// Re-use the sol! generated types for ABI-encoded calldata
alloy_sol_types::sol! {
    function readData(uint8 index) external view returns (bytes memory data);
}

fn create_access_list_with_chunks(num_chunks: u64) -> AccessList {
    // chunk_size=32 in ConsensusConfig::testing()
    let spec = PdDataRead {
        partition_index: 0,
        start: 0,
        len: (num_chunks * 32) as u32,
        byte_off: 0,
    };

    AccessList(vec![AccessListItem {
        address: PD_PRECOMPILE_ADDRESS,
        storage_keys: vec![
            B256::from(spec.encode()),
            B256::from(
                encode_pd_fee(
                    PdAccessListArgsTypeId::PdPriorityFee as u8,
                    U256::from(1_u64),
                )
                .expect("test fee encoding"),
            ),
            B256::from(
                encode_pd_fee(
                    PdAccessListArgsTypeId::PdBaseFeeCap as u8,
                    U256::from(1_u64),
                )
                .expect("test fee encoding"),
            ),
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
        data: Bytes::from(readDataCall { index: 0 }.abi_encode()),
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
    // Create a ChunkDataIndex pre-populated with enough chunks for both EVMs.
    // chunk_size=32 in ConsensusConfig::testing(), with num_chunks_in_partition=10.
    let chunk_data_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
    let chunk = Arc::new(bytes::Bytes::from(vec![0_u8; 32]));
    for offset in 0..10_u64 {
        chunk_data_index.insert((0_u32, offset), chunk.clone());
    }
    let consensus = irys_types::config::ConsensusConfig::testing();
    let chunk_config = irys_types::chunk_provider::ChunkConfig::from_consensus(&consensus);
    let hardfork_config = Arc::new(consensus.hardforks);
    let factory = IrysEvmFactory::new(chunk_config, hardfork_config, chunk_data_index);

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
