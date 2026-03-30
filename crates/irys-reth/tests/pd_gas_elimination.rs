//! Integration tests for PD-aware RETURNDATACOPY gas elimination.
//!
//! These tests deploy a minimal raw-bytecode "proxy" contract that calls the PD
//! precompile via STATICCALL and copies the result via RETURNDATACOPY. They prove
//! that memory expansion gas is eliminated for PD return data at scale (5–10 MB).

use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_sol_types::SolCall as _;
use dashmap::DashMap;
use irys_reth::evm::{IRYS_USD_PRICE_ACCOUNT, IrysEvmFactory};
use irys_reth::precompiles::pd::functions::readDataCall;
use irys_types::precompile::PD_PRECOMPILE_ADDRESS;
use irys_types::range_specifier::{PdAccessListArgsTypeId, PdDataRead, encode_pd_fee};
use reth_evm::EvmEnv;
use revm::bytecode::Bytecode;
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::database::CacheDB;
use revm::database_interface::EmptyDB;
use revm::interpreter::num_words;
use revm::primitives::{TxKind, U256, hardfork::SpecId};
use revm::state::AccountInfo;
use std::sync::Arc;

use alloy_evm::{Evm as _, EvmFactory as _};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const TEST_CALLER: Address = Address::repeat_byte(0x42);
const PROXY_ADDR: Address = Address::repeat_byte(0x77);
const TEST_FEE_PER_CHUNK: u128 = 1_000_000_000_000_000_000; // 1 IRYS
const CHUNK_SIZE: u64 = 256_000;
const NUM_CHUNKS_IN_PARTITION: u64 = 200;

// ---------------------------------------------------------------------------
// Proxy contract bytecode
// ---------------------------------------------------------------------------

/// Minimal runtime bytecode that:
/// 1. Copies calldata to memory
/// 2. STATICCALLs the PD precompile (0x500) with that calldata
/// 3. Checks success (reverts if inner call failed)
/// 4. RETURNDATACOPYs the full return buffer to memory offset 0
/// 5. Returns the copied data via RETURN
///
/// This exercises the custom irys_returndatacopy handler because the
/// RETURNDATACOPY opcode is used to copy PD precompile return data.
const PD_PROXY_RUNTIME: &[u8] = &[
    // 1. CALLDATACOPY: memory[0..cds] = calldata
    0x36, // CALLDATASIZE                         PC=0
    0x60, 0x00, // PUSH1 0                        PC=1
    0x60, 0x00, // PUSH1 0                        PC=3
    0x37, // CALLDATACOPY                          PC=5
    // 2. STATICCALL(gas, 0x500, 0, cds, 0, 0)
    0x60, 0x00, // PUSH1 0         retSize        PC=6
    0x60, 0x00, // PUSH1 0         retOffset      PC=8
    0x36, // CALLDATASIZE           argsSize       PC=10
    0x60, 0x00, // PUSH1 0         argsOffset     PC=11
    0x61, 0x05, 0x00, // PUSH2 0x0500  address    PC=13
    0x5A, // GAS                                   PC=16
    0xFA, // STATICCALL                            PC=17
    // 2b. Check success: revert if failed
    0x15, // ISZERO                                PC=18
    0x60, 0x20, // PUSH1 32       revert_pc       PC=19
    0x57, // JUMPI                                 PC=21
    // 3. RETURNDATACOPY: memory[0..rds] = returndata
    0x3D, // RETURNDATASIZE                        PC=22
    0x60, 0x00, // PUSH1 0         srcOffset      PC=23
    0x60, 0x00, // PUSH1 0         destOffset     PC=25
    0x3E, // RETURNDATACOPY  ← opcode under test   PC=27
    // 4. RETURN memory[0..rds]
    0x3D, // RETURNDATASIZE                        PC=28
    0x60, 0x00, // PUSH1 0                        PC=29
    0xF3, // RETURN                                PC=31
    // 5. Revert path (PC=32)
    0x5B, // JUMPDEST                              PC=32
    0x3D, // RETURNDATASIZE                        PC=33
    0x60, 0x00, // PUSH1 0                        PC=34
    0x60, 0x00, // PUSH1 0                        PC=36
    0x3E, // RETURNDATACOPY                        PC=38
    0x3D, // RETURNDATASIZE                        PC=39
    0x60, 0x00, // PUSH1 0                        PC=40
    0xFD, // REVERT                                PC=42
];

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn chunk_data_index(num_chunks: u64) -> irys_types::chunk_provider::ChunkDataIndex {
    let index = Arc::new(DashMap::new());
    let chunk = Arc::new(bytes::Bytes::from(vec![0_u8; CHUNK_SIZE as usize]));
    for offset in 0..num_chunks {
        index.insert((0_u32, offset), chunk.clone());
    }
    index
}

