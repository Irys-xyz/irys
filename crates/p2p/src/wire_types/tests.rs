//! Wire type JSON roundtrip tests.
//!
//! Verifies that every sovereign wire type can survive a full roundtrip:
//! canonical → wire → JSON → wire → canonical. This ensures the `From` impls
//! and serde attributes are correct.

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
// UnpackedChunk roundtrip
// =============================================================================

#[test]
fn test_unpacked_chunk_roundtrip() {
    let canonical = canonical_unpacked_chunk();
    let wire_type: wire::UnpackedChunk = canonical.clone().into();

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::UnpackedChunk = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::UnpackedChunk = deserialized.into();
    assert_eq!(canonical, roundtrip);
}

// =============================================================================
// IngressProof roundtrip
// =============================================================================

#[test]
fn test_ingress_proof_roundtrip() {
    let canonical = canonical_ingress_proof();
    let wire_type: wire::IngressProof = canonical.clone().into();

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::IngressProof = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: IngressProof = deserialized.into();
    assert_eq!(canonical, roundtrip);
}

// =============================================================================
// CommitmentTransaction roundtrip
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
fn test_commitment_roundtrip(#[case] canonical: CommitmentTransaction) {
    let wire_type: wire::CommitmentTransaction = canonical.clone().into();

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::CommitmentTransaction = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: CommitmentTransaction = deserialized.into();
    assert_eq!(canonical, roundtrip);
}

// =============================================================================
// DataTransactionHeader roundtrip
// =============================================================================

#[test]
fn test_data_transaction_header_roundtrip() {
    let canonical = canonical_data_tx_header();
    let wire_type: wire::DataTransactionHeader = canonical.clone().into();

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::DataTransactionHeader = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: DataTransactionHeader = deserialized.into();
    assert!(canonical.eq_tx(&roundtrip));
}

// =============================================================================
// IrysBlockHeader roundtrip
// =============================================================================

#[test]
fn test_block_header_roundtrip() {
    let canonical = canonical_block_header();
    let wire_type: wire::IrysBlockHeader = canonical.clone().into();

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
    let wire_type: wire::IrysBlockHeader = canonical_block_header().into();
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
// BlockBody roundtrip
// =============================================================================

#[test]
fn test_block_body_roundtrip() {
    let canonical = canonical_block_body();
    let wire_type: wire::BlockBody = canonical.clone().into();

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
    let canonical = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
        tx: DataTransactionHeaderV1 {
            perm_fee: None,
            bundle_format: None,
            ..canonical_data_tx_header_v1_inner()
        },
        metadata: DataTransactionMetadata::new(),
    });
    let wire_type: wire::DataTransactionHeader = canonical.clone().into();

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::DataTransactionHeader = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: DataTransactionHeader = deserialized.into();
    assert!(canonical.eq_tx(&roundtrip));
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
    let wire_poa: wire::PoaData = poa.clone().into();

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

    let wire_type: wire::IrysBlockHeader = canonical.into();

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
// HandshakeRequest/Response roundtrip
// =============================================================================

#[test]
fn test_handshake_request_v1_roundtrip() {
    let canonical = canonical_handshake_request_v1();
    let wire: wire::HandshakeRequestV1 = canonical.clone().into();

    let wire_json = serde_json::to_string(&wire).unwrap();
    let deserialized: wire::HandshakeRequestV1 = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::HandshakeRequestV1 = deserialized.into();
    assert_eq!(canonical, roundtrip);
}

#[test]
fn test_handshake_request_v2_roundtrip() {
    let canonical = canonical_handshake_request_v2();
    let wire: wire::HandshakeRequestV2 = canonical.clone().into();

    let wire_json = serde_json::to_string(&wire).unwrap();
    let deserialized: wire::HandshakeRequestV2 = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::HandshakeRequestV2 = deserialized.into();
    assert_eq!(canonical, roundtrip);
}

#[test]
fn test_handshake_response_v1_roundtrip() {
    let canonical = canonical_handshake_response_v1();
    let wire: wire::HandshakeResponseV1 = canonical.clone().into();

    let wire_json = serde_json::to_string(&wire).unwrap();
    let deserialized: wire::HandshakeResponseV1 = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::HandshakeResponseV1 = deserialized.into();
    assert_eq!(canonical, roundtrip);
}

#[test]
fn test_handshake_response_v2_roundtrip() {
    let canonical = canonical_handshake_response_v2();
    let wire: wire::HandshakeResponseV2 = canonical.clone().into();

    let wire_json = serde_json::to_string(&wire).unwrap();
    let deserialized: wire::HandshakeResponseV2 = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::HandshakeResponseV2 = deserialized.into();
    assert_eq!(canonical, roundtrip);
}

// =============================================================================
// GossipDataV1 roundtrip
// =============================================================================

