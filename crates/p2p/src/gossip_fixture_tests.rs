//! Gossip protocol JSON serialization fixture tests.
//!
//! These tests verify that the JSON serialization format of all gossip protocol
//! messages remains stable across code changes. Each test constructs a message
//! with deterministic values, serializes it to JSON, and asserts the output
//! matches the expected fixture from `gossip_fixtures.json`.
//!
//! If any test fails, it means a gossip protocol message format has changed,
//! which could break network compatibility between nodes running different
//! versions.
//!
//! To regenerate fixtures: `cargo test -p irys-p2p generate_fixture_json -- --ignored`

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::OnceLock;

use irys_types::{
    version::{NodeInfo, PeerAddress, ProtocolVersion},
    IrysPeerId, RethPeerInfo, U256,
};
use reth::revm::primitives::B256;
use reth_ethereum_primitives::Block as RethBlock;
use semver::Version;
use serde::{de::DeserializeOwned, Serialize};

use crate::types::{GossipResponse, HandshakeRequirementReason, RejectionReason};
use crate::wire_types::{self as wire, test_helpers::*};

// =============================================================================
// Additional test value constructors (not shared — specific to fixture tests)
// =============================================================================

fn test_peer_id(byte: u8) -> IrysPeerId {
    IrysPeerId::from(test_address(byte))
}

fn test_peer_address() -> PeerAddress {
    PeerAddress {
        gossip: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 4200),
        api: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 4201),
        execution: RethPeerInfo {
            peering_tcp_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 30303),
            peer_id: Default::default(),
        },
    }
}

fn fixture_unpacked_chunk() -> wire::UnpackedChunk {
    (&canonical_unpacked_chunk()).into()
}

fn fixture_data_tx_header() -> wire::DataTransactionHeader {
    (&canonical_data_tx_header()).into()
}

fn fixture_commitment_v1_stake() -> wire::CommitmentTransaction {
    (&canonical_commitment_v1_stake()).into()
}

fn fixture_commitment_v1_pledge() -> wire::CommitmentTransaction {
    (&canonical_commitment_v1_pledge()).into()
}

fn fixture_commitment_v1_unpledge() -> wire::CommitmentTransaction {
    (&canonical_commitment_v1_unpledge()).into()
}

fn fixture_commitment_v1_unstake() -> wire::CommitmentTransaction {
    (&canonical_commitment_v1_unstake()).into()
}

fn fixture_commitment_v2_stake() -> wire::CommitmentTransaction {
    (&canonical_commitment_v2_stake()).into()
}

fn fixture_commitment_v2_pledge() -> wire::CommitmentTransaction {
    (&canonical_commitment_v2_pledge()).into()
}

fn fixture_commitment_v2_unpledge() -> wire::CommitmentTransaction {
    (&canonical_commitment_v2_unpledge()).into()
}

fn fixture_commitment_v2_unstake() -> wire::CommitmentTransaction {
    (&canonical_commitment_v2_unstake()).into()
}

fn fixture_commitment_v2_update_reward_address() -> wire::CommitmentTransaction {
    (&canonical_commitment_v2_update_reward_address()).into()
}

fn fixture_block_header() -> wire::IrysBlockHeader {
    (&canonical_block_header()).into()
}

fn fixture_ingress_proof() -> wire::IngressProof {
    (&canonical_ingress_proof()).into()
}

fn fixture_block_body() -> wire::BlockBody {
    (&canonical_block_body()).into()
}

fn fixture_execution_payload() -> RethBlock {
    canonical_execution_payload()
}

// =============================================================================
// Fixture file helpers
// =============================================================================

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/gossip_fixtures.json")
}

fn load_fixtures() -> serde_json::Map<String, serde_json::Value> {
    let path = fixture_path();
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
    let value: serde_json::Value = serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("Failed to parse {}: {e}", path.display()));
    match value {
        serde_json::Value::Object(map) => map,
        _ => panic!("{} must be a JSON object", path.display()),
    }
}

// =============================================================================
// Fixture assertion helper
// =============================================================================

static FIXTURES: OnceLock<serde_json::Map<String, serde_json::Value>> = OnceLock::new();

