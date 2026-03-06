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
    block::{IrysBlockHeader, IrysBlockHeaderV1, PoaData, VDFLimiterInfo},
    chunk::UnpackedChunk,
    commitment_common::{
        CommitmentTransaction, CommitmentV1WithMetadata, CommitmentV2WithMetadata,
    },
    commitment_v1::{CommitmentTransactionV1, CommitmentTypeV1},
    commitment_v2::{CommitmentTransactionV2, CommitmentTypeV2},
    ingress::{IngressProof, IngressProofV1},
    serialization::{Base64, H256List},
    storage_pricing::Amount,
    transaction::{
        DataTransactionHeader, DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata,
        DataTransactionMetadata,
    },
    version::{NodeInfo, PeerAddress, ProtocolVersion},
    CommitmentTransactionMetadata, DataTransactionLedger, IrysAddress, IrysPeerId, IrysSignature,
    RethPeerInfo, Signature, SystemTransactionLedger, TxChunkOffset, UnixTimestampMs, H256, U256,
};
use reth::revm::primitives::B256;
use semver::Version;
use serde::{de::DeserializeOwned, Serialize};

use crate::wire_types as wire;

// =============================================================================
// Deterministic test value constructors
// =============================================================================

fn test_h256(byte: u8) -> H256 {
    H256::from([byte; 32])
}

fn test_address(byte: u8) -> IrysAddress {
    IrysAddress::from_slice(&[byte; 20])
}

fn test_peer_id(byte: u8) -> IrysPeerId {
    IrysPeerId::from(test_address(byte))
}

fn test_signature() -> IrysSignature {
    // Use a deterministic signature with known r, s, v values
    IrysSignature::new(Signature::new(
        reth::revm::primitives::U256::from(1_u64),
        reth::revm::primitives::U256::from(2_u64),
        false,
    ))
}

fn test_base64(bytes: &[u8]) -> Base64 {
    Base64(bytes.to_vec())
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

fn test_unix_timestamp() -> UnixTimestampMs {
    UnixTimestampMs::from(1_700_000_000_000_u128)
}

fn fixture_unpacked_chunk() -> wire::UnpackedChunk {
    let canonical = UnpackedChunk {
        data_root: test_h256(0xAA),
        data_size: 262144,
        data_path: test_base64(&[0xDE, 0xAD, 0xBE, 0xEF]),
        bytes: test_base64(&[0xCA, 0xFE, 0xBA, 0xBE]),
        tx_offset: TxChunkOffset(0),
    };
    (&canonical).into()
}

fn fixture_data_tx_header_v1() -> DataTransactionHeaderV1 {
    DataTransactionHeaderV1 {
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
    }
}

fn fixture_data_tx_header() -> wire::DataTransactionHeader {
    let canonical = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
        tx: fixture_data_tx_header_v1(),
        metadata: DataTransactionMetadata::new(),
    });
    (&canonical).into()
}

fn fixture_commitment_v1_stake() -> wire::CommitmentTransaction {
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
    (&canonical).into()
}

fn fixture_commitment_v1_pledge() -> wire::CommitmentTransaction {
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
    (&canonical).into()
}

fn fixture_commitment_v1_unpledge() -> wire::CommitmentTransaction {
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
    (&canonical).into()
}

fn fixture_commitment_v1_unstake() -> wire::CommitmentTransaction {
    let canonical = CommitmentTransaction::V1(CommitmentV1WithMetadata {
        tx: CommitmentTransactionV1 {
            id: test_h256(0x40),
            anchor: test_h256(0x41),
            signer: test_address(0x42),
            commitment_type: CommitmentTypeV1::Unstake,
            chain_id: 1270,
            fee: 1000,
            value: U256::from(100_000_u64),
            signature: test_signature(),
        },
        metadata: CommitmentTransactionMetadata::new(),
    });
    (&canonical).into()
}

fn fixture_commitment_v2_stake() -> wire::CommitmentTransaction {
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
    (&canonical).into()
}

fn fixture_commitment_v2_pledge() -> wire::CommitmentTransaction {
    let canonical = CommitmentTransaction::V2(CommitmentV2WithMetadata {
        tx: CommitmentTransactionV2 {
            id: test_h256(0x60),
            anchor: test_h256(0x61),
            signer: test_address(0x62),
            commitment_type: CommitmentTypeV2::Pledge {
                pledge_count_before_executing: 7,
            },
            chain_id: 1270,
            fee: 4000,
            value: U256::from(800_000_u64),
            signature: test_signature(),
        },
        metadata: CommitmentTransactionMetadata::new(),
    });
    (&canonical).into()
}

fn fixture_commitment_v2_unpledge() -> wire::CommitmentTransaction {
    let canonical = CommitmentTransaction::V2(CommitmentV2WithMetadata {
        tx: CommitmentTransactionV2 {
            id: test_h256(0x70),
            anchor: test_h256(0x71),
            signer: test_address(0x72),
            commitment_type: CommitmentTypeV2::Unpledge {
                pledge_count_before_executing: 2,
                partition_hash: test_h256(0x73),
            },
            chain_id: 1270,
            fee: 3000,
            value: U256::from(400_000_u64),
            signature: test_signature(),
        },
        metadata: CommitmentTransactionMetadata::new(),
    });
    (&canonical).into()
}

fn fixture_commitment_v2_unstake() -> wire::CommitmentTransaction {
    let canonical = CommitmentTransaction::V2(CommitmentV2WithMetadata {
        tx: CommitmentTransactionV2 {
            id: test_h256(0x80),
            anchor: test_h256(0x81),
            signer: test_address(0x82),
            commitment_type: CommitmentTypeV2::Unstake,
            chain_id: 1270,
            fee: 2000,
            value: U256::from(300_000_u64),
            signature: test_signature(),
        },
        metadata: CommitmentTransactionMetadata::new(),
    });
    (&canonical).into()
}

