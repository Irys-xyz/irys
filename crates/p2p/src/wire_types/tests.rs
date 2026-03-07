//! Wire type JSON parity tests.
//!
//! Verifies that every sovereign wire type produces identical JSON output
//! to the corresponding canonical irys_types type. If a wire type drifts
//! from the canonical format, these tests catch it.

use std::sync::Arc;

use irys_types::{
    commitment_common::CommitmentTransaction,
    gossip,
    ingress::IngressProof,
    serialization::H256List,
    transaction::{
        DataTransactionHeader, DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata,
        DataTransactionMetadata,
    },
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
    // Compare fields since metadata is skipped in serde.
    // Note: chain_id has no getter on CommitmentTransaction; covered by assert_json_parity above.
    assert_eq!(canonical.id(), roundtrip.id());
    assert_eq!(canonical.signer(), roundtrip.signer());
    assert_eq!(canonical.fee(), roundtrip.fee());
    assert_eq!(canonical.value(), roundtrip.value());
    assert_eq!(canonical.anchor(), roundtrip.anchor());
    assert_eq!(canonical.commitment_type(), roundtrip.commitment_type());
    assert_eq!(canonical.signature(), roundtrip.signature());
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
    assert_eq!(canonical.anchor, roundtrip.anchor);
    assert_eq!(canonical.signer, roundtrip.signer);
    assert_eq!(canonical.data_root, roundtrip.data_root);
    assert_eq!(canonical.data_size, roundtrip.data_size);
    assert_eq!(canonical.header_size, roundtrip.header_size);
    assert_eq!(canonical.term_fee, roundtrip.term_fee);
    assert_eq!(canonical.perm_fee, roundtrip.perm_fee);
    assert_eq!(canonical.ledger_id, roundtrip.ledger_id);
    assert_eq!(canonical.bundle_format, roundtrip.bundle_format);
    assert_eq!(canonical.chain_id, roundtrip.chain_id);
    assert_eq!(canonical.signature, roundtrip.signature);
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
    assert_eq!(canonical.signature, roundtrip.signature);
    assert_eq!(canonical.height, roundtrip.height);
    assert_eq!(canonical.diff, roundtrip.diff);
    assert_eq!(canonical.cumulative_diff, roundtrip.cumulative_diff);
    assert_eq!(canonical.solution_hash, roundtrip.solution_hash);
    assert_eq!(canonical.last_diff_timestamp, roundtrip.last_diff_timestamp);
    assert_eq!(
        canonical.previous_solution_hash,
        roundtrip.previous_solution_hash
    );
    assert_eq!(canonical.last_epoch_hash, roundtrip.last_epoch_hash);
    assert_eq!(canonical.chunk_hash, roundtrip.chunk_hash);
    assert_eq!(canonical.previous_block_hash, roundtrip.previous_block_hash);
    assert_eq!(
        canonical.previous_cumulative_diff,
        roundtrip.previous_cumulative_diff
    );
    assert_eq!(canonical.poa, roundtrip.poa);
    assert_eq!(canonical.reward_address, roundtrip.reward_address);
    assert_eq!(canonical.reward_amount, roundtrip.reward_amount);
    assert_eq!(canonical.miner_address, roundtrip.miner_address);
    assert_eq!(canonical.timestamp, roundtrip.timestamp);
    assert_eq!(canonical.system_ledgers, roundtrip.system_ledgers);
    assert_eq!(canonical.data_ledgers, roundtrip.data_ledgers);
    assert_eq!(canonical.evm_block_hash, roundtrip.evm_block_hash);
    assert_eq!(canonical.vdf_limiter_info, roundtrip.vdf_limiter_info);
    assert_eq!(canonical.oracle_irys_price, roundtrip.oracle_irys_price);
    assert_eq!(canonical.ema_irys_price, roundtrip.ema_irys_price);
    assert_eq!(canonical.treasury, roundtrip.treasury);
}

// =============================================================================
// Error-path tests for version-tagged serde
// =============================================================================

#[test]
fn test_version_tagged_unknown_version() {
    let mut obj = serde_json::to_value(canonical_block_header()).unwrap();
    obj.as_object_mut()
        .unwrap()
        .insert("version".to_string(), serde_json::json!(255));
    let result = serde_json::from_value::<wire::IrysBlockHeader>(obj);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("unknown"),
        "Error should mention unknown version: {err}"
    );
}