/// Asserts that the JSON serialization of a value matches the expected fixture
/// from gossip_fixtures.json, and that the value can be deserialized back.
fn assert_json_fixture<T: Serialize + DeserializeOwned + std::fmt::Debug>(
    value: &T,
    fixture_name: &str,
) {
    let fixtures = FIXTURES.get_or_init(load_fixtures);
    let expected_value = fixtures
        .get(fixture_name)
        .unwrap_or_else(|| panic!("Fixture '{fixture_name}' not found in gossip_fixtures.json"));

    let actual_value = serde_json::to_value(value).unwrap();
    assert_eq!(
        actual_value,
        *expected_value,
        "\n\nFixture mismatch for '{fixture_name}'!\n\
         The JSON serialization format has changed.\n\
         This may break gossip protocol compatibility.\n\n\
         === ACTUAL ===\n{}\n\n\
         === EXPECTED ===\n{}\n",
        serde_json::to_string_pretty(&actual_value).unwrap(),
        serde_json::to_string_pretty(expected_value).unwrap(),
    );

    // Verify roundtrip deserialization produces identical JSON
    let serialized = serde_json::to_string_pretty(value).unwrap();
    let deserialized: T = serde_json::from_str(&serialized).unwrap();
    let re_serialized = serde_json::to_string_pretty(&deserialized).unwrap();
    assert_eq!(
        serialized, re_serialized,
        "JSON roundtrip failed for '{fixture_name}' - \
         deserialization produced different output"
    );
}

// =============================================================================
// Fixture test definitions
// =============================================================================

/// Generates a `#[test]` function per entry and an `all_fixture_entries()` fn
/// that returns all (name, value) pairs for the fixture generator.
macro_rules! fixture_tests {
    ($($name:ident => $value:expr),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                assert_json_fixture(&$value, stringify!($name));
            }
        )*

        fn all_fixture_entries() -> Vec<(&'static str, serde_json::Value)> {
            vec![
                $( (stringify!($name), serde_json::to_value(&$value).unwrap()), )*
            ]
        }
    };
}