#[test]
fn test_gossip_data_v1_roundtrip() {
    fn assert_roundtrip(canonical: gossip::v1::GossipDataV1) {
        let wire: wire::GossipDataV1 = canonical.clone().into();
        let json = serde_json::to_string(&wire).unwrap();
        let deserialized: wire::GossipDataV1 = serde_json::from_str(&json).unwrap();
        let roundtrip: gossip::v1::GossipDataV1 = deserialized.into();
        assert_eq!(canonical, roundtrip);
    }

    assert_roundtrip(gossip::v1::GossipDataV1::Chunk(canonical_unpacked_chunk()));
    assert_roundtrip(gossip::v1::GossipDataV1::Transaction(
        canonical_data_tx_header(),
    ));
    assert_roundtrip(gossip::v1::GossipDataV1::CommitmentTransaction(
        canonical_commitment_v2_stake(),
    ));
    assert_roundtrip(gossip::v1::GossipDataV1::Block(Arc::new(
        canonical_block_header(),
    )));
    assert_roundtrip(gossip::v1::GossipDataV1::ExecutionPayload(
        canonical_execution_payload(),
    ));
    assert_roundtrip(gossip::v1::GossipDataV1::IngressProof(
        canonical_ingress_proof(),
    ));
}

// =============================================================================
// GossipDataV2 roundtrip
// =============================================================================

#[test]
fn test_gossip_data_v2_roundtrip() {
    fn assert_roundtrip(canonical: gossip::v2::GossipDataV2) {
        let wire: wire::GossipDataV2 = canonical.clone().into();
        let json = serde_json::to_string(&wire).unwrap();
        let deserialized: wire::GossipDataV2 = serde_json::from_str(&json).unwrap();
        let roundtrip: gossip::v2::GossipDataV2 = deserialized.into();
        assert_eq!(canonical, roundtrip);
    }

    assert_roundtrip(gossip::v2::GossipDataV2::Chunk(Arc::new(
        canonical_unpacked_chunk(),
    )));
    assert_roundtrip(gossip::v2::GossipDataV2::Transaction(
        canonical_data_tx_header(),
    ));
    assert_roundtrip(gossip::v2::GossipDataV2::CommitmentTransaction(
        canonical_commitment_v2_stake(),
    ));
    assert_roundtrip(gossip::v2::GossipDataV2::BlockHeader(Arc::new(
        canonical_block_header(),
    )));
    assert_roundtrip(gossip::v2::GossipDataV2::BlockBody(Arc::new(
        canonical_block_body(),
    )));
    assert_roundtrip(gossip::v2::GossipDataV2::ExecutionPayload(
        canonical_execution_payload(),
    ));
    assert_roundtrip(gossip::v2::GossipDataV2::IngressProof(
        canonical_ingress_proof(),
    ));
}

// =============================================================================
// NodeInfo roundtrip
// =============================================================================

#[test]
fn test_node_info_roundtrip() {
    let canonical_json = serde_json::to_string(&canonical_node_info()).unwrap();
    let wire_type: wire::NodeInfoV1 = canonical_node_info().into();

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::NodeInfoV1 = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::version::NodeInfo = deserialized.into();
    // NodeInfo doesn't derive PartialEq, so compare via JSON
    let roundtrip_json = serde_json::to_string(&roundtrip).unwrap();
    assert_eq!(canonical_json, roundtrip_json);
}

#[test]
fn test_node_info_v2_roundtrip() {
    let canonical_json = serde_json::to_string(&canonical_node_info()).unwrap();
    let wire_type: wire::NodeInfoV2 = canonical_node_info().into();

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    // V2 serializes peer_count as a string
    assert!(
        wire_json.contains(r#""peerCount":"5""#),
        "V2 must serialize peerCount as a string"
    );
    let deserialized: wire::NodeInfoV2 = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::version::NodeInfo = deserialized.into();
    // NodeInfo doesn't derive PartialEq, so compare via JSON
    let roundtrip_json = serde_json::to_string(&roundtrip).unwrap();
    assert_eq!(canonical_json, roundtrip_json);
}

// =============================================================================
// BlockIndexItem roundtrip (V1 — with num_ledgers)
// =============================================================================

#[test]
fn test_block_index_item_v1_roundtrip() {
    let canonical = canonical_block_index_item();
    let wire_type: wire::BlockIndexItemV1 = canonical.clone().into();

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::BlockIndexItemV1 = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::block::BlockIndexItem = deserialized.into();
    assert_eq!(canonical, roundtrip);
}

// =============================================================================
// BlockIndexItem roundtrip (V2 — without num_ledgers)
// =============================================================================

#[test]
fn test_block_index_item_roundtrip() {
    let canonical = canonical_block_index_item();
    let wire_type: wire::BlockIndexItemV2 = canonical.clone().into();

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    assert!(
        !wire_json.contains("num_ledgers"),
        "V2 wire format must not contain num_ledgers"
    );
    let deserialized: wire::BlockIndexItemV2 = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: irys_types::block::BlockIndexItem = deserialized.into();
    assert_eq!(canonical, roundtrip);
}

// =============================================================================
// IrysTransactionResponse roundtrip
// =============================================================================

#[rstest::rstest]
#[case::storage(canonical_irys_tx_response_storage())]
#[case::commitment(canonical_irys_tx_response_commitment())]
fn test_irys_transaction_response_roundtrip(#[case] canonical: IrysTransactionResponse) {
    let wire_type: wire::IrysTransactionResponse = canonical.clone().into();

    let wire_json = serde_json::to_string(&wire_type).unwrap();
    let deserialized: wire::IrysTransactionResponse = serde_json::from_str(&wire_json).unwrap();
    let roundtrip: IrysTransactionResponse = deserialized.into();
    assert_eq!(canonical, roundtrip);
}