#[test]
fn test_version_tagged_missing_version() {
    let wire_type: wire::IrysBlockHeader = (&canonical_block_header()).into();
    let mut obj = serde_json::to_value(&wire_type).unwrap();
    obj.as_object_mut().unwrap().remove("version");
    let result = serde_json::from_value::<wire::IrysBlockHeader>(obj);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("version"),
        "Error should mention missing version field: {err}"
    );
}

#[test]
fn test_version_tagged_non_object_input() {
    let json_array = serde_json::json!([1, 2, 3]);
    let result = serde_json::from_value::<wire::IrysBlockHeader>(json_array);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("expected object"),
        "Error should mention expected object: {err}"
    );
}

// =============================================================================
// BlockBody parity
// =============================================================================

#[test]
fn test_block_body_parity() {
    let canonical = canonical_block_body();
    let wire_type: wire::BlockBody = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::BlockBody = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::BlockBody = deserialized.into();
    assert_eq!(canonical.block_hash, roundtrip.block_hash);
    assert_eq!(
        canonical.data_transactions.len(),
        roundtrip.data_transactions.len()
    );
    assert_eq!(
        canonical.commitment_transactions.len(),
        roundtrip.commitment_transactions.len()
    );
    // Verify individual transaction fields survived the round-trip
    assert_eq!(
        canonical.data_transactions[0].id,
        roundtrip.data_transactions[0].id
    );
    assert_eq!(
        canonical.commitment_transactions[0].id(),
        roundtrip.commitment_transactions[0].id()
    );
}

// =============================================================================
// None/empty edge case tests
// =============================================================================

#[test]
fn test_data_tx_header_with_none_optional_fields() {
    // DataTransactionHeader with perm_fee: None and bundle_format: None
    let canonical = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
        tx: DataTransactionHeaderV1 {
            perm_fee: None,
            bundle_format: None,
            ..canonical_data_tx_header_v1_inner()
        },
        metadata: DataTransactionMetadata::new(),
    });
    let wire_type: wire::DataTransactionHeader = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::DataTransactionHeader = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: DataTransactionHeader = deserialized.into();
    assert_eq!(roundtrip.perm_fee, None);
    assert_eq!(roundtrip.bundle_format, None);
}

#[test]
fn test_poa_data_all_none_fields() {
    use irys_types::block::PoaData;
    let poa = PoaData {
        partition_chunk_offset: 0,
        partition_hash: test_h256(0x00),
        chunk: None,
        ledger_id: None,
        tx_path: None,
        data_path: None,
    };
    let wire_poa: wire::PoaData = (&poa).into();
    assert_json_parity(&poa, &wire_poa);

    let wire_json = serde_json::to_string(&wire_poa).unwrap();
    let deserialized: wire::PoaData = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: PoaData = deserialized.into();
    assert_eq!(poa, roundtrip);
}

#[test]
fn test_block_header_empty_ledgers() {
    let irys_types::IrysBlockHeader::V1(mut header) = canonical_block_header();
    header.system_ledgers = vec![];
    header.data_ledgers = vec![];
    header.vdf_limiter_info.last_step_checkpoints = H256List(vec![]);
    header.vdf_limiter_info.steps = H256List(vec![]);
    let canonical = irys_types::IrysBlockHeader::V1(header);

    let wire_type: wire::IrysBlockHeader = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::IrysBlockHeader = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::IrysBlockHeader = deserialized.into();
    assert!(roundtrip.system_ledgers.is_empty());
    assert!(roundtrip.data_ledgers.is_empty());
    assert!(roundtrip
        .vdf_limiter_info
        .last_step_checkpoints
        .0
        .is_empty());
    assert!(roundtrip.vdf_limiter_info.steps.0.is_empty());
}

// =============================================================================
// Missing commitment variant parity tests
// =============================================================================

#[test]
fn test_commitment_v1_unstake_parity() {
    let canonical = canonical_commitment_v1_unstake();
    let wire_type: wire::CommitmentTransaction = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::CommitmentTransaction = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: CommitmentTransaction = deserialized.into();
    assert_eq!(canonical.id(), roundtrip.id());
    assert_eq!(canonical.commitment_type(), roundtrip.commitment_type());
}

#[test]
fn test_commitment_v2_pledge_parity() {
    let canonical = canonical_commitment_v2_pledge();
    let wire_type: wire::CommitmentTransaction = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::CommitmentTransaction = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: CommitmentTransaction = deserialized.into();
    assert_eq!(canonical.id(), roundtrip.id());
    assert_eq!(canonical.commitment_type(), roundtrip.commitment_type());
}