fixture_tests! {
    // V1 Gossip Data variants
    v1_gossip_data_chunk =>
        wire::GossipDataV1::Chunk(fixture_unpacked_chunk()),
    v1_gossip_data_transaction =>
        wire::GossipDataV1::Transaction(fixture_data_tx_header()),
    v1_gossip_data_commitment_stake =>
        wire::GossipDataV1::CommitmentTransaction(fixture_commitment_v1_stake()),
    v1_gossip_data_commitment_pledge =>
        wire::GossipDataV1::CommitmentTransaction(fixture_commitment_v1_pledge()),
    v1_gossip_data_commitment_unpledge =>
        wire::GossipDataV1::CommitmentTransaction(fixture_commitment_v1_unpledge()),
    v1_gossip_data_commitment_unstake =>
        wire::GossipDataV1::CommitmentTransaction(fixture_commitment_v1_unstake()),
    v1_gossip_data_block =>
        wire::GossipDataV1::Block(fixture_block_header()),
    v1_gossip_data_ingress_proof =>
        wire::GossipDataV1::IngressProof(fixture_ingress_proof()),
    v1_gossip_data_execution_payload =>
        wire::GossipDataV1::ExecutionPayload(fixture_execution_payload()),

    // V1 Data Requests
    v1_data_request_execution_payload =>
        wire::GossipDataRequestV1::ExecutionPayload(B256::from([0xEE; 32])),
    v1_data_request_block =>
        wire::GossipDataRequestV1::Block(test_h256(0xBB)),
    v1_data_request_chunk =>
        wire::GossipDataRequestV1::Chunk(test_h256(0xCC)),
    v1_data_request_transaction =>
        wire::GossipDataRequestV1::Transaction(test_h256(0xDD)),

    // V1 Request Wrapper
    v1_gossip_request => wire::GossipRequestV1 {
        miner_address: test_address(0xAA),
        data: wire::GossipDataV1::Chunk(fixture_unpacked_chunk()),
    },

    // V2 Gossip Data variants
    v2_gossip_data_chunk =>
        wire::GossipDataV2::Chunk(fixture_unpacked_chunk()),
    v2_gossip_data_transaction =>
        wire::GossipDataV2::Transaction(fixture_data_tx_header()),
    v2_gossip_data_commitment_v2_stake =>
        wire::GossipDataV2::CommitmentTransaction(fixture_commitment_v2_stake()),
    v2_gossip_data_commitment_v2_pledge =>
        wire::GossipDataV2::CommitmentTransaction(fixture_commitment_v2_pledge()),
    v2_gossip_data_commitment_v2_unpledge =>
        wire::GossipDataV2::CommitmentTransaction(fixture_commitment_v2_unpledge()),
    v2_gossip_data_commitment_v2_unstake =>
        wire::GossipDataV2::CommitmentTransaction(fixture_commitment_v2_unstake()),
    v2_gossip_data_commitment_v2_update_reward_address =>
        wire::GossipDataV2::CommitmentTransaction(fixture_commitment_v2_update_reward_address()),
    v2_gossip_data_block_header =>
        wire::GossipDataV2::BlockHeader(fixture_block_header()),
    v2_gossip_data_block_body =>
        wire::GossipDataV2::BlockBody(fixture_block_body()),
    v2_gossip_data_ingress_proof =>
        wire::GossipDataV2::IngressProof(fixture_ingress_proof()),
    v2_gossip_data_execution_payload =>
        wire::GossipDataV2::ExecutionPayload(fixture_execution_payload()),

    // V2 Data Requests
    v2_data_request_execution_payload =>
        wire::GossipDataRequestV2::ExecutionPayload(B256::from([0xEE; 32])),
    v2_data_request_block_header =>
        wire::GossipDataRequestV2::BlockHeader(test_h256(0xBB)),
    v2_data_request_block_body =>
        wire::GossipDataRequestV2::BlockBody(test_h256(0xBB)),
    v2_data_request_chunk =>
        wire::GossipDataRequestV2::Chunk(test_h256(0xCC)),
    v2_data_request_transaction =>
        wire::GossipDataRequestV2::Transaction(test_h256(0xDD)),

    // V2 Request Wrapper
    v2_gossip_request => wire::GossipRequestV2 {
        peer_id: test_peer_id(0xBB),
        miner_address: test_address(0xAA),
        data: wire::GossipDataV2::Chunk(fixture_unpacked_chunk()),
    },

    // Handshakes
    handshake_request_v1 => wire::HandshakeRequestV1 {
        version: Version::new(1, 2, 3),
        protocol_version: ProtocolVersion::V1,
        mining_address: test_address(0xAA),
        chain_id: 1270,
        address: test_peer_address(),
        timestamp: 1700000000000,
        user_agent: Some("irys-node/1.2.3".to_string()),
        signature: test_signature(),
    },
    handshake_request_v2 => wire::HandshakeRequestV2 {
        version: Version::new(1, 2, 3),
        protocol_version: ProtocolVersion::V2,
        mining_address: test_address(0xAA),
        peer_id: test_peer_id(0xBB),
        chain_id: 1270,
        address: test_peer_address(),
        timestamp: 1700000000000,
        user_agent: Some("irys-node/1.2.3".to_string()),
        consensus_config_hash: test_h256(0xFF),
        signature: test_signature(),
    },
    handshake_response_v1 => wire::HandshakeResponseV1 {
        version: Version::new(1, 2, 3),
        protocol_version: ProtocolVersion::V1,
        peers: vec![test_peer_address()],
        timestamp: 1700000000000,
        message: Some("Welcome".to_string()),
    },
    handshake_response_v2 => wire::HandshakeResponseV2 {
        version: Version::new(1, 2, 3),
        protocol_version: ProtocolVersion::V2,
        peers: vec![test_peer_address()],
        timestamp: 1700000000000,
        message: Some("Welcome".to_string()),
        consensus_config_hash: test_h256(0xFF),
    },

    // GossipResponse variants
    gossip_response_accepted =>
        GossipResponse::<()>::Accepted(()),
    gossip_response_rejected_handshake_required_none =>
        GossipResponse::<()>::Rejected(RejectionReason::HandshakeRequired(None)),
    gossip_response_rejected_handshake_required_not_in_peer_list =>
        GossipResponse::<()>::Rejected(RejectionReason::HandshakeRequired(Some(
            HandshakeRequirementReason::RequestOriginIsNotInThePeerList,
        ))),
    gossip_response_rejected_handshake_required_origin_mismatch =>
        GossipResponse::<()>::Rejected(RejectionReason::HandshakeRequired(Some(
            HandshakeRequirementReason::RequestOriginDoesNotMatchExpected,
        ))),
    gossip_response_rejected_handshake_required_unknown_miner =>
        GossipResponse::<()>::Rejected(RejectionReason::HandshakeRequired(Some(
            HandshakeRequirementReason::MinerAddressIsUnknown,
        ))),
    gossip_response_rejected_gossip_disabled =>
        GossipResponse::<()>::Rejected(RejectionReason::GossipDisabled),
    gossip_response_rejected_invalid_data =>
        GossipResponse::<()>::Rejected(RejectionReason::InvalidData),
    gossip_response_rejected_rate_limited =>
        GossipResponse::<()>::Rejected(RejectionReason::RateLimited),
    gossip_response_rejected_unable_to_verify_origin =>
        GossipResponse::<()>::Rejected(RejectionReason::UnableToVerifyOrigin),
    gossip_response_rejected_invalid_credentials =>
        GossipResponse::<()>::Rejected(RejectionReason::InvalidCredentials),
    gossip_response_rejected_protocol_mismatch =>
        GossipResponse::<()>::Rejected(RejectionReason::ProtocolMismatch),
    gossip_response_rejected_unsupported_protocol_version =>
        GossipResponse::<()>::Rejected(RejectionReason::UnsupportedProtocolVersion(99)),
    gossip_response_rejected_unsupported_feature =>
        GossipResponse::<()>::Rejected(RejectionReason::UnsupportedFeature),

    // GossipResponse with typed payloads (not just unit)
    gossip_response_accepted_handshake_v1 =>
        GossipResponse::Accepted(wire::HandshakeResponseV1 {
            version: Version::new(1, 2, 3),
            protocol_version: ProtocolVersion::V1,
            peers: vec![test_peer_address()],
            timestamp: 1700000000000,
            message: Some("Welcome".to_string()),
        }),
    gossip_response_accepted_handshake_v2 =>
        GossipResponse::Accepted(wire::HandshakeResponseV2 {
            version: Version::new(1, 2, 3),
            protocol_version: ProtocolVersion::V2,
            peers: vec![test_peer_address()],
            timestamp: 1700000000000,
            message: Some("Welcome".to_string()),
            consensus_config_hash: test_h256(0xFF),
        }),
    gossip_response_accepted_gossip_data_v1 =>
        GossipResponse::Accepted(Some(
            wire::GossipDataV1::Chunk(fixture_unpacked_chunk()),
        )),
    gossip_response_accepted_gossip_data_v2 =>
        GossipResponse::Accepted(Some(
            wire::GossipDataV2::Chunk(fixture_unpacked_chunk()),
        )),
    gossip_response_accepted_gossip_data_v1_none =>
        GossipResponse::Accepted(None::<wire::GossipDataV1>),
    gossip_response_accepted_gossip_data_v2_none =>
        GossipResponse::Accepted(None::<wire::GossipDataV2>),
    gossip_response_accepted_bool =>
        GossipResponse::Accepted(true),

    // Leaf types — standalone fixtures to detect serde changes in shared primitives
    leaf_irys_address => test_address(0xAA),
    leaf_h256 => test_h256(0xBB),
    leaf_b256 => B256::from([0xCC; 32]),
    leaf_u256 => U256::from(123_456_789_u64),
    leaf_irys_peer_id => test_peer_id(0xDD),
    leaf_irys_signature => test_signature(),
    leaf_peer_address => test_peer_address(),
    leaf_protocol_version_v1 => ProtocolVersion::V1,
    leaf_protocol_version_v2 => ProtocolVersion::V2,

    // Standalone ExecutionPayload (RethBlock)
    execution_payload => fixture_execution_payload(),

    // NodeInfo
    node_info => NodeInfo {
        version: "1.2.3".to_string(),
        peer_count: 5,
        chain_id: 1270,
        height: 42,
        block_hash: test_h256(0xBB),
        block_index_height: 40,
        block_index_hash: test_h256(0xBC),
        pending_blocks: 2,
        is_syncing: false,
        current_sync_height: 42,
        uptime_secs: 3600,
        mining_address: test_address(0xAA),
        cumulative_difficulty: U256::from(50_000_u64),
    },
}

