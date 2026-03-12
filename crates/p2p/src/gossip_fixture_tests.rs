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

use std::sync::OnceLock;

use irys_types::{
    block::{BlockIndexQuery, DataLedger},
    version::{NodeInfo, PeerAddress, ProtocolVersion},
    IrysPeerId, U256,
};
use reth::revm::primitives::B256;
use reth_ethereum_primitives::Block as RethBlock;
use semver::Version;
use serde::{de::DeserializeOwned, Serialize};

use crate::wire_types::{
    self as wire, test_helpers::*, GossipResponse, HandshakeRequirementReason, RejectionReason,
};

// =============================================================================
// Additional test value constructors (not shared — specific to fixture tests)
// =============================================================================

fn test_peer_id(byte: u8) -> IrysPeerId {
    IrysPeerId::from(test_address(byte))
}

fn test_peer_address() -> PeerAddress {
    crate::wire_types::test_helpers::test_peer_address(0x01)
}

fn fixture_unpacked_chunk() -> wire::UnpackedChunk {
    canonical_unpacked_chunk().into()
}

fn fixture_data_tx_header() -> wire::DataTransactionHeader {
    canonical_data_tx_header().into()
}

fn fixture_commitment_v1_stake() -> wire::CommitmentTransaction {
    canonical_commitment_v1_stake().into()
}

fn fixture_commitment_v1_pledge() -> wire::CommitmentTransaction {
    canonical_commitment_v1_pledge().into()
}

fn fixture_commitment_v1_unpledge() -> wire::CommitmentTransaction {
    canonical_commitment_v1_unpledge().into()
}

fn fixture_commitment_v1_unstake() -> wire::CommitmentTransaction {
    canonical_commitment_v1_unstake().into()
}

fn fixture_commitment_v2_stake() -> wire::CommitmentTransaction {
    canonical_commitment_v2_stake().into()
}

fn fixture_commitment_v2_pledge() -> wire::CommitmentTransaction {
    canonical_commitment_v2_pledge().into()
}

fn fixture_commitment_v2_unpledge() -> wire::CommitmentTransaction {
    canonical_commitment_v2_unpledge().into()
}

fn fixture_commitment_v2_unstake() -> wire::CommitmentTransaction {
    canonical_commitment_v2_unstake().into()
}

fn fixture_commitment_v2_update_reward_address() -> wire::CommitmentTransaction {
    canonical_commitment_v2_update_reward_address().into()
}

fn fixture_block_header() -> wire::IrysBlockHeader {
    canonical_block_header().into()
}

fn fixture_ingress_proof() -> wire::IngressProof {
    canonical_ingress_proof().into()
}

fn fixture_block_body() -> wire::BlockBody {
    canonical_block_body().into()
}

// --- None-variant helpers (optional fields cleared) ---

/// DataTransactionHeader with bundle_format and perm_fee set to None.
fn fixture_data_tx_header_none() -> wire::DataTransactionHeader {
    let mut inner = canonical_data_tx_header_v1_inner();
    inner.bundle_format = None;
    inner.perm_fee = None;
    let canonical =
        irys_types::DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: inner,
            metadata: Default::default(),
        });
    canonical.into()
}

