//! Wire type JSON parity tests.
//!
//! Verifies that every sovereign wire type produces identical JSON output
//! to the corresponding canonical irys_types type. If a wire type drifts
//! from the canonical format, these tests catch it.

use irys_types::{
    block::{IrysBlockHeader, IrysBlockHeaderV1, PoaData, VDFLimiterInfo},
    chunk::UnpackedChunk,
    commitment_common::{
        CommitmentTransaction, CommitmentV1WithMetadata, CommitmentV2WithMetadata,
    },
    commitment_v1::{CommitmentTransactionV1, CommitmentTypeV1},
    commitment_v2::{CommitmentTransactionV2, CommitmentTypeV2},
    ingress::{IngressProof, IngressProofV1},
    serialization::H256List,
    storage_pricing::Amount,
    transaction::{
        DataTransactionHeader, DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata,
        DataTransactionMetadata,
    },
    CommitmentTransactionMetadata, DataTransactionLedger, SystemTransactionLedger, TxChunkOffset,
    U256,
};
use reth::revm::primitives::B256;

use super::test_helpers::*;
use crate::wire_types as wire;

// =============================================================================
// Helper: assert canonical type and wire type produce identical JSON
// =============================================================================

fn assert_json_parity<C: serde::Serialize, W: serde::Serialize>(canonical: &C, wire: &W) {
    let canonical_json = serde_json::to_string(canonical).expect("canonical serialization failed");
    let wire_json = serde_json::to_string(wire).expect("wire serialization failed");
    assert_eq!(
        canonical_json, wire_json,
        "Wire type JSON differs from canonical type JSON.\nCanonical: {}\nWire:      {}",
        canonical_json, wire_json
    );
}

// =============================================================================
// UnpackedChunk parity
// =============================================================================

#[test]
fn test_unpacked_chunk_parity() {
    let canonical = UnpackedChunk {
        data_root: test_h256(0xAA),
        data_size: 262144,
        data_path: test_base64(&[0xDE, 0xAD, 0xBE, 0xEF]),
        bytes: test_base64(&[0xCA, 0xFE, 0xBA, 0xBE]),
        tx_offset: TxChunkOffset(0),
    };
    let wire_type: wire::UnpackedChunk = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    // Round-trip: deserialize wire JSON back, convert to canonical
    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::UnpackedChunk = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::UnpackedChunk = deserialized.into();
    assert_eq!(canonical, roundtrip);
}

// =============================================================================
// IngressProof parity
// =============================================================================

#[test]
fn test_ingress_proof_parity() {
    let canonical = IngressProof::V1(IngressProofV1 {
        signature: test_signature(),
        data_root: test_h256(0xBB),
        proof: test_h256(0xCC),
        chain_id: 1270,
        anchor: test_h256(0xDD),
    });
    let wire_type: wire::IngressProof = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::IngressProof = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: IngressProof = deserialized.try_into().unwrap();
    assert_eq!(canonical, roundtrip);
}

// =============================================================================
// CommitmentTransaction parity
// =============================================================================

#[test]
fn test_commitment_v1_stake_parity() {
    let canonical = CommitmentTransaction::V1(CommitmentV1WithMetadata {
        tx: CommitmentTransactionV1 {
            id: test_h256(0x10),
            anchor: test_h256(0x11),
            signer: test_address(0x12),
            commitment_type: CommitmentTypeV1::Stake,
            chain_id: 1270,
            fee: 5000,
            value: U256::from(1_000_000_u64),
            signature: test_signature(),
        },
        metadata: CommitmentTransactionMetadata::new(),
    });
    let wire_type: wire::CommitmentTransaction = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::CommitmentTransaction = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: CommitmentTransaction = deserialized.try_into().unwrap();
    // Compare fields since metadata is skipped in serde
    assert_eq!(canonical.id(), roundtrip.id());
    assert_eq!(canonical.signer(), roundtrip.signer());
    assert_eq!(canonical.fee(), roundtrip.fee());
    assert_eq!(canonical.value(), roundtrip.value());
}

#[test]
fn test_commitment_v1_pledge_parity() {
    let canonical = CommitmentTransaction::V1(CommitmentV1WithMetadata {
        tx: CommitmentTransactionV1 {
            id: test_h256(0x20),
            anchor: test_h256(0x21),
            signer: test_address(0x22),
            commitment_type: CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 5,
            },
            chain_id: 1270,
            fee: 3000,
            value: U256::from(500_000_u64),
            signature: test_signature(),
        },
        metadata: CommitmentTransactionMetadata::new(),
    });
    let wire_type: wire::CommitmentTransaction = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);
}

#[test]
fn test_commitment_v1_unpledge_parity() {
    let canonical = CommitmentTransaction::V1(CommitmentV1WithMetadata {
        tx: CommitmentTransactionV1 {
            id: test_h256(0x30),
            anchor: test_h256(0x31),
            signer: test_address(0x32),
            commitment_type: CommitmentTypeV1::Unpledge {
                pledge_count_before_executing: 3,
                partition_hash: test_h256(0x33),
            },
            chain_id: 1270,
            fee: 2000,
            value: U256::from(250_000_u64),
            signature: test_signature(),
        },
        metadata: CommitmentTransactionMetadata::new(),
    });
    let wire_type: wire::CommitmentTransaction = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);
}