// =============================================================================
// Test: Generate fixtures (run with --nocapture to see JSON output)
// =============================================================================

/// Helper test to generate/regenerate the gossip_fixtures.json file.
/// Run with: `cargo test -p irys-p2p generate_fixture_json -- --ignored`
#[test]
#[ignore = "writes fixtures/gossip_fixtures.json — run explicitly to regenerate"]
fn generate_fixture_json() {
    let map: serde_json::Map<String, serde_json::Value> = all_fixture_entries()
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();

    let json = serde_json::to_string_pretty(&serde_json::Value::Object(map)).unwrap();
    let path = fixture_path();
    std::fs::write(&path, format!("{json}\n")).unwrap();
    println!("Wrote fixtures to {}", path.display());
}

// =============================================================================
// Canonical-type deserialization against pinned fixtures
//
// The client deserializes responses using canonical irys_types (not wire types).
// These tests verify that canonical types can deserialize the fixture JSON
// produced by wire types. If an irys_types serde change breaks this, these
// tests catch it independently of the parity tests.
// =============================================================================

/// Asserts that the fixture JSON for `fixture_name` can be deserialized into
/// the canonical type `T`.
fn assert_canonical_deserializes_fixture<T: serde::de::DeserializeOwned + std::fmt::Debug>(
    fixture_name: &str,
) {
    let fixtures = FIXTURES.get_or_init(load_fixtures);
    let fixture_value = fixtures
        .get(fixture_name)
        .unwrap_or_else(|| panic!("Fixture '{fixture_name}' not found in gossip_fixtures.json"));

    let result: Result<T, _> = serde_json::from_value(fixture_value.clone());
    assert!(
        result.is_ok(),
        "Canonical type {} failed to deserialize fixture '{fixture_name}':\n  error: {}\n  \
         fixture JSON: {}",
        std::any::type_name::<T>(),
        result.unwrap_err(),
        serde_json::to_string_pretty(fixture_value).unwrap(),
    );
}