/// IrysBlockHeader with all nested optional fields set to None:
/// PoaData.{chunk, ledger_id, tx_path, data_path},
/// VDFLimiterInfo.{vdf_difficulty, next_vdf_difficulty},
/// DataTransactionLedger.expires.
fn fixture_block_header_none() -> wire::IrysBlockHeader {
    use irys_types::block::{IrysBlockHeaderV1, PoaData, VDFLimiterInfo};
    use irys_types::serialization::H256List;
    use irys_types::{DataTransactionLedger, SystemTransactionLedger};

    let header = irys_types::IrysBlockHeader::V1(IrysBlockHeaderV1 {
        block_hash: test_h256(0xBB),
        signature: test_signature(),
        height: 42,
        diff: U256::from(1000_u64),
        cumulative_diff: U256::from(50_000_u64),
        solution_hash: test_h256(0xCC),
        last_diff_timestamp: irys_types::UnixTimestampMs::from(1_700_000_000_000_u128),
        previous_solution_hash: test_h256(0xCD),
        last_epoch_hash: test_h256(0xCE),
        chunk_hash: test_h256(0xCF),
        previous_block_hash: test_h256(0xD0),
        previous_cumulative_diff: U256::from(49_000_u64),
        poa: PoaData {
            partition_chunk_offset: 100,
            partition_hash: test_h256(0xD1),
            chunk: None,
            ledger_id: None,
            tx_path: None,
            data_path: None,
        },
        reward_address: test_address(0xE0),
        reward_amount: U256::from(100_u64),
        miner_address: test_address(0xE1),
        timestamp: irys_types::UnixTimestampMs::from(1_700_000_000_000_u128),
        system_ledgers: vec![SystemTransactionLedger {
            ledger_id: 0,
            tx_ids: H256List(vec![test_h256(0xF0)]),
        }],
        data_ledgers: vec![DataTransactionLedger {
            ledger_id: 1,
            tx_root: test_h256(0xF1),
            tx_ids: H256List(vec![test_h256(0xF2)]),
            total_chunks: 256,
            expires: None,
            proofs: None,
            required_proof_count: None,
        }],
        evm_block_hash: reth::revm::primitives::B256::from([0xF3; 32]),
        vdf_limiter_info: VDFLimiterInfo {
            output: test_h256(0xA0),
            global_step_number: 1000,
            seed: test_h256(0xA1),
            next_seed: test_h256(0xA2),
            prev_output: test_h256(0xA3),
            last_step_checkpoints: H256List(vec![test_h256(0xA4)]),
            steps: H256List(vec![test_h256(0xA5)]),
            vdf_difficulty: None,
            next_vdf_difficulty: None,
        },
        oracle_irys_price: irys_types::storage_pricing::Amount::new(U256::from(100_u64)),
        ema_irys_price: irys_types::storage_pricing::Amount::new(U256::from(95_u64)),
        treasury: U256::from(999_999_u64),
    });
    header.into()
}

/// BlockBody with data transactions that have optional fields set to None.
fn fixture_block_body_none() -> wire::BlockBody {
    wire::BlockBody {
        block_hash: test_h256(0xBB),
        data_transactions: vec![fixture_data_tx_header_none()],
        commitment_transactions: vec![fixture_commitment_v2_stake()],
    }
}

fn fixture_execution_payload() -> RethBlock {
    canonical_execution_payload()
}

fn fixture_node_info() -> wire::NodeInfoV1 {
    canonical_node_info().into()
}

fn fixture_node_info_v2() -> wire::NodeInfoV2 {
    canonical_node_info().into()
}

fn fixture_block_index_item_v1() -> wire::BlockIndexItemV1 {
    canonical_block_index_item().into()
}

fn fixture_block_index_item() -> wire::BlockIndexItemV2 {
    canonical_block_index_item().into()
}

fn fixture_irys_tx_response_storage() -> wire::IrysTransactionResponse {
    canonical_irys_tx_response_storage().into()
}

