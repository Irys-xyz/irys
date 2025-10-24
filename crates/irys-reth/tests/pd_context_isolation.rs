//! Integration tests for PdContext isolation and concurrency safety.
//!
//! These tests verify that:
//! 1. Each EVM instance has its own isolated PdContext (no cross-EVM contamination)
//! 2. Within a single EVM, the precompile and transact_raw share the same access list
//! 3. Concurrent EVM execution is safe and deterministic

use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_evm::Evm;
use alloy_primitives::{aliases::U200, Address, Bytes, TxKind, B256, U256};
use irys_primitives::chunk_provider::MockChunkProvider;
use irys_primitives::precompile::PD_PRECOMPILE_ADDRESS;
use irys_primitives::range_specifier::{
    ByteRangeSpecifier, ChunkRangeSpecifier, PdAccessListArg, U18, U34,
};
use irys_reth::evm::{IrysEvm, IrysEvmFactory};
use reth_evm::{precompiles::PrecompilesMap, EvmEnv, EvmFactory};
use revm::context::result::ResultAndState;
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::database::EmptyDB;
use revm::inspector::NoOpInspector;
use revm::primitives::hardfork::SpecId;
use std::sync::Arc;

const DEFAULT_GAS_LIMIT: u64 = 30_000_000;
const DEFAULT_CHAIN_ID: u64 = 1;
const TEST_SPEC: SpecId = SpecId::CANCUN;
const DEFAULT_BASEFEE: u64 = 0;
const DEFAULT_BYTE_RANGE_LENGTH: u32 = 100;

fn create_test_evm() -> IrysEvm<EmptyDB, NoOpInspector, PrecompilesMap> {
    let mock_chunk_provider = Arc::new(MockChunkProvider::new());
    let factory = IrysEvmFactory::new(mock_chunk_provider);

    let mut cfg_env = CfgEnv::default();
    cfg_env.spec = TEST_SPEC;
    cfg_env.chain_id = DEFAULT_CHAIN_ID;

    let block_env = BlockEnv {
        gas_limit: DEFAULT_GAS_LIMIT,
        basefee: DEFAULT_BASEFEE,
        ..Default::default()
    };

    factory.create_evm(EmptyDB::default(), EvmEnv { cfg_env, block_env })
}

fn create_pd_transaction(access_list: AccessList, function_id: u8, index: u8) -> TxEnv {
    TxEnv {
        caller: Address::random(),
        kind: TxKind::Call(PD_PRECOMPILE_ADDRESS),
        nonce: 0,
        gas_limit: DEFAULT_GAS_LIMIT,
        value: U256::ZERO,
        data: Bytes::from(vec![function_id, index]),
        gas_price: 0,
        chain_id: Some(DEFAULT_CHAIN_ID),
        gas_priority_fee: None,
        access_list,
        blob_hashes: Vec::new(),
        max_fee_per_blob_gas: 0,
        tx_type: 1,
        authorization_list: Default::default(),
    }
}

>>>>>>> 5d680a16 (fix(reth): added tests to confirm evm context/access list isolation)
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
<<<<<<< HEAD
        length: U34::try_from(100).unwrap(),
=======
        length: U34::try_from(DEFAULT_BYTE_RANGE_LENGTH).unwrap(),
>>>>>>> 5d680a16 (fix(reth): added tests to confirm evm context/access list isolation)
    };

    AccessList(vec![AccessListItem {
        address: PD_PRECOMPILE_ADDRESS,
        storage_keys: vec![
            B256::from(PdAccessListArg::ChunkRead(chunk_range).encode()),
            B256::from(PdAccessListArg::ByteRead(byte_range).encode()),
        ],
    }])
}

<<<<<<< HEAD
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
=======
fn execute_pd_transaction(
    evm: &mut IrysEvm<EmptyDB, NoOpInspector, PrecompilesMap>,
    chunks: u64,
) -> ResultAndState {
    let access_list = create_access_list_with_chunks(chunks);
    let tx = create_pd_transaction(access_list, 0, 0);
    evm.transact_raw(tx).expect("Transaction should succeed")
}

#[test]
fn test_multiple_evms_have_isolated_access_lists() {
    let mut evm1 = create_test_evm();
    let mut evm2 = create_test_evm();

    let result1 = execute_pd_transaction(&mut evm1, 5);
    let result2 = execute_pd_transaction(&mut evm2, 10);

    assert!(result1.result.is_success());
    assert!(result2.result.is_success());
    assert_ne!(
        result1.result.gas_used(),
        result2.result.gas_used(),
        "EVMs with different chunk counts should use different gas amounts"
    );
}

#[test]
fn test_sequential_evms_dont_leak_state() {
    let mut evm1 = create_test_evm();
    let gas1 = execute_pd_transaction(&mut evm1, 5).result.gas_used();
    drop(evm1);

    let mut evm2 = create_test_evm();
    let gas2 = execute_pd_transaction(&mut evm2, 10).result.gas_used();

    let mut evm3 = create_test_evm();
    let gas3 = execute_pd_transaction(&mut evm3, 5).result.gas_used();

    assert_eq!(
        gas1, gas3,
        "EVMs with same chunk count should use same gas (no state leakage)"
    );
    assert_ne!(
        gas1, gas2,
        "EVM with 10 chunks should use different gas than EVM with 5 chunks"
    );
}