#[test]
fn test_canonical_deserializes_handshake_response_v1() {
    assert_canonical_deserializes_fixture::<GossipResponse<irys_types::HandshakeResponseV1>>(
        "gossip_response_accepted_handshake_v1",
    );
}

#[test]
fn test_canonical_deserializes_handshake_response_v2() {
    assert_canonical_deserializes_fixture::<GossipResponse<irys_types::HandshakeResponseV2>>(
        "gossip_response_accepted_handshake_v2",
    );
}

#[test]
fn test_canonical_deserializes_gossip_data_v1() {
    assert_canonical_deserializes_fixture::<
        GossipResponse<Option<irys_types::gossip::v1::GossipDataV1>>,
    >("gossip_response_accepted_gossip_data_v1");
}

#[test]
fn test_canonical_deserializes_gossip_data_v2() {
    assert_canonical_deserializes_fixture::<
        GossipResponse<Option<irys_types::gossip::v2::GossipDataV2>>,
    >("gossip_response_accepted_gossip_data_v2");
}

#[test]
fn test_canonical_deserializes_gossip_data_v1_none() {
    assert_canonical_deserializes_fixture::<
        GossipResponse<Option<irys_types::gossip::v1::GossipDataV1>>,
    >("gossip_response_accepted_gossip_data_v1_none");
}

#[test]
fn test_canonical_deserializes_gossip_data_v2_none() {
    assert_canonical_deserializes_fixture::<
        GossipResponse<Option<irys_types::gossip::v2::GossipDataV2>>,
    >("gossip_response_accepted_gossip_data_v2_none");
}

#[test]
fn test_canonical_deserializes_execution_payload() {
    // The client deserializes ExecutionPayload using RethBlock directly
    assert_canonical_deserializes_fixture::<RethBlock>("execution_payload");
}

#[test]
fn test_canonical_deserializes_v1_execution_payload_variant() {
    assert_canonical_deserializes_fixture::<wire::GossipDataV1>("v1_gossip_data_execution_payload");
}

#[test]
fn test_canonical_deserializes_v2_execution_payload_variant() {
    assert_canonical_deserializes_fixture::<wire::GossipDataV2>("v2_gossip_data_execution_payload");
}