fn fixture_irys_tx_response_commitment() -> wire::IrysTransactionResponse {
    canonical_irys_tx_response_commitment().into()
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
    v1_gossip_data_transaction_none =>
        wire::GossipDataV1::Transaction(fixture_data_tx_header_none()),
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
    v1_gossip_data_block_none =>
        wire::GossipDataV1::Block(fixture_block_header_none()),
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
    v2_gossip_data_transaction_none =>
        wire::GossipDataV2::Transaction(fixture_data_tx_header_none()),
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
    v2_gossip_data_block_header_none =>
        wire::GossipDataV2::BlockHeader(fixture_block_header_none()),
    v2_gossip_data_block_body =>
        wire::GossipDataV2::BlockBody(fixture_block_body()),
    v2_gossip_data_block_body_none =>
        wire::GossipDataV2::BlockBody(fixture_block_body_none()),
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
        address: test_peer_address().into(),
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
        address: test_peer_address().into(),
        timestamp: 1700000000000,
        user_agent: Some("irys-node/1.2.3".to_string()),
        consensus_config_hash: test_h256(0xFF),
        signature: test_signature(),
    },
    handshake_response_v1 => wire::HandshakeResponseV1 {
        version: Version::new(1, 2, 3),
        protocol_version: ProtocolVersion::V1,
        peers: vec![test_peer_address().into()],
        timestamp: 1700000000000,
        message: Some("Welcome".to_string()),
    },
    handshake_response_v2 => wire::HandshakeResponseV2 {
        version: Version::new(1, 2, 3),
        protocol_version: ProtocolVersion::V2,
        peers: vec![test_peer_address().into()],
        timestamp: 1700000000000,
        message: Some("Welcome".to_string()),
        consensus_config_hash: test_h256(0xFF),
    },

    // Handshakes with optional fields set to None
    handshake_request_v1_none => wire::HandshakeRequestV1 {
        version: Version::new(1, 2, 3),
        protocol_version: ProtocolVersion::V1,
        mining_address: test_address(0xAA),
        chain_id: 1270,
        address: test_peer_address().into(),
        timestamp: 1700000000000,
        user_agent: None,
        signature: test_signature(),
    },
    handshake_request_v2_none => wire::HandshakeRequestV2 {
        version: Version::new(1, 2, 3),
        protocol_version: ProtocolVersion::V2,
        mining_address: test_address(0xAA),
        peer_id: test_peer_id(0xBB),
        chain_id: 1270,
        address: test_peer_address().into(),
        timestamp: 1700000000000,
        user_agent: None,
        consensus_config_hash: test_h256(0xFF),
        signature: test_signature(),
    },
    handshake_response_v1_none => wire::HandshakeResponseV1 {
        version: Version::new(1, 2, 3),
        protocol_version: ProtocolVersion::V1,
        peers: vec![test_peer_address().into()],
        timestamp: 1700000000000,
        message: None,
    },
    handshake_response_v2_none => wire::HandshakeResponseV2 {
        version: Version::new(1, 2, 3),
        protocol_version: ProtocolVersion::V2,
        peers: vec![test_peer_address().into()],
        timestamp: 1700000000000,
        message: None,
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
            peers: vec![test_peer_address().into()],
            timestamp: 1700000000000,
            message: Some("Welcome".to_string()),
        }),
    gossip_response_accepted_handshake_v2 =>
        GossipResponse::Accepted(wire::HandshakeResponseV2 {
            version: Version::new(1, 2, 3),
            protocol_version: ProtocolVersion::V2,
            peers: vec![test_peer_address().into()],
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

    // NodeInfo (canonical — backwards compat) and wire type
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
    wire_node_info => fixture_node_info(),
    wire_node_info_v2 => fixture_node_info_v2(),

    // BlockIndexItem / LedgerIndexItem
    block_index_item_v1 => fixture_block_index_item_v1(),
    block_index_item => fixture_block_index_item(),
    block_index_query => BlockIndexQuery { height: 100, limit: 50 },

    // DataLedger leaf variants
    leaf_data_ledger_publish => DataLedger::Publish,
    leaf_data_ledger_submit => DataLedger::Submit,

    // IrysTransactionResponse variants
    irys_transaction_response_storage => fixture_irys_tx_response_storage(),
    irys_transaction_response_commitment => fixture_irys_tx_response_commitment(),

    // GossipResponse envelope variants for GET endpoints
    gossip_response_accepted_node_info =>
        GossipResponse::Accepted(fixture_node_info()),
    gossip_response_accepted_peer_list =>
        GossipResponse::Accepted(vec![test_peer_address()]),
    gossip_response_accepted_block_index_v1 =>
        GossipResponse::Accepted(vec![fixture_block_index_item_v1()]),
    gossip_response_accepted_block_index =>
        GossipResponse::Accepted(vec![fixture_block_index_item()]),
    gossip_response_accepted_whitelist =>
        GossipResponse::Accepted(vec![test_address(0xAA), test_address(0xBB)]),

    // Protocol versions (returned by /protocol_version as bare Vec<u32>)
    protocol_versions => ProtocolVersion::supported_versions_u32().to_vec(),
}

// =============================================================================
// Exhaustive fixture coverage
//
// These checks enforce — at compile time — that every variant of each wire
// enum has a corresponding entry in gossip_fixtures.json.
//
// HOW IT WORKS: each variant is mapped to its fixture name exactly once via a
// `pattern => "fixture_name"` arm.  A `const` block compiles an exhaustive
// match over those patterns — if you add a variant without a mapping, the
// compiler errors.  The same fixture names drive a runtime assertion that the
// fixture file actually contains them.
//
// There is no separate `values` list — the patterns and fixture names are the
// single source of truth, eliminating any possibility of the two diverging.
//
// WHEN YOU ADD A NEW VARIANT:
//   1. Add a fixture_tests! entry for it (with a golden JSON value).
//   2. Add a `Pattern => "fixture_name"` arm in the matching block below.
//   3. Regenerate fixtures: cargo test -p irys-p2p generate_fixture_json -- --ignored
// =============================================================================

/// Compile-time + runtime enforcement that every enum variant has a fixture.
///
/// The `const` block contains an exhaustive match over `$enum_ty` — this is
/// the compile-time mechanism.  The `#[test]` collects fixture names from the
/// same arms and verifies they exist in `gossip_fixtures.json`.
macro_rules! assert_fixture_coverage {
    (
        $test_name:ident, $enum_ty:ty,
        $( $pat:pat => $fixture_name:expr ),+ $(,)?
    ) => {
        // Compile-time: exhaustive match ensures every variant maps to a fixture.
        const _: () = {
            fn _exhaustive_check(v: &$enum_ty) -> &'static str {
                match v { $( $pat => $fixture_name ),+ }
            }
            _ = _exhaustive_check;
        };

        // Runtime: verify each fixture name actually exists in the golden file.
        #[test]
        fn $test_name() {
            let fixtures = FIXTURES.get_or_init(load_fixtures);
            for name in [ $($fixture_name),+ ] {
                assert!(
                    fixtures.contains_key(name),
                    "Missing fixture for {} variant: '{name}'. \
                     Add a fixture_tests! entry and regenerate fixtures.",
                    stringify!($enum_ty),
                );
            }
        }
    };
}

