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
        DataTransactionMetadata, IrysTransactionResponse,
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

#[rstest::rstest]
#[case::v1_stake(canonical_commitment_v1_stake())]
#[case::v1_pledge(canonical_commitment_v1_pledge())]
#[case::v1_unpledge(canonical_commitment_v1_unpledge())]
#[case::v1_unstake(canonical_commitment_v1_unstake())]
#[case::v2_stake(canonical_commitment_v2_stake())]
#[case::v2_pledge(canonical_commitment_v2_pledge())]
#[case::v2_unpledge(canonical_commitment_v2_unpledge())]
#[case::v2_unstake(canonical_commitment_v2_unstake())]
#[case::v2_update_reward_address(canonical_commitment_v2_update_reward_address())]
fn test_commitment_parity(#[case] canonical: CommitmentTransaction) {
    let wire_type: wire::CommitmentTransaction = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::CommitmentTransaction = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: CommitmentTransaction = deserialized.into();
    assert_eq!(canonical, roundtrip);
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
    assert_eq!(canonical, roundtrip);
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
    assert_eq!(canonical, roundtrip);
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
    assert_eq!(canonical, roundtrip);
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
    assert_eq!(canonical, roundtrip);
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
    assert_eq!(canonical, roundtrip);
}

#[test]
fn test_handshake_request_v2_parity() {
    let canonical = canonical_handshake_request_v2();
    let wire: wire::HandshakeRequestV2 = (&canonical).into();
    assert_json_parity(&canonical, &wire);

    let wire_json = serde_json::to_string(&wire).unwrap();
    let deserialized: wire::HandshakeRequestV2 = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::HandshakeRequestV2 = deserialized.into();
    assert_eq!(canonical, roundtrip);
}

#[test]
fn test_handshake_response_v1_parity() {
    let canonical = canonical_handshake_response_v1();
    let wire: wire::HandshakeResponseV1 = (&canonical).into();
    assert_json_parity(&canonical, &wire);

    let wire_json = serde_json::to_string(&wire).unwrap();
    let deserialized: wire::HandshakeResponseV1 = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::HandshakeResponseV1 = deserialized.into();
    assert_eq!(canonical, roundtrip);
}

#[test]
fn test_handshake_response_v2_parity() {
    let canonical = canonical_handshake_response_v2();
    let wire: wire::HandshakeResponseV2 = (&canonical).into();
    assert_json_parity(&canonical, &wire);

    let wire_json = serde_json::to_string(&wire).unwrap();
    let deserialized: wire::HandshakeResponseV2 = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::HandshakeResponseV2 = deserialized.into();
    assert_eq!(canonical, roundtrip);
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
    let canonical =
        gossip::v1::GossipDataV1::CommitmentTransaction(canonical_commitment_v2_stake());
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

#[rstest::rstest]
#[case::execution_payload(gossip::v1::GossipDataRequestV1::ExecutionPayload(reth::revm::primitives::B256::from([0xAA; 32])))]
#[case::block(gossip::v1::GossipDataRequestV1::Block(test_h256(0xBB)))]
#[case::chunk(gossip::v1::GossipDataRequestV1::Chunk(test_h256(0xCC)))]
#[case::transaction(gossip::v1::GossipDataRequestV1::Transaction(test_h256(0xDD)))]
fn test_gossip_data_request_v1_parity(#[case] canonical: gossip::v1::GossipDataRequestV1) {
    let wire: wire::GossipDataRequestV1 = (&canonical).into();
    assert_json_parity(&canonical, &wire);
}

#[rstest::rstest]
#[case::execution_payload(gossip::v2::GossipDataRequestV2::ExecutionPayload(reth::revm::primitives::B256::from([0xAA; 32])))]
#[case::block_header(gossip::v2::GossipDataRequestV2::BlockHeader(test_h256(0xBB)))]
#[case::block_body(gossip::v2::GossipDataRequestV2::BlockBody(test_h256(0xCC)))]
#[case::chunk(gossip::v2::GossipDataRequestV2::Chunk(test_h256(0xDD)))]
#[case::transaction(gossip::v2::GossipDataRequestV2::Transaction(test_h256(0xEE)))]
fn test_gossip_data_request_v2_parity(#[case] canonical: gossip::v2::GossipDataRequestV2) {
    let wire: wire::GossipDataRequestV2 = (&canonical).into();
    assert_json_parity(&canonical, &wire);
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

// =============================================================================
// NodeInfo parity
// =============================================================================

#[test]
fn test_node_info_parity() {
    let canonical = canonical_node_info();
    let wire_type: wire::NodeInfo = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::NodeInfo = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::version::NodeInfo = deserialized.into();
    // NodeInfo doesn't derive PartialEq, so compare via JSON
    assert_json_parity(&canonical, &roundtrip);
}

// =============================================================================
// BlockIndexItem parity
// =============================================================================

#[test]
fn test_block_index_item_parity() {
    let canonical = canonical_block_index_item();
    let wire_type: wire::BlockIndexItem = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::BlockIndexItem = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::block::BlockIndexItem = deserialized.into();
    assert_eq!(canonical, roundtrip);
}

// =============================================================================
// IrysTransactionResponse parity
// =============================================================================

#[rstest::rstest]
#[case::storage(canonical_irys_tx_response_storage())]
#[case::commitment(canonical_irys_tx_response_commitment())]
fn test_irys_transaction_response_parity(#[case] canonical: IrysTransactionResponse) {
    let wire_type: wire::IrysTransactionResponse = (&canonical).into();
    assert_json_parity(&canonical, &wire_type);

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::IrysTransactionResponse = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: IrysTransactionResponse = deserialized.into();
    assert_eq!(canonical, roundtrip);
}