#[test]
fn test_commitment_v2_stake_parity() {
    let canonical = CommitmentTransaction::V2(CommitmentV2WithMetadata {
        tx: CommitmentTransactionV2 {
            id: test_h256(0x50),
            anchor: test_h256(0x51),
            signer: test_address(0x52),
            commitment_type: CommitmentTypeV2::Stake,
            chain_id: 1270,
            fee: 5000,
            value: U256::from(2_000_000_u64),
            signature: test_signature(),
        },
        metadata: CommitmentTransactionMetadata::new(),
    });
    let wire_type: wire::CommitmentTransaction = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);
}

#[test]
fn test_commitment_v2_update_reward_address_parity() {
    let canonical = CommitmentTransaction::V2(CommitmentV2WithMetadata {
        tx: CommitmentTransactionV2 {
            id: test_h256(0x90),
            anchor: test_h256(0x91),
            signer: test_address(0x92),
            commitment_type: CommitmentTypeV2::UpdateRewardAddress {
                new_reward_address: test_address(0x93),
            },
            chain_id: 1270,
            fee: 1500,
            value: U256::from(0_u64),
            signature: test_signature(),
        },
        metadata: CommitmentTransactionMetadata::new(),
    });
    let wire_type: wire::CommitmentTransaction = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);
}

// =============================================================================
// DataTransactionHeader parity
// =============================================================================

#[test]
fn test_data_transaction_header_parity() {
    let canonical = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
        tx: DataTransactionHeaderV1 {
            id: test_h256(0x01),
            anchor: test_h256(0x02),
            signer: test_address(0x03),
            data_root: test_h256(0x04),
            data_size: 1048576,
            header_size: 256,
            term_fee: 1_000_000_u64.into(),
            perm_fee: Some(500_000_u64.into()),
            ledger_id: 1,
            bundle_format: Some(1),
            chain_id: 1270,
            signature: test_signature(),
        },
        metadata: DataTransactionMetadata::new(),
    });
    let wire_type: wire::DataTransactionHeader = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::DataTransactionHeader = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: DataTransactionHeader = deserialized.try_into().unwrap();
    assert_eq!(canonical.id, roundtrip.id);
    assert_eq!(canonical.data_size, roundtrip.data_size);
}

// =============================================================================
// IrysBlockHeader parity
// =============================================================================

#[test]
fn test_block_header_parity() {
    let canonical = IrysBlockHeader::V1(IrysBlockHeaderV1 {
        block_hash: test_h256(0xF0),
        signature: test_signature(),
        height: 42,
        diff: U256::from(1000_u64),
        cumulative_diff: U256::from(50000_u64),
        solution_hash: test_h256(0xF1),
        last_diff_timestamp: test_unix_timestamp(),
        previous_solution_hash: test_h256(0xF2),
        last_epoch_hash: test_h256(0xF3),
        chunk_hash: test_h256(0xF4),
        previous_block_hash: test_h256(0xF5),
        previous_cumulative_diff: U256::from(49000_u64),
        poa: PoaData {
            partition_chunk_offset: 100,
            partition_hash: test_h256(0xE0),
            chunk: Some(test_base64(&[0x01, 0x02, 0x03])),
            ledger_id: Some(1),
            tx_path: Some(test_base64(&[0x04, 0x05])),
            data_path: Some(test_base64(&[0x06, 0x07])),
        },
        reward_address: test_address(0xA1),
        reward_amount: U256::from(100_u64),
        miner_address: test_address(0xA2),
        timestamp: test_unix_timestamp(),
        system_ledgers: vec![SystemTransactionLedger {
            ledger_id: 0,
            tx_ids: H256List(vec![test_h256(0xC1)]),
        }],
        data_ledgers: vec![DataTransactionLedger {
            ledger_id: 1,
            tx_root: test_h256(0xD0),
            tx_ids: H256List(vec![test_h256(0xD1)]),
            total_chunks: 256,
            expires: Some(100),
            proofs: None,
            required_proof_count: None,
        }],
        evm_block_hash: B256::from([0xEE; 32]),
        vdf_limiter_info: VDFLimiterInfo {
            output: test_h256(0xB0),
            global_step_number: 1000,
            seed: test_h256(0xB1),
            next_seed: test_h256(0xB2),
            prev_output: test_h256(0xB3),
            last_step_checkpoints: H256List(vec![test_h256(0xB4)]),
            steps: H256List(vec![test_h256(0xB5)]),
            vdf_difficulty: Some(200000),
            next_vdf_difficulty: Some(210000),
        },
        oracle_irys_price: Amount::new(U256::from(1_000_000_u64)),
        ema_irys_price: Amount::new(U256::from(950_000_u64)),
        treasury: U256::from(10_000_000_u64),
    });

    let wire_type: wire::IrysBlockHeader = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::IrysBlockHeader = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: IrysBlockHeader = deserialized.try_into().unwrap();
    assert_eq!(canonical.block_hash, roundtrip.block_hash);
    assert_eq!(canonical.height, roundtrip.height);
    assert_eq!(canonical.diff, roundtrip.diff);
}