fn test_factory(num_chunks: u64) -> IrysEvmFactory {
    use irys_types::ConsensusConfig;
    use irys_types::chunk_provider::ChunkConfig;

    let consensus = ConsensusConfig::testing();
    let hardfork_config = Arc::new(consensus.hardforks);

    let chunk_config = ChunkConfig {
        num_chunks_in_partition: NUM_CHUNKS_IN_PARTITION,
        chunk_size: CHUNK_SIZE,
        entropy_packing_iterations: 0,
        chain_id: 1,
    };

    IrysEvmFactory::new(chunk_config, hardfork_config, chunk_data_index(num_chunks))
}

fn test_db_with_proxy() -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(EmptyDB::default());

    // IRYS/USD price = $1.00
    db.insert_account_info(
        *IRYS_USD_PRICE_ACCOUNT,
        AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000_u128),
            ..Default::default()
        },
    );

    // Fund test caller
    db.insert_account_info(
        TEST_CALLER,
        AccountInfo {
            balance: U256::MAX / U256::from(2),
            ..Default::default()
        },
    );

    // Deploy proxy contract
    let bytecode = Bytecode::new_raw(Bytes::copy_from_slice(PD_PROXY_RUNTIME));
    let code_hash = keccak256(PD_PROXY_RUNTIME);
    db.insert_account_info(
        PROXY_ADDR,
        AccountInfo {
            code_hash,
            code: Some(bytecode),
            ..Default::default()
        },
    );

    db
}

fn evm_env() -> EvmEnv {
    let mut cfg_env = CfgEnv::default();
    cfg_env.chain_id = 1;
    cfg_env.spec = SpecId::CANCUN;

    let block_env = BlockEnv {
        gas_limit: 30_000_000,
        basefee: 0,
        ..Default::default()
    };

    EvmEnv { cfg_env, block_env }
}

fn build_access_list(num_chunks: u64) -> AccessList {
    let spec = PdDataRead {
        partition_index: 0,
        start: 0,
        len: num_chunks as u32 * CHUNK_SIZE as u32,
        byte_off: 0,
    };

    let fee = U256::from(TEST_FEE_PER_CHUNK);
    AccessList(vec![AccessListItem {
        address: PD_PRECOMPILE_ADDRESS,
        storage_keys: vec![
            B256::from(spec.encode()),
            B256::from(
                encode_pd_fee(PdAccessListArgsTypeId::PdPriorityFee as u8, fee)
                    .expect("fee encoding"),
            ),
            B256::from(
                encode_pd_fee(PdAccessListArgsTypeId::PdBaseFeeCap as u8, fee)
                    .expect("fee encoding"),
            ),
        ],
    }])
}

fn pd_proxy_tx(num_chunks: u64) -> TxEnv {
    let calldata = readDataCall { index: 0 }.abi_encode();
    TxEnv {
        caller: TEST_CALLER,
        kind: TxKind::Call(PROXY_ADDR),
        nonce: 0,
        gas_limit: 30_000_000,
        value: U256::ZERO,
        data: calldata.into(),
        gas_price: 0,
        chain_id: Some(1),
        gas_priority_fee: None,
        access_list: build_access_list(num_chunks),
        blob_hashes: Vec::new(),
        max_fee_per_blob_gas: 0,
        tx_type: 1,
        authorization_list: Default::default(),
    }
}

/// Execute a PD read of `num_chunks` chunks through the proxy contract.
/// Returns `(is_success, gas_used, output_len)`.
fn run_pd_read(num_chunks: u64) -> (bool, u64, usize) {
    let factory = test_factory(num_chunks);
    let mut evm = factory.create_evm(test_db_with_proxy(), evm_env());
    let result = evm.transact_raw(pd_proxy_tx(num_chunks)).unwrap();
    let success = result.result.is_success();
    let gas_used = result.result.gas_used();
    let output_len = result.result.into_output().map(|o| o.len()).unwrap_or(0);
    (success, gas_used, output_len)
}