#[test]
fn test_commitment_v2_unpledge_parity() {
    let canonical = canonical_commitment_v2_unpledge();
    let wire_type: wire::CommitmentTransaction = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::CommitmentTransaction = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: CommitmentTransaction = deserialized.into();
    assert_eq!(canonical.id(), roundtrip.id());
    assert_eq!(canonical.commitment_type(), roundtrip.commitment_type());
}

#[test]
fn test_commitment_v2_unstake_parity() {
    let canonical = canonical_commitment_v2_unstake();
    let wire_type: wire::CommitmentTransaction = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::CommitmentTransaction = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: CommitmentTransaction = deserialized.into();
    assert_eq!(canonical.id(), roundtrip.id());
    assert_eq!(canonical.commitment_type(), roundtrip.commitment_type());
}

// =============================================================================
// HandshakeRequest/Response parity
// =============================================================================

#[test]
fn test_handshake_request_v1_parity() {
    let canonical = canonical_handshake_request_v1();
    let wire: wire::HandshakeRequestV1 = (&canonical).into();
    assert_json_parity(&canonical, &wire);

    let wire_json = serde_json::to_string(&wire).unwrap();
    let deserialized: wire::HandshakeRequestV1 = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::HandshakeRequestV1 = deserialized.into();
    assert_eq!(canonical.mining_address, roundtrip.mining_address);
    assert_eq!(canonical.chain_id, roundtrip.chain_id);
    assert_eq!(canonical.timestamp, roundtrip.timestamp);
    assert_eq!(canonical.user_agent, roundtrip.user_agent);
}

#[test]
fn test_handshake_request_v2_parity() {
    let canonical = canonical_handshake_request_v2();
    let wire: wire::HandshakeRequestV2 = (&canonical).into();
    assert_json_parity(&canonical, &wire);

    let wire_json = serde_json::to_string(&wire).unwrap();
    let deserialized: wire::HandshakeRequestV2 = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::HandshakeRequestV2 = deserialized.into();
    assert_eq!(canonical.mining_address, roundtrip.mining_address);
    assert_eq!(canonical.peer_id, roundtrip.peer_id);
    assert_eq!(canonical.chain_id, roundtrip.chain_id);
    assert_eq!(canonical.consensus_config_hash, roundtrip.consensus_config_hash);
}

#[test]
fn test_handshake_response_v1_parity() {
    let canonical = canonical_handshake_response_v1();
    let wire: wire::HandshakeResponseV1 = (&canonical).into();
    assert_json_parity(&canonical, &wire);

    let wire_json = serde_json::to_string(&wire).unwrap();
    let deserialized: wire::HandshakeResponseV1 = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::HandshakeResponseV1 = deserialized.into();
    assert_eq!(canonical.peers.len(), roundtrip.peers.len());
    assert_eq!(canonical.timestamp, roundtrip.timestamp);
    assert_eq!(canonical.message, roundtrip.message);
}

#[test]
fn test_handshake_response_v2_parity() {
    let canonical = canonical_handshake_response_v2();
    let wire: wire::HandshakeResponseV2 = (&canonical).into();
    assert_json_parity(&canonical, &wire);

    let wire_json = serde_json::to_string(&wire).unwrap();
    let deserialized: wire::HandshakeResponseV2 = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::HandshakeResponseV2 = deserialized.into();
    assert_eq!(canonical.peers.len(), roundtrip.peers.len());
    assert_eq!(canonical.timestamp, roundtrip.timestamp);
    assert_eq!(canonical.consensus_config_hash, roundtrip.consensus_config_hash);
}

// =============================================================================
// GossipDataV1 parity
// =============================================================================

#[test]
fn test_gossip_data_v1_parity() {
    // Chunk variant
    let canonical = gossip::v1::GossipDataV1::Chunk(canonical_unpacked_chunk());
    let wire: wire::GossipDataV1 = (&canonical).into();
    assert_json_parity(&canonical, &wire);

    // Transaction variant
    let canonical = gossip::v1::GossipDataV1::Transaction(canonical_data_tx_header());
    let wire: wire::GossipDataV1 = (&canonical).into();
    assert_json_parity(&canonical, &wire);

    // CommitmentTransaction variant
    let canonical = gossip::v1::GossipDataV1::CommitmentTransaction(canonical_commitment_v2_stake());
    let wire: wire::GossipDataV1 = (&canonical).into();
    assert_json_parity(&canonical, &wire);

    // Block variant (canonical wraps in Arc)
    let canonical = gossip::v1::GossipDataV1::Block(Arc::new(canonical_block_header()));
    let wire: wire::GossipDataV1 = (&canonical).into();
    assert_json_parity(&canonical, &wire);

    // IngressProof variant
    let canonical = gossip::v1::GossipDataV1::IngressProof(canonical_ingress_proof());
    let wire: wire::GossipDataV1 = (&canonical).into();
    assert_json_parity(&canonical, &wire);
}

