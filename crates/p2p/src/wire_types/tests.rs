//! Wire type JSON parity tests.
//!
//! Verifies that every sovereign wire type produces identical JSON output
//! to the corresponding canonical irys_types type. If a wire type drifts
//! from the canonical format, these tests catch it.

use irys_types::{
    commitment_common::CommitmentTransaction, ingress::IngressProof,
    transaction::DataTransactionHeader,
};

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
    let canonical = canonical_unpacked_chunk();
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
    let canonical = canonical_ingress_proof();
    let wire_type: wire::IngressProof = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::IngressProof = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: IngressProof = deserialized.into();
    assert_eq!(canonical, roundtrip);
}

// =============================================================================
// CommitmentTransaction parity
// =============================================================================

#[test]
fn test_commitment_v1_stake_parity() {
    let canonical = canonical_commitment_v1_stake();
    let wire_type: wire::CommitmentTransaction = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::CommitmentTransaction = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: CommitmentTransaction = deserialized.into();
    // Compare fields since metadata is skipped in serde
    assert_eq!(canonical.id(), roundtrip.id());
    assert_eq!(canonical.signer(), roundtrip.signer());
    assert_eq!(canonical.fee(), roundtrip.fee());
    assert_eq!(canonical.value(), roundtrip.value());
}

#[test]
fn test_commitment_v1_pledge_parity() {
    let canonical = canonical_commitment_v1_pledge();
    let wire_type: wire::CommitmentTransaction = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);
}

#[test]
fn test_commitment_v1_unpledge_parity() {
    let canonical = canonical_commitment_v1_unpledge();
    let wire_type: wire::CommitmentTransaction = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);
}

#[test]
fn test_commitment_v2_stake_parity() {
    let canonical = canonical_commitment_v2_stake();
    let wire_type: wire::CommitmentTransaction = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);
}

#[test]
fn test_commitment_v2_update_reward_address_parity() {
    let canonical = canonical_commitment_v2_update_reward_address();
    let wire_type: wire::CommitmentTransaction = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);
}

// =============================================================================
// DataTransactionHeader parity
// =============================================================================

#[test]
fn test_data_transaction_header_parity() {
    let canonical = canonical_data_tx_header();
    let wire_type: wire::DataTransactionHeader = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::DataTransactionHeader = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: DataTransactionHeader = deserialized.into();
    assert_eq!(canonical.id, roundtrip.id);
    assert_eq!(canonical.data_size, roundtrip.data_size);
}

// =============================================================================
// IrysBlockHeader parity
// =============================================================================

#[test]
fn test_block_header_parity() {
    let canonical = canonical_block_header();
    let wire_type: wire::IrysBlockHeader = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::IrysBlockHeader = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::IrysBlockHeader = deserialized.into();
    assert_eq!(canonical.block_hash, roundtrip.block_hash);
    assert_eq!(canonical.height, roundtrip.height);
    assert_eq!(canonical.diff, roundtrip.diff);
}