/// Compute the EVM memory expansion cost for `num_words` words.
fn expected_memory_cost(num_words: usize) -> u64 {
    let w = num_words as u64;
    3 * w + w * w / 512
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// 5 MB PD read through a contract succeeds within gas limit.
///
/// Without gas elimination, memory expansion alone costs ~50.5M gas (exceeds
/// the 30M gas limit). With elimination, only copy gas is charged (~480K).
#[test]
fn pd_5mb_read_through_contract_succeeds() {
    let num_chunks = 20; // 20 × 256,000 = 5,120,000 bytes
    let (success, gas_used, output_len) = run_pd_read(num_chunks);

    assert!(success, "5 MB PD read through proxy should succeed");

    // Return buffer: 5,120,000 payload + 64 ABI overhead = 5,120,064 bytes
    assert!(
        output_len > 5_100_000,
        "Output should be ~5.12 MB, got {output_len}"
    );

    // Copy gas: 3 * ceil(5,120,064 / 32) = 3 * 160,002 = 480,006
    let return_bytes = num_chunks * CHUNK_SIZE + 64; // ABI overhead
    let copy_words = num_words(return_bytes as usize);
    let copy_gas = 3 * copy_words as u64;

    // Without elimination, memory expansion would be:
    let expansion_gas = expected_memory_cost(copy_words);
    assert!(
        expansion_gas > 30_000_000,
        "Sanity: expansion ({expansion_gas}) should exceed gas limit without elimination"
    );

    // With elimination, gas should be copy gas + overhead (intrinsic, STATICCALL, etc.)
    assert!(
        gas_used < copy_gas + 600_000,
        "Gas used ({gas_used}) should be copy gas ({copy_gas}) + overhead, not {expansion_gas}"
    );
}

/// 10 MB PD read through a contract also succeeds.
///
/// Without elimination, memory expansion costs ~200M gas.
#[test]
fn pd_10mb_read_through_contract_succeeds() {
    let num_chunks = 40; // 40 × 256,000 = 10,240,000 bytes
    let (success, gas_used, output_len) = run_pd_read(num_chunks);

    assert!(success, "10 MB PD read through proxy should succeed");

    assert!(
        output_len > 10_200_000,
        "Output should be ~10.24 MB, got {output_len}"
    );

    let return_bytes = num_chunks * CHUNK_SIZE + 64;
    let copy_words = num_words(return_bytes as usize);
    let copy_gas = 3 * copy_words as u64;

    assert!(
        gas_used < copy_gas + 600_000,
        "Gas used ({gas_used}) should be copy gas ({copy_gas}) + overhead"
    );
}

/// Gas scales linearly with data size, not quadratically.
///
/// If memory expansion were charged, the 5× size increase would cause a
/// ~25× gas increase (quadratic). With elimination, the ratio should be
/// close to 5× (linear copy gas).
#[test]
fn pd_gas_scales_linearly_with_size() {
    let (ok_1, gas_1mb, _) = run_pd_read(4); // 4 × 256 KB = 1.024 MB
    let (ok_5, gas_5mb, _) = run_pd_read(20); // 20 × 256 KB = 5.12 MB

    assert!(ok_1, "1 MB read should succeed");
    assert!(ok_5, "5 MB read should succeed");

    let ratio = gas_5mb as f64 / gas_1mb as f64;

    // With linear copy gas, 5 MB / 1 MB ratio should be ~5×.
    // Allow up to 7× for overhead variation. Quadratic would be ~25×.
    assert!(
        ratio < 7.0,
        "Gas ratio 5MB/1MB = {ratio:.1}× — should be ~5× (linear), not ~25× (quadratic)"
    );
}

// ===========================================================================
// Task 3: Comparative — non-PD RETURNDATACOPY still charges memory gas
// ===========================================================================

const RETURNER_ADDR: Address = Address::repeat_byte(0x88);
const NON_PD_CALLER_ADDR: Address = Address::repeat_byte(0x99);

/// Build a "returner" contract that returns `size` zero bytes.
/// Runtime: PUSH3 <size> / PUSH1 0 / RETURN (7 bytes)
fn returner_bytecode(size: u32) -> Bytes {
    let [_, b1, b2, b3] = size.to_be_bytes();
    Bytes::from(vec![
        0x62, b1, b2, b3, // PUSH3 <size>
        0x60, 0x00, // PUSH1 0 (offset)
        0xF3, // RETURN
    ])
}

/// Build a "non-PD caller" contract that CALLs the returner, then
/// RETURNDATACOPYs the result to memory offset 0 and returns it.
///
/// This contract has no CALLDATACOPY, so memory starts at 0 and the
/// RETURNDATACOPY is what causes memory expansion. This avoids the
/// IDENTITY precompile problem where CALLDATACOPY pre-expands memory.
fn non_pd_caller_bytecode(returner: Address) -> Bytes {
    let mut code = Vec::with_capacity(53);
    // CALL(gas, returner, 0, 0, 0, 0, 0)
    code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0  retSize       PC=0
    code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0  retOffset     PC=2
    code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0  argsSize      PC=4
    code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0  argsOffset    PC=6
    code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0  value         PC=8
    code.push(0x73); // PUSH20                                       PC=10
    code.extend_from_slice(returner.as_slice()); //     address       PC=11
    code.push(0x5A); // GAS                                          PC=31
    code.push(0xF1); // CALL                                         PC=32
    // Check success
    code.push(0x15); // ISZERO                                       PC=33
    code.extend_from_slice(&[0x60, 47]); // PUSH1 47  revert_pc      PC=34
    code.push(0x57); // JUMPI                                        PC=36
    // RETURNDATACOPY: memory[0..rds] = returndata
    code.push(0x3D); // RETURNDATASIZE                               PC=37
    code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0  srcOffset     PC=38
    code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0  destOffset    PC=40
    code.push(0x3E); // RETURNDATACOPY                               PC=42
    // RETURN
    code.push(0x3D); // RETURNDATASIZE                               PC=43
    code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0                PC=44
    code.push(0xF3); // RETURN                                       PC=46
    // Revert path (PC=47)
    code.push(0x5B); // JUMPDEST                                     PC=47
    code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0                PC=48
    code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0                PC=50
    code.push(0xFD); // REVERT                                       PC=52
    Bytes::from(code)
}

fn deploy_contract(db: &mut CacheDB<EmptyDB>, addr: Address, runtime: &[u8]) {
    let bytecode = Bytecode::new_raw(Bytes::copy_from_slice(runtime));
    let code_hash = keccak256(runtime);
    db.insert_account_info(
        addr,
        AccountInfo {
            code_hash,
            code: Some(bytecode),
            ..Default::default()
        },
    );
}

fn test_db_with_all_contracts(returner_size: u32) -> CacheDB<EmptyDB> {
    let mut db = test_db_with_proxy();

    // Deploy returner
    let ret_code = returner_bytecode(returner_size);
    deploy_contract(&mut db, RETURNER_ADDR, &ret_code);

    // Deploy non-PD caller
    let caller_code = non_pd_caller_bytecode(RETURNER_ADDR);
    deploy_contract(&mut db, NON_PD_CALLER_ADDR, &caller_code);

    db
}

/// Non-PD 512 KB RETURNDATACOPY charges memory expansion gas.
///
/// 512,000 bytes = 16,000 words → expansion cost = 3 * 16000 + 16000² / 512
///                                               = 48,000 + 500,000 = 548,000
#[test]
fn non_pd_512kb_returndatacopy_includes_memory_expansion_gas() {
    let return_size: u32 = 512_000;
    let factory = test_factory(0); // no PD chunks needed
    let db = test_db_with_all_contracts(return_size);
    let mut evm = factory.create_evm(db, evm_env());

    let tx = TxEnv {
        caller: TEST_CALLER,
        kind: TxKind::Call(NON_PD_CALLER_ADDR),
        nonce: 0,
        gas_limit: 30_000_000,
        value: U256::ZERO,
        data: Bytes::new(), // no calldata needed
        gas_price: 0,
        chain_id: Some(1),
        gas_priority_fee: None,
        access_list: AccessList::default(),
        blob_hashes: Vec::new(),
        max_fee_per_blob_gas: 0,
        tx_type: 1,
        authorization_list: Default::default(),
    };

    let result = evm.transact_raw(tx).unwrap();
    assert!(result.result.is_success(), "512 KB non-PD should succeed");

    let gas_used = result.result.gas_used();
    let words = num_words(return_size as usize);
    let expansion = expected_memory_cost(words);

    // Gas must include memory expansion
    assert!(
        gas_used > expansion,
        "Non-PD gas ({gas_used}) should include memory expansion ({expansion})"
    );
}

/// Same-size PD read costs dramatically less gas than non-PD.
#[test]
fn pd_vs_non_pd_512kb_gas_comparison() {
    let return_size: u32 = 512_000;

    // PD path: 2 chunks × 256,000 = 512,000 bytes
    let (pd_ok, gas_pd, _) = run_pd_read(2);
    assert!(pd_ok, "PD 512 KB read should succeed");

    // Non-PD path: same size through returner contract
    let factory = test_factory(0);
    let db = test_db_with_all_contracts(return_size);
    let mut evm = factory.create_evm(db, evm_env());
    let tx = TxEnv {
        caller: TEST_CALLER,
        kind: TxKind::Call(NON_PD_CALLER_ADDR),
        nonce: 0,
        gas_limit: 30_000_000,
        value: U256::ZERO,
        data: Bytes::new(),
        gas_price: 0,
        chain_id: Some(1),
        gas_priority_fee: None,
        access_list: AccessList::default(),
        blob_hashes: Vec::new(),
        max_fee_per_blob_gas: 0,
        tx_type: 1,
        authorization_list: Default::default(),
    };
    let result = evm.transact_raw(tx).unwrap();
    assert!(result.result.is_success(), "Non-PD 512 KB should succeed");
    let gas_non_pd = result.result.gas_used();

    // Non-PD includes ~548K memory expansion; PD does not.
    assert!(
        gas_non_pd > gas_pd * 3,
        "Non-PD ({gas_non_pd}) should be much higher than PD ({gas_pd})"
    );
}

// ===========================================================================
// Task 4: Memory high-water mark preserved after PD copy
// ===========================================================================

/// Build a PD proxy variant that optionally does an MSTORE 1 word past
/// the RETURNDATACOPY region to test incremental memory expansion.
///
/// When `extra_mstore` is true, adds: PUSH1 0x42 / PUSH3 <offset> / MSTORE
/// before the RETURN. This expands memory by 1 word beyond the PD copy.
fn pd_proxy_with_mstore(extra_mstore: bool) -> Bytes {
    let mut code = Vec::from(PD_PROXY_RUNTIME);
    if extra_mstore {
        // Insert MSTORE before the RETURN at PC=28.
        // Remove the RETURN section (bytes 28..32) and rebuild.
        code.truncate(28); // keep up to RETURNDATACOPY
        // PUSH1 0x42 (value to store)
        code.extend_from_slice(&[0x60, 0x42]);
        // PUSH3 <offset> — 1 word past 5 MB: 5,120,064 + 32 = 5,120,096 = 0x4E2060
        code.extend_from_slice(&[0x62, 0x4E, 0x20, 0x60]);
        // MSTORE
        code.push(0x52);
        // RETURNDATASIZE / PUSH1 0 / RETURN (original return sequence)
        code.extend_from_slice(&[0x3D, 0x60, 0x00, 0xF3]);
        // Revert path — update JUMPDEST PC
        let revert_pc = code.len() as u8;
        code.extend_from_slice(&[
            0x5B, // JUMPDEST
            0x3D, 0x60, 0x00, 0x60, 0x00, 0x3E, // RETURNDATACOPY
            0x3D, 0x60, 0x00, 0xFD, // REVERT
        ]);
        // Fix the PUSH1 for revert_pc (at PC=19 in the original, byte index 19)
        code[20] = revert_pc;
    }
    Bytes::from(code)
}

const PROXY_MSTORE_ADDR: Address = Address::repeat_byte(0xAA);
const PROXY_NO_MSTORE_ADDR: Address = Address::repeat_byte(0xBB);

/// After a PD RETURNDATACOPY, subsequent memory expansion charges only
/// the incremental cost (~628 gas per word at 5 MB), not the full 50M+.
#[test]
fn pd_copy_preserves_memory_high_water_mark() {
    let num_chunks: u64 = 20;
    let factory = test_factory(num_chunks);

    let mut db = test_db_with_proxy();
    let code_with = pd_proxy_with_mstore(true);
    let code_without = pd_proxy_with_mstore(false);
    deploy_contract(&mut db, PROXY_MSTORE_ADDR, &code_with);
    deploy_contract(&mut db, PROXY_NO_MSTORE_ADDR, &code_without);

    let calldata: Bytes = readDataCall { index: 0 }.abi_encode().into();
    let access_list = build_access_list(num_chunks);

    // Run without extra MSTORE
    let mut evm = factory.create_evm(db.clone(), evm_env());
    let tx_without = TxEnv {
        caller: TEST_CALLER,
        kind: TxKind::Call(PROXY_NO_MSTORE_ADDR),
        nonce: 0,
        gas_limit: 30_000_000,
        value: U256::ZERO,
        data: calldata.clone(),
        gas_price: 0,
        chain_id: Some(1),
        gas_priority_fee: None,
        access_list: access_list.clone(),
        blob_hashes: Vec::new(),
        max_fee_per_blob_gas: 0,
        tx_type: 1,
        authorization_list: Default::default(),
    };
    let res_without = evm.transact_raw(tx_without).unwrap();
    assert!(res_without.result.is_success());
    let gas_without = res_without.result.gas_used();

    // Run with extra MSTORE
    let mut evm = factory.create_evm(db, evm_env());
    let tx_with = TxEnv {
        caller: TEST_CALLER,
        kind: TxKind::Call(PROXY_MSTORE_ADDR),
        nonce: 0,
        gas_limit: 30_000_000,
        value: U256::ZERO,
        data: calldata,
        gas_price: 0,
        chain_id: Some(1),
        gas_priority_fee: None,
        access_list,
        blob_hashes: Vec::new(),
        max_fee_per_blob_gas: 0,
        tx_type: 1,
        authorization_list: Default::default(),
    };
    let res_with = evm.transact_raw(tx_with).unwrap();
    assert!(res_with.result.is_success());
    let gas_with = res_with.result.gas_used();

    let delta = gas_with.saturating_sub(gas_without);

    // Extra MSTORE 1 word past 5 MB should cost:
    // - MSTORE base (3) + incremental expansion (~628 gas at 160K words)
    // - Plus the PUSH opcodes (3+3 gas)
    // Total delta should be under 2,000, NOT millions.
    assert!(
        delta < 2_000,
        "Extra MSTORE delta ({delta}) should be ~630-1260 — high-water mark preserved"
    );
    // Also assert it's nonzero — expansion DID happen.
    // MSTORE at offset 5,120,096 expands from 160,002 to 160,004 words (2 words).
    // Incremental cost = cost(160004) - cost(160002) ≈ 1256, plus MSTORE(3) + PUSH(9) ≈ 1265.
    assert!(
        delta > 500,
        "Extra MSTORE delta ({delta}) should be > 500 — incremental expansion was charged"
    );
}

// ===========================================================================
// Task 5: Transaction boundary isolation
// ===========================================================================

/// PD return marker does not leak across transactions.
///
/// TX1 does a PD read (exempt from memory gas). TX2 does a non-PD read
/// on the same EVM instance — TX2 must still pay memory expansion.
#[test]
fn pd_marker_does_not_leak_across_transactions() {
    let num_chunks: u64 = 2; // 512 KB — small enough for non-PD to succeed
    let return_size: u32 = 512_000;

    let factory = test_factory(num_chunks);
    let mut db = test_db_with_proxy();
    let ret_code = returner_bytecode(return_size);
    deploy_contract(&mut db, RETURNER_ADDR, &ret_code);
    let caller_code = non_pd_caller_bytecode(RETURNER_ADDR);
    deploy_contract(&mut db, NON_PD_CALLER_ADDR, &caller_code);

    // TX1: PD read through proxy (on one EVM instance)
    let mut evm1 = factory.create_evm(db.clone(), evm_env());
    let tx1 = pd_proxy_tx(num_chunks);
    let res1 = evm1.transact_raw(tx1).unwrap();
    assert!(res1.result.is_success(), "TX1 PD read should succeed");
    let gas_pd = res1.result.gas_used();

    // TX2: non-PD read on a fresh EVM instance (same thread — tests
    // that PdReturnMarkerScope cleared the thread-local marker)
    let mut evm2 = factory.create_evm(db, evm_env());
    let tx2 = TxEnv {
        caller: TEST_CALLER,
        kind: TxKind::Call(NON_PD_CALLER_ADDR),
        nonce: 0,
        gas_limit: 30_000_000,
        value: U256::ZERO,
        data: Bytes::new(),
        gas_price: 0,
        chain_id: Some(1),
        gas_priority_fee: None,
        access_list: AccessList::default(),
        blob_hashes: Vec::new(),
        max_fee_per_blob_gas: 0,
        tx_type: 1,
        authorization_list: Default::default(),
    };
    let res2 = evm2.transact_raw(tx2).unwrap();
    assert!(res2.result.is_success(), "TX2 non-PD read should succeed");
    let gas_non_pd = res2.result.gas_used();

    let expansion = expected_memory_cost(num_words(return_size as usize));

    // TX2 must include memory expansion — marker was cleared between txs.
    assert!(
        gas_non_pd > expansion,
        "TX2 gas ({gas_non_pd}) should include memory expansion ({expansion}) — marker was cleared"
    );

    // TX1 should be much cheaper (no expansion).
    assert!(
        gas_pd < gas_non_pd,
        "TX1 PD ({gas_pd}) should be cheaper than TX2 non-PD ({gas_non_pd})"
    );
}

// ===========================================================================
// Task 6: Frame-scope and edge case tests
// ===========================================================================

const WRAPPER_ADDR: Address = Address::repeat_byte(0xCC);

/// Build a "wrapper" contract that CALLs the PD proxy and then does
/// RETURNDATACOPY on the proxy's return data (not PD's).
///
/// The wrapper sees the PD proxy's return allocation — not the PD
/// precompile's original allocation — so the marker should NOT match.
fn nested_caller_bytecode(inner_addr: Address) -> Bytes {
    let calldata = readDataCall { index: 0 }.abi_encode();
    let cds = calldata.len();
    let mut code = Vec::with_capacity(100);

    // Store the calldata in memory so we can forward it to inner contract
    for (i, chunk) in calldata.chunks(32).enumerate() {
        let offset = i * 32;
        // PUSH32 <chunk> — but we need to pad to 32 bytes
        let mut word = [0_u8; 32];
        word[..chunk.len()].copy_from_slice(chunk);
        code.push(0x7F); // PUSH32
        code.extend_from_slice(&word);
        code.push(0x60); // PUSH1 offset
        code.push(offset as u8);
        code.push(0x52); // MSTORE
    }

    // CALL(gas, inner_addr, 0, 0, cds, 0, 0)
    code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0  retSize
    code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0  retOffset
    code.push(0x60); // PUSH1 cds  argsSize
    code.push(cds as u8);
    code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0  argsOffset
    code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0  value
    code.push(0x73); // PUSH20 inner_addr
    code.extend_from_slice(inner_addr.as_slice());
    code.push(0x5A); // GAS
    code.push(0xF1); // CALL

    // Check success
    code.push(0x15); // ISZERO
    let jumpi_pc_idx = code.len();
    code.extend_from_slice(&[0x60, 0x00]); // placeholder for revert_pc
    code.push(0x57); // JUMPI

    // RETURNDATACOPY at fresh offset (0x100000 = 1 MB, well beyond calldata area)
    code.push(0x3D); // RETURNDATASIZE
    code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0 srcOffset
    code.push(0x62); // PUSH3 destOffset
    code.extend_from_slice(&[0x10, 0x00, 0x00]); // 0x100000
    code.push(0x3E); // RETURNDATACOPY

    // RETURN
    code.push(0x3D); // RETURNDATASIZE
    code.push(0x62); // PUSH3 offset
    code.extend_from_slice(&[0x10, 0x00, 0x00]);
    code.push(0xF3); // RETURN

    // Revert path
    let revert_pc = code.len() as u8;
    code.push(0x5B); // JUMPDEST
    code.extend_from_slice(&[0x60, 0x00, 0x60, 0x00, 0xFD]); // PUSH1 0, PUSH1 0, REVERT

    // Patch revert_pc
    code[jumpi_pc_idx + 1] = revert_pc;

    Bytes::from(code)
}

/// Nested call does NOT inherit PD exemption.
///
/// Wrapper → PD proxy → PD precompile. The wrapper's RETURNDATACOPY sees
/// the proxy's RETURN data (a new allocation), not the PD precompile's
/// original allocation. Memory expansion must be charged.
#[test]
fn nested_call_does_not_inherit_pd_exemption() {
    let num_chunks: u64 = 2; // 512 KB — small enough that non-exempt gas fits in limit
    let factory = test_factory(num_chunks);
    let mut db = test_db_with_proxy();

    let wrapper_code = nested_caller_bytecode(PROXY_ADDR);
    deploy_contract(&mut db, WRAPPER_ADDR, &wrapper_code);

    let mut evm = factory.create_evm(db, evm_env());

    let tx = TxEnv {
        caller: TEST_CALLER,
        kind: TxKind::Call(WRAPPER_ADDR),
        nonce: 0,
        gas_limit: 30_000_000,
        value: U256::ZERO,
        data: Bytes::new(),
        gas_price: 0,
        chain_id: Some(1),
        gas_priority_fee: None,
        access_list: build_access_list(num_chunks),
        blob_hashes: Vec::new(),
        max_fee_per_blob_gas: 0,
        tx_type: 1,
        authorization_list: Default::default(),
    };

    let result = evm.transact_raw(tx).unwrap();
    assert!(result.result.is_success(), "Nested call should succeed");

    let gas_used = result.result.gas_used();

    // The wrapper's RETURNDATACOPY copies at dest_offset=0x100000 (1 MB).
    // Return data is ~512 KB. Total memory touches ~1.5 MB.
    // Memory expansion for 1.5 MB ≈ 4.7M gas.
    // If exemption leaked, gas would be < 500K (only copy gas).
    let return_size = num_chunks * CHUNK_SIZE + 64;
    let dest_offset = 0x10_0000_usize; // 1 MB
    let total_mem_words = num_words(dest_offset + return_size as usize);
    let expansion = expected_memory_cost(total_mem_words);

    assert!(
        gas_used > expansion / 2,
        "Nested call gas ({gas_used}) should include memory expansion (~{expansion}) — exemption is frame-local"
    );
}

/// Two PD reads in separate transactions both get the exemption.
///
/// Uses two separate EVM instances (same thread) through the proxy,
/// verifying each independently gets low gas. This tests that
/// `PdReturnMarkerScope` correctly clears the marker between transactions
/// without interfering with subsequent PD calls.
#[test]
fn two_pd_reads_separate_txs_both_exempt() {
    let num_chunks: u64 = 4; // 1 MB per read
    let factory = test_factory(num_chunks);
    let db = test_db_with_proxy();

    // Read 1
    let mut evm1 = factory.create_evm(db.clone(), evm_env());
    let res1 = evm1.transact_raw(pd_proxy_tx(num_chunks)).unwrap();
    assert!(res1.result.is_success(), "PD read 1 should succeed");

    // Read 2 — separate EVM, same thread (marker cleared by PdReturnMarkerScope)
    let mut evm2 = factory.create_evm(db, evm_env());
    let res2 = evm2.transact_raw(pd_proxy_tx(num_chunks)).unwrap();
    assert!(res2.result.is_success(), "PD read 2 should succeed");

    let return_bytes = num_chunks * CHUNK_SIZE + 64;
    let copy_gas = 3 * num_words(return_bytes as usize) as u64;

    // Both should have low gas (copy + overhead only, no expansion)
    assert!(
        res1.result.gas_used() < copy_gas + 600_000,
        "Read 1 gas ({}) should be low",
        res1.result.gas_used()
    );
    assert!(
        res2.result.gas_used() < copy_gas + 600_000,
        "Read 2 gas ({}) should be low",
        res2.result.gas_used()
    );
}

// ===========================================================================
// Task 6c: Two PD STATICCALLs in one transaction both get exemption
// ===========================================================================

const DUAL_PROXY_ADDR: Address = Address::repeat_byte(0xDD);

/// Runtime bytecode that STATICCALLs PD(0x500) twice and does
/// RETURNDATACOPY after each. Both copies should be PD-exempt.
///
/// Flow:
/// 1. CALLDATACOPY calldata to memory[0]
/// 2. STATICCALL PD → RETURNDATACOPY to memory[0..rds]
/// 3. Restore calldata (CALLDATACOPY to memory[0] again)
/// 4. STATICCALL PD → RETURNDATACOPY to memory[rds..2*rds]
/// 5. RETURN 32 bytes
///
/// The second RETURNDATACOPY uses RETURNDATASIZE as the dest offset,
/// placing data immediately after the first copy region.
const DUAL_PD_PROXY_RUNTIME: &[u8] = &[
    // 1. CALLDATACOPY: memory[0..cds] = calldata
    0x36, // CALLDATASIZE              PC=0
    0x60, 0x00, // PUSH1 0    srcOffset      PC=1
    0x60, 0x00, // PUSH1 0    destOffset     PC=3
    0x37, // CALLDATACOPY              PC=5
    // 2. STATICCALL 1: PD(0x500)(memory[0..cds])
    0x60, 0x00, // PUSH1 0    retSize        PC=6
    0x60, 0x00, // PUSH1 0    retOffset      PC=8
    0x36, // CALLDATASIZE argsSize     PC=10
    0x60, 0x00, // PUSH1 0    argsOffset     PC=11
    0x61, 0x05, 0x00, // PUSH2 0x0500 address      PC=13
    0x5A, // GAS                       PC=16
    0xFA, // STATICCALL                PC=17
    // 2b. Check success: revert if failed
    0x15, // ISZERO                    PC=18
    0x60, 60,   // PUSH1 60   revert_pc     PC=19
    0x57, // JUMPI                     PC=21
    // 3. RETURNDATACOPY 1: memory[0..rds] = returndata
    0x3D, // RETURNDATASIZE            PC=22
    0x60, 0x00, // PUSH1 0    srcOffset      PC=23
    0x60, 0x00, // PUSH1 0    destOffset     PC=25
    0x3E, // RETURNDATACOPY            PC=27
    // 4. Restore calldata: memory[0..cds] = calldata
    0x36, // CALLDATASIZE              PC=28
    0x60, 0x00, // PUSH1 0    srcOffset      PC=29
    0x60, 0x00, // PUSH1 0    destOffset     PC=31
    0x37, // CALLDATACOPY              PC=33
    // 5. STATICCALL 2: PD(0x500)(memory[0..cds])
    0x60, 0x00, // PUSH1 0    retSize        PC=34
    0x60, 0x00, // PUSH1 0    retOffset      PC=36
    0x36, // CALLDATASIZE argsSize     PC=38
    0x60, 0x00, // PUSH1 0    argsOffset     PC=39
    0x61, 0x05, 0x00, // PUSH2 0x0500 address      PC=41
    0x5A, // GAS                       PC=44
    0xFA, // STATICCALL                PC=45
    // 5b. Check success: revert if failed
    0x15, // ISZERO                    PC=46
    0x60, 60,   // PUSH1 60   revert_pc     PC=47
    0x57, // JUMPI                     PC=49
    // 6. RETURNDATACOPY 2: memory[rds..2*rds] = returndata
    0x3D, // RETURNDATASIZE  (len)     PC=50
    0x60, 0x00, // PUSH1 0    srcOffset      PC=51
    0x3D, // RETURNDATASIZE  (destOff) PC=53
    0x3E, // RETURNDATACOPY            PC=54
    // 7. RETURN 32 bytes from offset 0
    0x60, 0x20, // PUSH1 32                  PC=55
    0x60, 0x00, // PUSH1 0                   PC=57
    0xF3, // RETURN                    PC=59
    // 8. Revert path
    0x5B, // JUMPDEST                  PC=60
    0x60, 0x00, // PUSH1 0                   PC=61
    0x60, 0x00, // PUSH1 0                   PC=63
    0xFD, // REVERT                    PC=65
];

/// Two PD STATICCALLs in a single transaction both get gas exemption.
///
/// A single contract calls PD(0x500) twice with RETURNDATACOPY after each.
/// Both copies should skip memory expansion gas. This proves that the
/// marker is correctly updated per-STATICCALL within a single transaction.
#[test]
fn two_pd_staticcalls_in_one_tx_both_exempt() {
    let num_chunks: u64 = 4; // 4 × 256,000 = 1,024,000 bytes per read
    let factory = test_factory(num_chunks);
    let mut db = test_db_with_proxy();

    deploy_contract(&mut db, DUAL_PROXY_ADDR, DUAL_PD_PROXY_RUNTIME);

    let calldata: Bytes = readDataCall { index: 0 }.abi_encode().into();
    let access_list = build_access_list(num_chunks);

    let mut evm = factory.create_evm(db, evm_env());
    let tx = TxEnv {
        caller: TEST_CALLER,
        kind: TxKind::Call(DUAL_PROXY_ADDR),
        nonce: 0,
        gas_limit: 30_000_000,
        value: U256::ZERO,
        data: calldata,
        gas_price: 0,
        chain_id: Some(1),
        gas_priority_fee: None,
        access_list,
        blob_hashes: Vec::new(),
        max_fee_per_blob_gas: 0,
        tx_type: 1,
        authorization_list: Default::default(),
    };

    let result = evm.transact_raw(tx).unwrap();
    assert!(
        result.result.is_success(),
        "Dual PD STATICCALL should succeed"
    );

    let gas_used = result.result.gas_used();

    // Each PD read returns: 4 × 256,000 + 64 ABI overhead = 1,024,064 bytes
    let return_bytes = (num_chunks * CHUNK_SIZE + 64) as usize;
    let copy_gas_per_call = 3 * num_words(return_bytes) as u64;
    let total_copy_gas = 2 * copy_gas_per_call;

    // Without exemption, memory expansion for ~2 MB would cost ~8M gas.
    let total_mem_words = num_words(2 * return_bytes);
    let expansion_without_exemption = expected_memory_cost(total_mem_words);

    assert!(
        expansion_without_exemption > 5_000_000,
        "Sanity: expansion without exemption ({expansion_without_exemption}) should exceed 5M"
    );

    // With exemption, gas should be copy gas + overhead only.
    assert!(
        gas_used < total_copy_gas + 600_000,
        "Dual PD gas ({gas_used}) should be ~copy ({total_copy_gas}) + overhead, not include expansion ({expansion_without_exemption})"
    );
}