#[test]
fn test_precompile_reads_transaction_access_list() {
    let mut evm = create_test_evm();
    let result = execute_pd_transaction(&mut evm, 7);

    assert!(result.result.is_success());

    match result.result.output() {
        Some(output) => assert!(!output.is_empty(), "Precompile should return chunk data"),
        None => panic!("Expected output from precompile"),
>>>>>>> 5d680a16 (fix(reth): added tests to confirm evm context/access list isolation)
    }
}

#[test]
<<<<<<< HEAD
fn test_evm_instances_have_isolated_pd_contexts() {
    // Create a single factory
    let mock_chunk_provider = Arc::new(MockChunkProvider::new());
    let factory = IrysEvmFactory::new(mock_chunk_provider);

    // Create two EVMs from the same factory
    let mut cfg_env = CfgEnv::default();
    cfg_env.spec = SpecId::CANCUN;
    cfg_env.chain_id = 1;
    let block_env = BlockEnv {
        gas_limit: 30_000_000,
        basefee: 0,
=======
fn test_empty_access_list_isolation() {
    let mut evm1 = create_test_evm();
    let _result1 = execute_pd_transaction(&mut evm1, 5);

    let mut evm2 = create_test_evm();
    let empty_access_list = AccessList(vec![]);
    let tx2 = create_pd_transaction(empty_access_list, 0, 0);
    let result2 = evm2.transact_raw(tx2);

    match result2 {
        Ok(res) => assert!(
            !res.result.is_success(),
            "Transaction with empty access list should fail"
        ),
        Err(_) => {}
    }
}

#[test]
fn test_concurrent_evm_execution() {
    use std::thread;

    const NUM_THREADS: usize = 10;
    const TRANSACTIONS_PER_THREAD: usize = 5;

    let handles: Vec<_> = (0..NUM_THREADS)
        .map(|thread_id| {
            thread::spawn(move || {
                let mut results = Vec::new();
                for tx_id in 0..TRANSACTIONS_PER_THREAD {
                    let mut evm = create_test_evm();
                    let chunk_count = (thread_id * TRANSACTIONS_PER_THREAD + tx_id + 1) as u64;
                    let result = execute_pd_transaction(&mut evm, chunk_count);

                    assert!(
                        result.result.is_success(),
                        "Thread {} tx {} should succeed",
                        thread_id,
                        tx_id
                    );
                    results.push((chunk_count, result.result.gas_used()));
                }
                results
            })
        })
        .collect();

    let all_results: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().expect("Thread should not panic"))
        .flatten()
        .collect();

    let mut gas_by_chunks = std::collections::HashMap::new();
    for (chunks, gas) in all_results {
        gas_by_chunks
            .entry(chunks)
            .or_insert_with(Vec::new)
            .push(gas);
    }

    for (chunks, gas_values) in gas_by_chunks {
        let first_gas = gas_values[0];
        for gas in &gas_values {
            assert_eq!(
                *gas, first_gas,
                "All transactions with {} chunks should use same gas (no race condition)",
                chunks
            );
        }
    }
}

#[test]
fn test_clone_vs_clone_for_new_evm_semantics() {
    let mock_chunk_provider = Arc::new(MockChunkProvider::new());
    let factory = IrysEvmFactory::new(mock_chunk_provider);

    let mut cfg_env = CfgEnv::default();
    cfg_env.spec = TEST_SPEC;
    cfg_env.chain_id = DEFAULT_CHAIN_ID;

    let block_env = BlockEnv {
        gas_limit: DEFAULT_GAS_LIMIT,
        basefee: DEFAULT_BASEFEE,
>>>>>>> 5d680a16 (fix(reth): added tests to confirm evm context/access list isolation)
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

<<<<<<< HEAD
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
=======
    let result1 = execute_pd_transaction(&mut evm1, 3);
    let result2 = execute_pd_transaction(&mut evm2, 8);

    assert!(result1.result.is_success());
    assert!(result2.result.is_success());
    assert_ne!(result1.result.gas_used(), result2.result.gas_used());
}

#[test]
fn test_access_list_update_within_single_evm() {
    let mut evm = create_test_evm();

    let gas1 = execute_pd_transaction(&mut evm, 5).result.gas_used();
    let gas2 = execute_pd_transaction(&mut evm, 10).result.gas_used();

    assert!(
        gas2 > gas1,
        "Transaction with 10 chunks should use more gas than transaction with 5 chunks"
    );
}

#[test]
fn test_no_cross_evm_contamination_stress() {
    for iteration in 0..100 {
        let mut evm = create_test_evm();
        let chunk_count = (iteration % 20) + 1;
        let result = execute_pd_transaction(&mut evm, chunk_count as u64);

        assert!(
            result.result.is_success(),
            "Iteration {} should succeed",
            iteration
        );
    }
}