fn fixture_commitment_v2_update_reward_address() -> wire::CommitmentTransaction {
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
    (&canonical).into()
}

fn fixture_block_header() -> wire::IrysBlockHeader {
    let canonical = IrysBlockHeader::V1(IrysBlockHeaderV1 {
        block_hash: test_h256(0xBB),
        signature: test_signature(),
        height: 42,
        diff: U256::from(1000_u64),
        cumulative_diff: U256::from(50_000_u64),
        solution_hash: test_h256(0xCC),
        last_diff_timestamp: test_unix_timestamp(),
        previous_solution_hash: test_h256(0xCD),
        last_epoch_hash: test_h256(0xCE),
        chunk_hash: test_h256(0xCF),
        previous_block_hash: test_h256(0xD0),
        previous_cumulative_diff: U256::from(49_000_u64),
        poa: PoaData {
            partition_chunk_offset: 100,
            partition_hash: test_h256(0xD1),
            chunk: Some(test_base64(&[0x01, 0x02, 0x03])),
            ledger_id: Some(1),
            tx_path: Some(test_base64(&[0x04, 0x05])),
            data_path: Some(test_base64(&[0x06, 0x07])),
        },
        reward_address: test_address(0xE0),
        reward_amount: U256::from(100_u64),
        miner_address: test_address(0xE1),
        timestamp: test_unix_timestamp(),
        system_ledgers: vec![SystemTransactionLedger {
            ledger_id: 0,
            tx_ids: H256List(vec![test_h256(0xF0)]),
        }],
        data_ledgers: vec![DataTransactionLedger {
            ledger_id: 1,
            tx_root: test_h256(0xF1),
            tx_ids: H256List(vec![test_h256(0xF2)]),
            total_chunks: 256,
            expires: Some(100),
            proofs: None,
            required_proof_count: None,
        }],
        evm_block_hash: B256::from([0xF3; 32]),
        vdf_limiter_info: VDFLimiterInfo {
            output: test_h256(0xA0),
            global_step_number: 1000,
            seed: test_h256(0xA1),
            next_seed: test_h256(0xA2),
            prev_output: test_h256(0xA3),
            last_step_checkpoints: H256List(vec![test_h256(0xA4)]),
            steps: H256List(vec![test_h256(0xA5)]),
            vdf_difficulty: Some(50000),
            next_vdf_difficulty: Some(55000),
        },
        oracle_irys_price: Amount::new(U256::from(100_u64)),
        ema_irys_price: Amount::new(U256::from(95_u64)),
        treasury: U256::from(999_999_u64),
    });
    (&canonical).into()
}

fn fixture_ingress_proof() -> wire::IngressProof {
    let canonical = IngressProof::V1(IngressProofV1 {
        signature: test_signature(),
        data_root: test_h256(0xDD),
        proof: test_h256(0xDE),
        chain_id: 1270,
        anchor: test_h256(0xDF),
    });
    (&canonical).into()
}

fn fixture_block_body() -> wire::BlockBody {
    wire::BlockBody {
        block_hash: test_h256(0xBB),
        data_transactions: vec![fixture_data_tx_header()],
        commitment_transactions: vec![fixture_commitment_v2_stake()],
    }
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
        wire::GossipResponse::<()>::Accepted(()),
    gossip_response_rejected_handshake_required_none =>
        wire::GossipResponse::<()>::Rejected(wire::RejectionReason::HandshakeRequired(None)),
    gossip_response_rejected_handshake_required_not_in_peer_list =>
        wire::GossipResponse::<()>::Rejected(wire::RejectionReason::HandshakeRequired(Some(
            wire::HandshakeRequirementReason::RequestOriginIsNotInThePeerList,
        ))),
    gossip_response_rejected_handshake_required_origin_mismatch =>
        wire::GossipResponse::<()>::Rejected(wire::RejectionReason::HandshakeRequired(Some(
            wire::HandshakeRequirementReason::RequestOriginDoesNotMatchExpected,
        ))),
    gossip_response_rejected_handshake_required_unknown_miner =>
        wire::GossipResponse::<()>::Rejected(wire::RejectionReason::HandshakeRequired(Some(
            wire::HandshakeRequirementReason::MinerAddressIsUnknown,
        ))),
    gossip_response_rejected_gossip_disabled =>
        wire::GossipResponse::<()>::Rejected(wire::RejectionReason::GossipDisabled),
    gossip_response_rejected_invalid_data =>
        wire::GossipResponse::<()>::Rejected(wire::RejectionReason::InvalidData),
    gossip_response_rejected_rate_limited =>
        wire::GossipResponse::<()>::Rejected(wire::RejectionReason::RateLimited),
    gossip_response_rejected_unable_to_verify_origin =>
        wire::GossipResponse::<()>::Rejected(wire::RejectionReason::UnableToVerifyOrigin),
    gossip_response_rejected_invalid_credentials =>
        wire::GossipResponse::<()>::Rejected(wire::RejectionReason::InvalidCredentials),
    gossip_response_rejected_protocol_mismatch =>
        wire::GossipResponse::<()>::Rejected(wire::RejectionReason::ProtocolMismatch),
    gossip_response_rejected_unsupported_protocol_version =>
        wire::GossipResponse::<()>::Rejected(wire::RejectionReason::UnsupportedProtocolVersion(99)),
    gossip_response_rejected_unsupported_feature =>
        wire::GossipResponse::<()>::Rejected(wire::RejectionReason::UnsupportedFeature),

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