assert_fixture_coverage!(
    test_all_gossip_response_variants_covered,
    GossipResponse<()>,
    GossipResponse::Accepted(_) => "gossip_response_accepted",
    GossipResponse::Rejected(_) => "gossip_response_rejected_gossip_disabled",
);

assert_fixture_coverage!(
    test_all_rejection_reasons_covered,
    RejectionReason,
    RejectionReason::HandshakeRequired(None) =>
        "gossip_response_rejected_handshake_required_none",
    RejectionReason::HandshakeRequired(Some(
        HandshakeRequirementReason::RequestOriginIsNotInThePeerList
    )) => "gossip_response_rejected_handshake_required_not_in_peer_list",
    RejectionReason::HandshakeRequired(Some(
        HandshakeRequirementReason::RequestOriginDoesNotMatchExpected
    )) => "gossip_response_rejected_handshake_required_origin_mismatch",
    RejectionReason::HandshakeRequired(Some(
        HandshakeRequirementReason::MinerAddressIsUnknown
    )) => "gossip_response_rejected_handshake_required_unknown_miner",
    RejectionReason::GossipDisabled => "gossip_response_rejected_gossip_disabled",
    RejectionReason::InvalidData => "gossip_response_rejected_invalid_data",
    RejectionReason::RateLimited => "gossip_response_rejected_rate_limited",
    RejectionReason::UnableToVerifyOrigin =>
        "gossip_response_rejected_unable_to_verify_origin",
    RejectionReason::InvalidCredentials =>
        "gossip_response_rejected_invalid_credentials",
    RejectionReason::ProtocolMismatch => "gossip_response_rejected_protocol_mismatch",
    RejectionReason::UnsupportedProtocolVersion(_) =>
        "gossip_response_rejected_unsupported_protocol_version",
    RejectionReason::UnsupportedFeature =>
        "gossip_response_rejected_unsupported_feature",
);

assert_fixture_coverage!(
    test_all_gossip_data_v1_variants_covered,
    wire::GossipDataV1,
    wire::GossipDataV1::Chunk(_) => "v1_gossip_data_chunk",
    wire::GossipDataV1::Transaction(_) => "v1_gossip_data_transaction",
    wire::GossipDataV1::CommitmentTransaction(_) => "v1_gossip_data_commitment_stake",
    wire::GossipDataV1::Block(_) => "v1_gossip_data_block",
    wire::GossipDataV1::ExecutionPayload(_) => "v1_gossip_data_execution_payload",
    wire::GossipDataV1::IngressProof(_) => "v1_gossip_data_ingress_proof",
);