// =============================================================================
// GossipDataV2 parity
// =============================================================================

#[test]
fn test_gossip_data_v2_parity() {
    // Chunk variant (canonical wraps in Arc)
    let canonical = gossip::v2::GossipDataV2::Chunk(Arc::new(canonical_unpacked_chunk()));
    let wire: wire::GossipDataV2 = (&canonical).into();
    assert_json_parity(&canonical, &wire);

    // Transaction variant
    let canonical = gossip::v2::GossipDataV2::Transaction(canonical_data_tx_header());
    let wire: wire::GossipDataV2 = (&canonical).into();
    assert_json_parity(&canonical, &wire);

    // CommitmentTransaction variant
    let canonical =
        gossip::v2::GossipDataV2::CommitmentTransaction(canonical_commitment_v2_stake());
    let wire: wire::GossipDataV2 = (&canonical).into();
    assert_json_parity(&canonical, &wire);

    // BlockHeader variant (canonical wraps in Arc)
    let canonical = gossip::v2::GossipDataV2::BlockHeader(Arc::new(canonical_block_header()));
    let wire: wire::GossipDataV2 = (&canonical).into();
    assert_json_parity(&canonical, &wire);

    // BlockBody variant (canonical wraps in Arc)
    let canonical = gossip::v2::GossipDataV2::BlockBody(Arc::new(canonical_block_body()));
    let wire: wire::GossipDataV2 = (&canonical).into();
    assert_json_parity(&canonical, &wire);

    // IngressProof variant
    let canonical = gossip::v2::GossipDataV2::IngressProof(canonical_ingress_proof());
    let wire: wire::GossipDataV2 = (&canonical).into();
    assert_json_parity(&canonical, &wire);
}

// =============================================================================
// GossipDataRequest parity
// =============================================================================

#[test]
fn test_gossip_data_request_v1_parity() {
    use reth::revm::primitives::B256;
    let variants = [
        gossip::v1::GossipDataRequestV1::ExecutionPayload(B256::from([0xAA; 32])),
        gossip::v1::GossipDataRequestV1::Block(test_h256(0xBB)),
        gossip::v1::GossipDataRequestV1::Chunk(test_h256(0xCC)),
        gossip::v1::GossipDataRequestV1::Transaction(test_h256(0xDD)),
    ];
    for canonical in &variants {
        let wire: wire::GossipDataRequestV1 = canonical.into();
        assert_json_parity(canonical, &wire);
    }
}

#[test]
fn test_gossip_data_request_v2_parity() {
    use reth::revm::primitives::B256;
    let variants = [
        gossip::v2::GossipDataRequestV2::ExecutionPayload(B256::from([0xAA; 32])),
        gossip::v2::GossipDataRequestV2::BlockHeader(test_h256(0xBB)),
        gossip::v2::GossipDataRequestV2::BlockBody(test_h256(0xCC)),
        gossip::v2::GossipDataRequestV2::Chunk(test_h256(0xDD)),
        gossip::v2::GossipDataRequestV2::Transaction(test_h256(0xEE)),
    ];
    for canonical in &variants {
        let wire: wire::GossipDataRequestV2 = canonical.into();
        assert_json_parity(canonical, &wire);
    }
}

// =============================================================================
// GossipRequest wrapper parity
// =============================================================================

#[test]
fn test_gossip_request_v1_parity() {
    let canonical = canonical_gossip_request_v1();
    let wire = wire::GossipRequestV1 {
        miner_address: canonical.miner_address,
        data: wire::UnpackedChunk::from(&canonical.data),
    };
    assert_json_parity(&canonical, &wire);
}

#[test]
fn test_gossip_request_v2_parity() {
    let canonical = canonical_gossip_request_v2();
    let wire = wire::GossipRequestV2 {
        peer_id: canonical.peer_id,
        miner_address: canonical.miner_address,
        data: wire::UnpackedChunk::from(&canonical.data),
    };
    assert_json_parity(&canonical, &wire);
}