assert_fixture_coverage!(
    test_all_gossip_data_v2_variants_covered,
    wire::GossipDataV2,
    wire::GossipDataV2::Chunk(_) => "v2_gossip_data_chunk",
    wire::GossipDataV2::Transaction(_) => "v2_gossip_data_transaction",
    wire::GossipDataV2::CommitmentTransaction(_) => "v2_gossip_data_commitment_v2_stake",
    wire::GossipDataV2::BlockHeader(_) => "v2_gossip_data_block_header",
    wire::GossipDataV2::BlockBody(_) => "v2_gossip_data_block_body",
    wire::GossipDataV2::ExecutionPayload(_) => "v2_gossip_data_execution_payload",
    wire::GossipDataV2::IngressProof(_) => "v2_gossip_data_ingress_proof",
);

assert_fixture_coverage!(
    test_all_gossip_data_request_v1_variants_covered,
    wire::GossipDataRequestV1,
    wire::GossipDataRequestV1::ExecutionPayload(_) => "v1_data_request_execution_payload",
    wire::GossipDataRequestV1::Block(_) => "v1_data_request_block",
    wire::GossipDataRequestV1::Chunk(_) => "v1_data_request_chunk",
    wire::GossipDataRequestV1::Transaction(_) => "v1_data_request_transaction",
);

assert_fixture_coverage!(
    test_all_gossip_data_request_v2_variants_covered,
    wire::GossipDataRequestV2,
    wire::GossipDataRequestV2::ExecutionPayload(_) => "v2_data_request_execution_payload",
    wire::GossipDataRequestV2::BlockHeader(_) => "v2_data_request_block_header",
    wire::GossipDataRequestV2::BlockBody(_) => "v2_data_request_block_body",
    wire::GossipDataRequestV2::Chunk(_) => "v2_data_request_chunk",
    wire::GossipDataRequestV2::Transaction(_) => "v2_data_request_transaction",
);

assert_fixture_coverage!(
    test_all_commitment_type_v1_variants_covered,
    wire::CommitmentTypeV1,
    wire::CommitmentTypeV1::Stake => "v1_gossip_data_commitment_stake",
    wire::CommitmentTypeV1::Pledge { .. } => "v1_gossip_data_commitment_pledge",
    wire::CommitmentTypeV1::Unpledge { .. } => "v1_gossip_data_commitment_unpledge",
    wire::CommitmentTypeV1::Unstake => "v1_gossip_data_commitment_unstake",
);

assert_fixture_coverage!(
    test_all_commitment_type_v2_variants_covered,
    wire::CommitmentTypeV2,
    wire::CommitmentTypeV2::Stake => "v2_gossip_data_commitment_v2_stake",
    wire::CommitmentTypeV2::Pledge { .. } => "v2_gossip_data_commitment_v2_pledge",
    wire::CommitmentTypeV2::Unpledge { .. } => "v2_gossip_data_commitment_v2_unpledge",
    wire::CommitmentTypeV2::Unstake => "v2_gossip_data_commitment_v2_unstake",
    wire::CommitmentTypeV2::UpdateRewardAddress { .. } =>
        "v2_gossip_data_commitment_v2_update_reward_address",
);

assert_fixture_coverage!(
    test_all_irys_transaction_response_variants_covered,
    wire::IrysTransactionResponse,
    wire::IrysTransactionResponse::Storage(_) => "irys_transaction_response_storage",
    wire::IrysTransactionResponse::Commitment(_) => "irys_transaction_response_commitment",
);

// =============================================================================
// Wire type coverage enforcement
//
// Parses every `wire_types/*.rs` source file with `syn`, extracts all public
// struct/enum names, and asserts each one appears somewhere in the test
// sources (this file, tests.rs, or test_helpers.rs).  If a new wire type is
// added without any test coverage, this test fails with instructions.
// =============================================================================

/// Verifies that every public struct/enum in `wire_types/` appears in the
/// test sources, catching newly added wire types that lack test coverage.
///
/// Check whether `word` appears in `haystack` as a whole identifier
/// (not as a substring of a longer alphanumeric/underscore token).
fn contains_word(haystack: &str, word: &str) -> bool {
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let bytes = haystack.as_bytes();
    let wlen = word.len();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(word) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_ident(bytes[abs - 1]);
        let after_ok = abs + wlen >= bytes.len() || !is_ident(bytes[abs + wlen]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

/// If this test fails, either:
/// 1. Add a `fixture_tests!` entry and/or roundtrip test for the new type.
/// 2. If the type is an inner struct tested via its wrapper enum, add it to
///    `EXCLUDED` below with a reason.
#[test]
fn all_wire_types_have_fixture_coverage() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let wire_types_dir = manifest_dir.join("src/wire_types");

    // Collect test source text to check type name references against.
    let test_sources: String = [
        manifest_dir.join("src/gossip_fixture_tests.rs"),
        wire_types_dir.join("tests.rs"),
        wire_types_dir.join("test_helpers.rs"),
    ]
    .iter()
    .map(|p| std::fs::read_to_string(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())))
    .collect::<Vec<_>>()
    .join("\n");

    // Types intentionally excluded from direct fixture coverage.
    // Inner types are always tested through their version-tagged wrapper enum;
    // the fixture snapshot will break if their serialization changes.
    const EXCLUDED: &[(&str, &str)] = &[
        (
            "BlockIndexItemV2ConversionError",
            "error type, not a wire message",
        ),
        (
            "IrysBlockHeaderV1Inner",
            "tested via IrysBlockHeader version-tagged enum",
        ),
        (
            "CommitmentTransactionV1Inner",
            "tested via CommitmentTransaction version-tagged enum",
        ),
        (
            "CommitmentTransactionV2Inner",
            "tested via CommitmentTransaction version-tagged enum",
        ),
        (
            "DataTransactionHeaderV1Inner",
            "tested via DataTransactionHeader version-tagged enum",
        ),
        (
            "IngressProofV1Inner",
            "tested via IngressProof version-tagged enum",
        ),
    ];
    let excluded_names: std::collections::HashSet<&str> =
        EXCLUDED.iter().map(|(name, _)| *name).collect();

    // Parse all wire type source files (skip mod.rs, tests.rs, test_helpers.rs).
    let mut all_source_types = std::collections::HashSet::new();
    let mut missing = Vec::new();

    for entry in std::fs::read_dir(&wire_types_dir).expect("read wire_types dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let filename = path.file_name().unwrap().to_str().unwrap().to_string();

        if !filename.ends_with(".rs")
            || filename == "mod.rs"
            || filename == "tests.rs"
            || filename == "test_helpers.rs"
        {
            continue;
        }

        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let file =
            syn::parse_file(&content).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));

        for item in &file.items {
            let name = match item {
                syn::Item::Struct(s) if matches!(s.vis, syn::Visibility::Public(_)) => {
                    s.ident.to_string()
                }
                syn::Item::Enum(e) if matches!(e.vis, syn::Visibility::Public(_)) => {
                    e.ident.to_string()
                }
                _ => continue,
            };

            all_source_types.insert(name.clone());

            if excluded_names.contains(name.as_str()) {
                continue;
            }

            if !contains_word(&test_sources, &name) {
                missing.push(format!("  {name} (in wire_types/{filename})"));
            }
        }
    }

    // Verify excluded types actually exist in source (catch stale entries).
    for (name, _reason) in EXCLUDED {
        assert!(
            all_source_types.contains(*name),
            "Stale EXCLUDED entry: '{name}' no longer exists in wire_types/. Remove it.",
        );
    }

    missing.sort();
    assert!(
        missing.is_empty(),
        "\n\nWire types missing from fixture/roundtrip tests:\n{}\n\n\
         To fix:\n\
         1. Add a fixture_tests! entry and/or roundtrip test for each type.\n\
         2. If the type is tested via a wrapper, add it to EXCLUDED with a reason.\n\
         3. Regenerate fixtures: cargo test -p irys-p2p generate_fixture_json -- --ignored\n",
        missing.join("\n"),
    );
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
