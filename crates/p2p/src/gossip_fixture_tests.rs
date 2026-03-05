//! Gossip protocol JSON serialization fixture tests.
//!
//! These tests verify that the JSON serialization format of all gossip protocol
//! messages remains stable across code changes. Each test constructs a message
//! with deterministic values, serializes it to JSON, and asserts the output
//! matches a hardcoded fixture string.
//!
//! If any test fails, it means a gossip protocol message format has changed,
//! which could break network compatibility between nodes running different
//! versions.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use irys_types::{
    block::{BlockBody, IrysBlockHeader, IrysBlockHeaderV1, PoaData, VDFLimiterInfo},
    chunk::UnpackedChunk,
    commitment_common::{
        CommitmentTransaction, CommitmentV1WithMetadata, CommitmentV2WithMetadata,
    },
    commitment_v1::{CommitmentTransactionV1, CommitmentTypeV1},
    commitment_v2::{CommitmentTransactionV2, CommitmentTypeV2},
    gossip::{
        v1::{GossipDataRequestV1, GossipDataV1},
        v2::{GossipDataRequestV2, GossipDataV2},
        GossipRequestV1, GossipRequestV2,
    },
    ingress::{IngressProof, IngressProofV1},
    serialization::{Base64, H256List},
    storage_pricing::Amount,
    transaction::{
        DataTransactionHeader, DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata,
        DataTransactionMetadata,
    },
    version::{
        HandshakeRequestV1, HandshakeRequestV2, HandshakeResponseV1, HandshakeResponseV2, NodeInfo,
        PeerAddress, ProtocolVersion,
    },
    CommitmentTransactionMetadata, DataTransactionLedger, IrysAddress, IrysPeerId, IrysSignature,
    RethPeerInfo, Signature, SystemTransactionLedger, TxChunkOffset, UnixTimestampMs, H256, U256,
};
use reth::revm::primitives::B256;
use semver::Version;
use serde::{de::DeserializeOwned, Serialize};

use crate::types::{GossipResponse, HandshakeRequirementReason, RejectionReason};

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

fn fixture_unpacked_chunk() -> UnpackedChunk {
    UnpackedChunk {
        data_root: test_h256(0xAA),
        data_size: 262144,
        data_path: test_base64(&[0xDE, 0xAD, 0xBE, 0xEF]),
        bytes: test_base64(&[0xCA, 0xFE, 0xBA, 0xBE]),
        tx_offset: TxChunkOffset(0),
    }
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

fn fixture_data_tx_header() -> DataTransactionHeader {
    DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
        tx: fixture_data_tx_header_v1(),
        metadata: DataTransactionMetadata::new(),
    })
}

fn fixture_commitment_v1_stake() -> CommitmentTransaction {
    CommitmentTransaction::V1(CommitmentV1WithMetadata {
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
    })
}

fn fixture_commitment_v1_pledge() -> CommitmentTransaction {
    CommitmentTransaction::V1(CommitmentV1WithMetadata {
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
    })
}

fn fixture_commitment_v1_unpledge() -> CommitmentTransaction {
    CommitmentTransaction::V1(CommitmentV1WithMetadata {
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
    })
}

fn fixture_commitment_v1_unstake() -> CommitmentTransaction {
    CommitmentTransaction::V1(CommitmentV1WithMetadata {
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
    })
}

fn fixture_commitment_v2_stake() -> CommitmentTransaction {
    CommitmentTransaction::V2(CommitmentV2WithMetadata {
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
    })
}

fn fixture_commitment_v2_pledge() -> CommitmentTransaction {
    CommitmentTransaction::V2(CommitmentV2WithMetadata {
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
    })
}

fn fixture_commitment_v2_unpledge() -> CommitmentTransaction {
    CommitmentTransaction::V2(CommitmentV2WithMetadata {
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
    })
}

fn fixture_commitment_v2_unstake() -> CommitmentTransaction {
    CommitmentTransaction::V2(CommitmentV2WithMetadata {
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
    })
}

fn fixture_commitment_v2_update_reward_address() -> CommitmentTransaction {
    CommitmentTransaction::V2(CommitmentV2WithMetadata {
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
    })
}

fn fixture_block_header() -> IrysBlockHeader {
    IrysBlockHeader::V1(IrysBlockHeaderV1 {
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
    })
}

fn fixture_ingress_proof() -> IngressProof {
    IngressProof::V1(IngressProofV1 {
        signature: test_signature(),
        data_root: test_h256(0xDD),
        proof: test_h256(0xDE),
        chain_id: 1270,
        anchor: test_h256(0xDF),
    })
}

fn fixture_block_body() -> BlockBody {
    BlockBody {
        block_hash: test_h256(0xBB),
        data_transactions: vec![fixture_data_tx_header()],
        commitment_transactions: vec![fixture_commitment_v2_stake()],
    }
}

// =============================================================================
// Fixture assertion helper
// =============================================================================

/// Asserts that the JSON serialization of a value matches the expected fixture
/// string exactly, and that the value can be deserialized back from the fixture.
fn assert_json_fixture<T: Serialize + DeserializeOwned + std::fmt::Debug>(
    value: &T,
    expected_json: &str,
    fixture_name: &str,
) {
    let serialized = serde_json::to_string_pretty(value).unwrap();
    assert_eq!(
        serialized, expected_json,
        "\n\nFixture mismatch for '{fixture_name}'!\n\
         The JSON serialization format has changed.\n\
         This may break gossip protocol compatibility.\n\n\
         === ACTUAL ===\n{serialized}\n\n\
         === EXPECTED ===\n{expected_json}\n"
    );

    // Verify roundtrip deserialization produces identical JSON
    let deserialized: T = serde_json::from_str(&serialized).unwrap();
    let re_serialized = serde_json::to_string_pretty(&deserialized).unwrap();
    assert_eq!(
        serialized, re_serialized,
        "JSON roundtrip failed for '{fixture_name}' - \
         deserialization produced different output"
    );
}

// =============================================================================
// Test: Generate fixtures (run with --nocapture to see JSON output)
// =============================================================================

/// Helper test to generate/regenerate fixture JSON strings.
/// Run with: `cargo test -p irys-p2p generate_fixture_json -- --nocapture`
///
/// Copy the output into the corresponding test constants below.
#[test]
fn generate_fixture_json() {
    let fixtures: Vec<(&str, String)> = vec![
        // V1 Gossip Data variants
        (
            "v1_gossip_data_chunk",
            serde_json::to_string_pretty(&GossipDataV1::Chunk(fixture_unpacked_chunk())).unwrap(),
        ),
        (
            "v1_gossip_data_transaction",
            serde_json::to_string_pretty(&GossipDataV1::Transaction(fixture_data_tx_header()))
                .unwrap(),
        ),
        (
            "v1_gossip_data_commitment_stake",
            serde_json::to_string_pretty(&GossipDataV1::CommitmentTransaction(
                fixture_commitment_v1_stake(),
            ))
            .unwrap(),
        ),
        (
            "v1_gossip_data_commitment_pledge",
            serde_json::to_string_pretty(&GossipDataV1::CommitmentTransaction(
                fixture_commitment_v1_pledge(),
            ))
            .unwrap(),
        ),
        (
            "v1_gossip_data_commitment_unpledge",
            serde_json::to_string_pretty(&GossipDataV1::CommitmentTransaction(
                fixture_commitment_v1_unpledge(),
            ))
            .unwrap(),
        ),
        (
            "v1_gossip_data_commitment_unstake",
            serde_json::to_string_pretty(&GossipDataV1::CommitmentTransaction(
                fixture_commitment_v1_unstake(),
            ))
            .unwrap(),
        ),
        (
            "v1_gossip_data_block",
            serde_json::to_string_pretty(&GossipDataV1::Block(Arc::new(fixture_block_header())))
                .unwrap(),
        ),
        (
            "v1_gossip_data_ingress_proof",
            serde_json::to_string_pretty(&GossipDataV1::IngressProof(fixture_ingress_proof()))
                .unwrap(),
        ),
        // V1 Data Requests
        (
            "v1_data_request_execution_payload",
            serde_json::to_string_pretty(&GossipDataRequestV1::ExecutionPayload(B256::from(
                [0xEE; 32],
            )))
            .unwrap(),
        ),
        (
            "v1_data_request_block",
            serde_json::to_string_pretty(&GossipDataRequestV1::Block(test_h256(0xBB))).unwrap(),
        ),
        (
            "v1_data_request_chunk",
            serde_json::to_string_pretty(&GossipDataRequestV1::Chunk(test_h256(0xCC))).unwrap(),
        ),
        (
            "v1_data_request_transaction",
            serde_json::to_string_pretty(&GossipDataRequestV1::Transaction(test_h256(0xDD)))
                .unwrap(),
        ),
        // V1 Request Wrapper
        (
            "v1_gossip_request",
            serde_json::to_string_pretty(&GossipRequestV1 {
                miner_address: test_address(0xAA),
                data: GossipDataV1::Chunk(fixture_unpacked_chunk()),
            })
            .unwrap(),
        ),
        // V2 Gossip Data variants
        (
            "v2_gossip_data_chunk",
            serde_json::to_string_pretty(&GossipDataV2::Chunk(Arc::new(fixture_unpacked_chunk())))
                .unwrap(),
        ),
        (
            "v2_gossip_data_transaction",
            serde_json::to_string_pretty(&GossipDataV2::Transaction(fixture_data_tx_header()))
                .unwrap(),
        ),
        (
            "v2_gossip_data_commitment_v2_stake",
            serde_json::to_string_pretty(&GossipDataV2::CommitmentTransaction(
                fixture_commitment_v2_stake(),
            ))
            .unwrap(),
        ),
        (
            "v2_gossip_data_commitment_v2_pledge",
            serde_json::to_string_pretty(&GossipDataV2::CommitmentTransaction(
                fixture_commitment_v2_pledge(),
            ))
            .unwrap(),
        ),
        (
            "v2_gossip_data_commitment_v2_unpledge",
            serde_json::to_string_pretty(&GossipDataV2::CommitmentTransaction(
                fixture_commitment_v2_unpledge(),
            ))
            .unwrap(),
        ),
        (
            "v2_gossip_data_commitment_v2_unstake",
            serde_json::to_string_pretty(&GossipDataV2::CommitmentTransaction(
                fixture_commitment_v2_unstake(),
            ))
            .unwrap(),
        ),
        (
            "v2_gossip_data_commitment_v2_update_reward_address",
            serde_json::to_string_pretty(&GossipDataV2::CommitmentTransaction(
                fixture_commitment_v2_update_reward_address(),
            ))
            .unwrap(),
        ),
        (
            "v2_gossip_data_block_header",
            serde_json::to_string_pretty(&GossipDataV2::BlockHeader(Arc::new(
                fixture_block_header(),
            )))
            .unwrap(),
        ),
        (
            "v2_gossip_data_block_body",
            serde_json::to_string_pretty(&GossipDataV2::BlockBody(Arc::new(fixture_block_body())))
                .unwrap(),
        ),
        (
            "v2_gossip_data_ingress_proof",
            serde_json::to_string_pretty(&GossipDataV2::IngressProof(fixture_ingress_proof()))
                .unwrap(),
        ),
        // V2 Data Requests
        (
            "v2_data_request_execution_payload",
            serde_json::to_string_pretty(&GossipDataRequestV2::ExecutionPayload(B256::from(
                [0xEE; 32],
            )))
            .unwrap(),
        ),
        (
            "v2_data_request_block_header",
            serde_json::to_string_pretty(&GossipDataRequestV2::BlockHeader(test_h256(0xBB)))
                .unwrap(),
        ),
        (
            "v2_data_request_block_body",
            serde_json::to_string_pretty(&GossipDataRequestV2::BlockBody(test_h256(0xBB))).unwrap(),
        ),
        (
            "v2_data_request_chunk",
            serde_json::to_string_pretty(&GossipDataRequestV2::Chunk(test_h256(0xCC))).unwrap(),
        ),
        (
            "v2_data_request_transaction",
            serde_json::to_string_pretty(&GossipDataRequestV2::Transaction(test_h256(0xDD)))
                .unwrap(),
        ),
        // V2 Request Wrapper
        (
            "v2_gossip_request",
            serde_json::to_string_pretty(&GossipRequestV2 {
                peer_id: test_peer_id(0xBB),
                miner_address: test_address(0xAA),
                data: GossipDataV2::Chunk(Arc::new(fixture_unpacked_chunk())),
            })
            .unwrap(),
        ),
        // Handshake V1
        (
            "handshake_request_v1",
            serde_json::to_string_pretty(&HandshakeRequestV1 {
                version: Version::new(1, 2, 3),
                protocol_version: ProtocolVersion::V1,
                mining_address: test_address(0xAA),
                chain_id: 1270,
                address: test_peer_address(),
                timestamp: 1700000000000,
                user_agent: Some("irys-node/1.2.3".to_string()),
                signature: test_signature(),
            })
            .unwrap(),
        ),
        (
            "handshake_request_v2",
            serde_json::to_string_pretty(&HandshakeRequestV2 {
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
            })
            .unwrap(),
        ),
        (
            "handshake_response_v1",
            serde_json::to_string_pretty(&HandshakeResponseV1 {
                version: Version::new(1, 2, 3),
                protocol_version: ProtocolVersion::V1,
                peers: vec![test_peer_address()],
                timestamp: 1700000000000,
                message: Some("Welcome".to_string()),
            })
            .unwrap(),
        ),
        (
            "handshake_response_v2",
            serde_json::to_string_pretty(&HandshakeResponseV2 {
                version: Version::new(1, 2, 3),
                protocol_version: ProtocolVersion::V2,
                peers: vec![test_peer_address()],
                timestamp: 1700000000000,
                message: Some("Welcome".to_string()),
                consensus_config_hash: test_h256(0xFF),
            })
            .unwrap(),
        ),
        // GossipResponse variants
        (
            "gossip_response_accepted",
            serde_json::to_string_pretty(&GossipResponse::<()>::Accepted(())).unwrap(),
        ),
        (
            "gossip_response_rejected_handshake_required_none",
            serde_json::to_string_pretty(&GossipResponse::<()>::Rejected(
                RejectionReason::HandshakeRequired(None),
            ))
            .unwrap(),
        ),
        (
            "gossip_response_rejected_handshake_required_not_in_peer_list",
            serde_json::to_string_pretty(&GossipResponse::<()>::Rejected(
                RejectionReason::HandshakeRequired(Some(
                    HandshakeRequirementReason::RequestOriginIsNotInThePeerList,
                )),
            ))
            .unwrap(),
        ),
        (
            "gossip_response_rejected_handshake_required_origin_mismatch",
            serde_json::to_string_pretty(&GossipResponse::<()>::Rejected(
                RejectionReason::HandshakeRequired(Some(
                    HandshakeRequirementReason::RequestOriginDoesNotMatchExpected,
                )),
            ))
            .unwrap(),
        ),
        (
            "gossip_response_rejected_handshake_required_unknown_miner",
            serde_json::to_string_pretty(&GossipResponse::<()>::Rejected(
                RejectionReason::HandshakeRequired(Some(
                    HandshakeRequirementReason::MinerAddressIsUnknown,
                )),
            ))
            .unwrap(),
        ),
        (
            "gossip_response_rejected_gossip_disabled",
            serde_json::to_string_pretty(&GossipResponse::<()>::Rejected(
                RejectionReason::GossipDisabled,
            ))
            .unwrap(),
        ),
        (
            "gossip_response_rejected_invalid_data",
            serde_json::to_string_pretty(&GossipResponse::<()>::Rejected(
                RejectionReason::InvalidData,
            ))
            .unwrap(),
        ),
        (
            "gossip_response_rejected_rate_limited",
            serde_json::to_string_pretty(&GossipResponse::<()>::Rejected(
                RejectionReason::RateLimited,
            ))
            .unwrap(),
        ),
        (
            "gossip_response_rejected_unable_to_verify_origin",
            serde_json::to_string_pretty(&GossipResponse::<()>::Rejected(
                RejectionReason::UnableToVerifyOrigin,
            ))
            .unwrap(),
        ),
        (
            "gossip_response_rejected_invalid_credentials",
            serde_json::to_string_pretty(&GossipResponse::<()>::Rejected(
                RejectionReason::InvalidCredentials,
            ))
            .unwrap(),
        ),
        (
            "gossip_response_rejected_protocol_mismatch",
            serde_json::to_string_pretty(&GossipResponse::<()>::Rejected(
                RejectionReason::ProtocolMismatch,
            ))
            .unwrap(),
        ),
        (
            "gossip_response_rejected_unsupported_protocol_version",
            serde_json::to_string_pretty(&GossipResponse::<()>::Rejected(
                RejectionReason::UnsupportedProtocolVersion(99),
            ))
            .unwrap(),
        ),
        (
            "gossip_response_rejected_unsupported_feature",
            serde_json::to_string_pretty(&GossipResponse::<()>::Rejected(
                RejectionReason::UnsupportedFeature,
            ))
            .unwrap(),
        ),
        // NodeInfo
        (
            "node_info",
            serde_json::to_string_pretty(&NodeInfo {
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
            })
            .unwrap(),
        ),
    ];

    for (name, json) in &fixtures {
        println!("=== {name} ===");
        println!("{json}");
        println!();
    }
}

// =============================================================================
// V1 Gossip Data fixture tests
// =============================================================================

#[test]
fn fixture_v1_gossip_data_chunk() {
    assert_json_fixture(
        &GossipDataV1::Chunk(fixture_unpacked_chunk()),
        FIXTURE_V1_GOSSIP_DATA_CHUNK,
        "v1_gossip_data_chunk",
    );
}

#[test]
fn fixture_v1_gossip_data_transaction() {
    assert_json_fixture(
        &GossipDataV1::Transaction(fixture_data_tx_header()),
        FIXTURE_V1_GOSSIP_DATA_TRANSACTION,
        "v1_gossip_data_transaction",
    );
}

#[test]
fn fixture_v1_gossip_data_commitment_stake() {
    assert_json_fixture(
        &GossipDataV1::CommitmentTransaction(fixture_commitment_v1_stake()),
        FIXTURE_V1_GOSSIP_DATA_COMMITMENT_STAKE,
        "v1_gossip_data_commitment_stake",
    );
}

#[test]
fn fixture_v1_gossip_data_commitment_pledge() {
    assert_json_fixture(
        &GossipDataV1::CommitmentTransaction(fixture_commitment_v1_pledge()),
        FIXTURE_V1_GOSSIP_DATA_COMMITMENT_PLEDGE,
        "v1_gossip_data_commitment_pledge",
    );
}

#[test]
fn fixture_v1_gossip_data_commitment_unpledge() {
    assert_json_fixture(
        &GossipDataV1::CommitmentTransaction(fixture_commitment_v1_unpledge()),
        FIXTURE_V1_GOSSIP_DATA_COMMITMENT_UNPLEDGE,
        "v1_gossip_data_commitment_unpledge",
    );
}

#[test]
fn fixture_v1_gossip_data_commitment_unstake() {
    assert_json_fixture(
        &GossipDataV1::CommitmentTransaction(fixture_commitment_v1_unstake()),
        FIXTURE_V1_GOSSIP_DATA_COMMITMENT_UNSTAKE,
        "v1_gossip_data_commitment_unstake",
    );
}

#[test]
fn fixture_v1_gossip_data_block() {
    assert_json_fixture(
        &GossipDataV1::Block(Arc::new(fixture_block_header())),
        FIXTURE_V1_GOSSIP_DATA_BLOCK,
        "v1_gossip_data_block",
    );
}

#[test]
fn fixture_v1_gossip_data_ingress_proof() {
    assert_json_fixture(
        &GossipDataV1::IngressProof(fixture_ingress_proof()),
        FIXTURE_V1_GOSSIP_DATA_INGRESS_PROOF,
        "v1_gossip_data_ingress_proof",
    );
}

// =============================================================================
// V1 Data Request fixture tests
// =============================================================================

#[test]
fn fixture_v1_data_request_execution_payload() {
    assert_json_fixture(
        &GossipDataRequestV1::ExecutionPayload(B256::from([0xEE; 32])),
        FIXTURE_V1_DATA_REQUEST_EXECUTION_PAYLOAD,
        "v1_data_request_execution_payload",
    );
}

#[test]
fn fixture_v1_data_request_block() {
    assert_json_fixture(
        &GossipDataRequestV1::Block(test_h256(0xBB)),
        FIXTURE_V1_DATA_REQUEST_BLOCK,
        "v1_data_request_block",
    );
}

#[test]
fn fixture_v1_data_request_chunk() {
    assert_json_fixture(
        &GossipDataRequestV1::Chunk(test_h256(0xCC)),
        FIXTURE_V1_DATA_REQUEST_CHUNK,
        "v1_data_request_chunk",
    );
}

#[test]
fn fixture_v1_data_request_transaction() {
    assert_json_fixture(
        &GossipDataRequestV1::Transaction(test_h256(0xDD)),
        FIXTURE_V1_DATA_REQUEST_TRANSACTION,
        "v1_data_request_transaction",
    );
}

// =============================================================================
// V1 Request Wrapper fixture test
// =============================================================================

#[test]
fn fixture_v1_gossip_request() {
    assert_json_fixture(
        &GossipRequestV1 {
            miner_address: test_address(0xAA),
            data: GossipDataV1::Chunk(fixture_unpacked_chunk()),
        },
        FIXTURE_V1_GOSSIP_REQUEST,
        "v1_gossip_request",
    );
}

// =============================================================================
// V2 Gossip Data fixture tests
// =============================================================================

#[test]
fn fixture_v2_gossip_data_chunk() {
    assert_json_fixture(
        &GossipDataV2::Chunk(Arc::new(fixture_unpacked_chunk())),
        FIXTURE_V2_GOSSIP_DATA_CHUNK,
        "v2_gossip_data_chunk",
    );
}

#[test]
fn fixture_v2_gossip_data_transaction() {
    assert_json_fixture(
        &GossipDataV2::Transaction(fixture_data_tx_header()),
        FIXTURE_V2_GOSSIP_DATA_TRANSACTION,
        "v2_gossip_data_transaction",
    );
}

#[test]
fn fixture_v2_gossip_data_commitment_v2_stake() {
    assert_json_fixture(
        &GossipDataV2::CommitmentTransaction(fixture_commitment_v2_stake()),
        FIXTURE_V2_GOSSIP_DATA_COMMITMENT_V2_STAKE,
        "v2_gossip_data_commitment_v2_stake",
    );
}

#[test]
fn fixture_v2_gossip_data_commitment_v2_pledge() {
    assert_json_fixture(
        &GossipDataV2::CommitmentTransaction(fixture_commitment_v2_pledge()),
        FIXTURE_V2_GOSSIP_DATA_COMMITMENT_V2_PLEDGE,
        "v2_gossip_data_commitment_v2_pledge",
    );
}

#[test]
fn fixture_v2_gossip_data_commitment_v2_unpledge() {
    assert_json_fixture(
        &GossipDataV2::CommitmentTransaction(fixture_commitment_v2_unpledge()),
        FIXTURE_V2_GOSSIP_DATA_COMMITMENT_V2_UNPLEDGE,
        "v2_gossip_data_commitment_v2_unpledge",
    );
}

#[test]
fn fixture_v2_gossip_data_commitment_v2_unstake() {
    assert_json_fixture(
        &GossipDataV2::CommitmentTransaction(fixture_commitment_v2_unstake()),
        FIXTURE_V2_GOSSIP_DATA_COMMITMENT_V2_UNSTAKE,
        "v2_gossip_data_commitment_v2_unstake",
    );
}

#[test]
fn fixture_v2_gossip_data_commitment_v2_update_reward_address() {
    assert_json_fixture(
        &GossipDataV2::CommitmentTransaction(fixture_commitment_v2_update_reward_address()),
        FIXTURE_V2_GOSSIP_DATA_COMMITMENT_V2_UPDATE_REWARD_ADDRESS,
        "v2_gossip_data_commitment_v2_update_reward_address",
    );
}

#[test]
fn fixture_v2_gossip_data_block_header() {
    assert_json_fixture(
        &GossipDataV2::BlockHeader(Arc::new(fixture_block_header())),
        FIXTURE_V2_GOSSIP_DATA_BLOCK_HEADER,
        "v2_gossip_data_block_header",
    );
}

#[test]
fn fixture_v2_gossip_data_block_body() {
    assert_json_fixture(
        &GossipDataV2::BlockBody(Arc::new(fixture_block_body())),
        FIXTURE_V2_GOSSIP_DATA_BLOCK_BODY,
        "v2_gossip_data_block_body",
    );
}

#[test]
fn fixture_v2_gossip_data_ingress_proof() {
    assert_json_fixture(
        &GossipDataV2::IngressProof(fixture_ingress_proof()),
        FIXTURE_V2_GOSSIP_DATA_INGRESS_PROOF,
        "v2_gossip_data_ingress_proof",
    );
}

// =============================================================================
// V2 Data Request fixture tests
// =============================================================================

#[test]
fn fixture_v2_data_request_execution_payload() {
    assert_json_fixture(
        &GossipDataRequestV2::ExecutionPayload(B256::from([0xEE; 32])),
        FIXTURE_V2_DATA_REQUEST_EXECUTION_PAYLOAD,
        "v2_data_request_execution_payload",
    );
}

#[test]
fn fixture_v2_data_request_block_header() {
    assert_json_fixture(
        &GossipDataRequestV2::BlockHeader(test_h256(0xBB)),
        FIXTURE_V2_DATA_REQUEST_BLOCK_HEADER,
        "v2_data_request_block_header",
    );
}

#[test]
fn fixture_v2_data_request_block_body() {
    assert_json_fixture(
        &GossipDataRequestV2::BlockBody(test_h256(0xBB)),
        FIXTURE_V2_DATA_REQUEST_BLOCK_BODY,
        "v2_data_request_block_body",
    );
}

#[test]
fn fixture_v2_data_request_chunk() {
    assert_json_fixture(
        &GossipDataRequestV2::Chunk(test_h256(0xCC)),
        FIXTURE_V2_DATA_REQUEST_CHUNK,
        "v2_data_request_chunk",
    );
}

#[test]
fn fixture_v2_data_request_transaction() {
    assert_json_fixture(
        &GossipDataRequestV2::Transaction(test_h256(0xDD)),
        FIXTURE_V2_DATA_REQUEST_TRANSACTION,
        "v2_data_request_transaction",
    );
}

// =============================================================================
// V2 Request Wrapper fixture test
// =============================================================================

#[test]
fn fixture_v2_gossip_request() {
    assert_json_fixture(
        &GossipRequestV2 {
            peer_id: test_peer_id(0xBB),
            miner_address: test_address(0xAA),
            data: GossipDataV2::Chunk(Arc::new(fixture_unpacked_chunk())),
        },
        FIXTURE_V2_GOSSIP_REQUEST,
        "v2_gossip_request",
    );
}

// =============================================================================
// Handshake fixture tests
// =============================================================================

#[test]
fn fixture_handshake_request_v1() {
    assert_json_fixture(
        &HandshakeRequestV1 {
            version: Version::new(1, 2, 3),
            protocol_version: ProtocolVersion::V1,
            mining_address: test_address(0xAA),
            chain_id: 1270,
            address: test_peer_address(),
            timestamp: 1700000000000,
            user_agent: Some("irys-node/1.2.3".to_string()),
            signature: test_signature(),
        },
        FIXTURE_HANDSHAKE_REQUEST_V1,
        "handshake_request_v1",
    );
}

#[test]
fn fixture_handshake_request_v2() {
    assert_json_fixture(
        &HandshakeRequestV2 {
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
        FIXTURE_HANDSHAKE_REQUEST_V2,
        "handshake_request_v2",
    );
}

#[test]
fn fixture_handshake_response_v1() {
    assert_json_fixture(
        &HandshakeResponseV1 {
            version: Version::new(1, 2, 3),
            protocol_version: ProtocolVersion::V1,
            peers: vec![test_peer_address()],
            timestamp: 1700000000000,
            message: Some("Welcome".to_string()),
        },
        FIXTURE_HANDSHAKE_RESPONSE_V1,
        "handshake_response_v1",
    );
}

#[test]
fn fixture_handshake_response_v2() {
    assert_json_fixture(
        &HandshakeResponseV2 {
            version: Version::new(1, 2, 3),
            protocol_version: ProtocolVersion::V2,
            peers: vec![test_peer_address()],
            timestamp: 1700000000000,
            message: Some("Welcome".to_string()),
            consensus_config_hash: test_h256(0xFF),
        },
        FIXTURE_HANDSHAKE_RESPONSE_V2,
        "handshake_response_v2",
    );
}

// =============================================================================
// GossipResponse fixture tests
// =============================================================================

#[test]
fn fixture_gossip_response_accepted() {
    assert_json_fixture(
        &GossipResponse::<()>::Accepted(()),
        FIXTURE_GOSSIP_RESPONSE_ACCEPTED,
        "gossip_response_accepted",
    );
}

#[test]
fn fixture_gossip_response_rejected_handshake_required_none() {
    assert_json_fixture(
        &GossipResponse::<()>::Rejected(RejectionReason::HandshakeRequired(None)),
        FIXTURE_GOSSIP_RESPONSE_REJECTED_HANDSHAKE_REQUIRED_NONE,
        "gossip_response_rejected_handshake_required_none",
    );
}

#[test]
fn fixture_gossip_response_rejected_handshake_required_not_in_peer_list() {
    assert_json_fixture(
        &GossipResponse::<()>::Rejected(RejectionReason::HandshakeRequired(Some(
            HandshakeRequirementReason::RequestOriginIsNotInThePeerList,
        ))),
        FIXTURE_GOSSIP_RESPONSE_REJECTED_HANDSHAKE_REQUIRED_NOT_IN_PEER_LIST,
        "gossip_response_rejected_handshake_required_not_in_peer_list",
    );
}

#[test]
fn fixture_gossip_response_rejected_handshake_required_origin_mismatch() {
    assert_json_fixture(
        &GossipResponse::<()>::Rejected(RejectionReason::HandshakeRequired(Some(
            HandshakeRequirementReason::RequestOriginDoesNotMatchExpected,
        ))),
        FIXTURE_GOSSIP_RESPONSE_REJECTED_HANDSHAKE_REQUIRED_ORIGIN_MISMATCH,
        "gossip_response_rejected_handshake_required_origin_mismatch",
    );
}

#[test]
fn fixture_gossip_response_rejected_handshake_required_unknown_miner() {
    assert_json_fixture(
        &GossipResponse::<()>::Rejected(RejectionReason::HandshakeRequired(Some(
            HandshakeRequirementReason::MinerAddressIsUnknown,
        ))),
        FIXTURE_GOSSIP_RESPONSE_REJECTED_HANDSHAKE_REQUIRED_UNKNOWN_MINER,
        "gossip_response_rejected_handshake_required_unknown_miner",
    );
}

#[test]
fn fixture_gossip_response_rejected_gossip_disabled() {
    assert_json_fixture(
        &GossipResponse::<()>::Rejected(RejectionReason::GossipDisabled),
        FIXTURE_GOSSIP_RESPONSE_REJECTED_GOSSIP_DISABLED,
        "gossip_response_rejected_gossip_disabled",
    );
}

#[test]
fn fixture_gossip_response_rejected_invalid_data() {
    assert_json_fixture(
        &GossipResponse::<()>::Rejected(RejectionReason::InvalidData),
        FIXTURE_GOSSIP_RESPONSE_REJECTED_INVALID_DATA,
        "gossip_response_rejected_invalid_data",
    );
}

#[test]
fn fixture_gossip_response_rejected_rate_limited() {
    assert_json_fixture(
        &GossipResponse::<()>::Rejected(RejectionReason::RateLimited),
        FIXTURE_GOSSIP_RESPONSE_REJECTED_RATE_LIMITED,
        "gossip_response_rejected_rate_limited",
    );
}

#[test]
fn fixture_gossip_response_rejected_unable_to_verify_origin() {
    assert_json_fixture(
        &GossipResponse::<()>::Rejected(RejectionReason::UnableToVerifyOrigin),
        FIXTURE_GOSSIP_RESPONSE_REJECTED_UNABLE_TO_VERIFY_ORIGIN,
        "gossip_response_rejected_unable_to_verify_origin",
    );
}

#[test]
fn fixture_gossip_response_rejected_invalid_credentials() {
    assert_json_fixture(
        &GossipResponse::<()>::Rejected(RejectionReason::InvalidCredentials),
        FIXTURE_GOSSIP_RESPONSE_REJECTED_INVALID_CREDENTIALS,
        "gossip_response_rejected_invalid_credentials",
    );
}

#[test]
fn fixture_gossip_response_rejected_protocol_mismatch() {
    assert_json_fixture(
        &GossipResponse::<()>::Rejected(RejectionReason::ProtocolMismatch),
        FIXTURE_GOSSIP_RESPONSE_REJECTED_PROTOCOL_MISMATCH,
        "gossip_response_rejected_protocol_mismatch",
    );
}

#[test]
fn fixture_gossip_response_rejected_unsupported_protocol_version() {
    assert_json_fixture(
        &GossipResponse::<()>::Rejected(RejectionReason::UnsupportedProtocolVersion(99)),
        FIXTURE_GOSSIP_RESPONSE_REJECTED_UNSUPPORTED_PROTOCOL_VERSION,
        "gossip_response_rejected_unsupported_protocol_version",
    );
}

#[test]
fn fixture_gossip_response_rejected_unsupported_feature() {
    assert_json_fixture(
        &GossipResponse::<()>::Rejected(RejectionReason::UnsupportedFeature),
        FIXTURE_GOSSIP_RESPONSE_REJECTED_UNSUPPORTED_FEATURE,
        "gossip_response_rejected_unsupported_feature",
    );
}

// =============================================================================
// NodeInfo fixture test
// =============================================================================

#[test]
fn fixture_node_info() {
    assert_json_fixture(
        &NodeInfo {
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
        FIXTURE_NODE_INFO,
        "node_info",
    );
}

// =============================================================================
// Hardcoded JSON fixture constants
// =============================================================================
// These constants are the canonical JSON serialization for each gossip protocol
// message. If any of these need to be updated, it means the wire format has
// changed and protocol compatibility must be reviewed.
//
// To regenerate: `cargo test -p irys-p2p generate_fixture_json -- --nocapture`

const FIXTURE_V1_GOSSIP_DATA_CHUNK: &str = r#"{
  "Chunk": {
    "dataRoot": "CVDFLCAjXhVWiPXH9nTCTpCgVzmDVoiPzNJYuccr1dqB",
    "dataSize": "262144",
    "dataPath": "3q2-7w",
    "bytes": "yv66vg",
    "txOffset": 0
  }
}"#;

const FIXTURE_V1_GOSSIP_DATA_TRANSACTION: &str = r#"{
  "Transaction": {
    "version": 1,
    "anchor": "8qbHbw2BbbTHBW1sbeqakYXVKRQM8Ne7pLK7m6CVfeR",
    "bundleFormat": "1",
    "chainId": "1270",
    "dataRoot": "GgBaCs3NCBuZN12kCJgAW63ydqohFkHEdfdEXBPzLHq",
    "dataSize": "1048576",
    "headerSize": "256",
    "id": "4vJ9JU1bJJE96FWSJKvHsmmFADCg4gpZQff4P3bkLKi",
    "ledgerId": 1,
    "permFee": "500000",
    "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv",
    "signer": "3S9hEB66TqZ8Ubncq1FwwuVH4dp",
    "termFee": "1000000"
  }
}"#;

const FIXTURE_V1_GOSSIP_DATA_COMMITMENT_STAKE: &str = r#"{
  "CommitmentTransaction": {
    "version": 1,
    "anchor": "29d2S7vB453rNYFdR5Ycwt7y9haRT5fwVwL9zTmBhfV2",
    "chainId": "1270",
    "commitmentType": {
      "type": "stake"
    },
    "fee": "5000",
    "id": "25hjHpTATmkdET17ynDhf1MCuYNDn1z7wXfVw5iaxLAK",
    "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv",
    "signer": "FbuAN3XZn2Kmrbihy2YggRvfNos",
    "value": "1000000"
  }
}"#;

const FIXTURE_V1_GOSSIP_DATA_COMMITMENT_PLEDGE: &str = r#"{
  "CommitmentTransaction": {
    "version": 1,
    "anchor": "3EKkiwNLWqoUbzFkPrmKbtUB4EweE6f4STzevYUmezeL",
    "chainId": "1270",
    "commitmentType": {
      "pledgeCountBeforeExecuting": "5",
      "type": "pledge"
    },
    "fee": "3000",
    "id": "3AQTaduKvYWFTu1ExZSQK1hQp5jSZ2yEt4KzsASAufKd",
    "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv",
    "signer": "UahXaJfQF9wo4Tv13ib54bC93XX",
    "value": "500000"
  }
}"#;

const FIXTURE_V1_GOSSIP_DATA_COMMITMENT_UNPLEDGE: &str = r#"{
  "CommitmentTransaction": {
    "version": 1,
    "anchor": "4K2V1kpVycZ6qSFsNdz2FtpNxnJs17eBNzf9rdCMcKoe",
    "chainId": "1270",
    "commitmentType": {
      "partitionHash": "4Ss5JMkXAD9Z7cktFEdrqeMuT6jGMF1pVozTyPHZ6zT4",
      "pledgeCountBeforeExecuting": "3",
      "type": "unpledge"
    },
    "fee": "2000",
    "id": "4F7BsTMVPKFshM1MwLf6y23cid6fL3xMpazVoF9krzUw",
    "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv",
    "signer": "hZVtnZoEiHZpGL7J8QdTSkTciFB",
    "value": "250000"
  }
}"#;

const FIXTURE_V1_GOSSIP_DATA_COMMITMENT_UNSTAKE: &str = r#"{
  "CommitmentTransaction": {
    "version": 1,
    "anchor": "5PjDJaGfSPJj4tFzMRCiuuAasKg5n8dJKXKenhuwZexx",
    "chainId": "1270",
    "commitmentType": {
      "type": "unstake"
    },
    "fee": "1000",
    "id": "5KovAGoer61Vvo1Uv7sod2PpdATt74wUm7ezjKsLpKeF",
    "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv",
    "signer": "vYJFzpw5BRBqUCJbD6fqpuj6Nxq",
    "value": "100000"
  }
}"#;

const FIXTURE_V1_GOSSIP_DATA_BLOCK: &str = r#"{
  "Block": {
    "version": 1,
    "blockHash": "DdqGmK5uamYN5vmuZrzpQhKeehLdwtPLVJdhu5P2iJKC",
    "chunkHash": "EzDBdLQ7QjUtsjn4HqXCECkuXsYfRA7jfTxq4gH1exmL",
    "cumulativeDiff": "50000",
    "dataLedgers": [
      {
        "expires": "100",
        "ledgerId": 1,
        "totalChunks": "256",
        "txIds": [
          "HMNXdshU7AspkuXpZHwMQqmc5RuhzP9SDkHo6yqyod45"
        ],
        "txRoot": "HHTEVaETWsabcpHK7zcS7xzqqGhWKKTcfLd93boP4HjN"
      }
    ],
    "diff": "1000",
    "emaIrysPrice": {
      "amount": "95"
    },
    "evmBlockHash": "0xf3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3",
    "height": "42",
    "lastDiffTimestamp": "1700000000000",
    "lastEpochHash": "EvHtV2w6pSBfjeXYrYCGwKz9HiLTk6Rv74JB1JEQudSd",
    "minerAddress": "49XF233b4hnYfgyzL5LL3zsZmhbn",
    "oracleIrysPrice": {
      "amount": "100"
    },
    "poa": {
      "chunk": "AQID",
      "dataPath": "Bgc",
      "ledgerId": 1,
      "partitionChunkOffset": 100,
      "partitionHash": "F83muwL8bL5M9vH5ASB2oxJS2By4mHVNnHJ9BSND9dQk",
      "txPath": "BAU"
    },
    "previousBlockHash": "F48Umds812n81q2Zj8r7X5Xfn2ks6DoZDsdV84KcQJ63",
    "previousCumulativeDiff": "49000",
    "previousSolutionHash": "ErNbLjU6E8tSbZH3REsMeTDP3Z8G52k6YedWwvBpAJ7v",
    "rewardAddress": "48iC7xethYqhHs7j88faQMEjgM4B",
    "rewardAmount": "100",
    "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv",
    "solutionHash": "EnTJCS15dqbDTU2XywYSMaScoPv4Py4GzExrtY9DQxoD",
    "systemLedgers": [
      {
        "ledgerId": 0,
        "txIds": [
          "HDXwMGmSvaHNUj2oghHWq6E5b7VJeFmo6vxUzDknJxQf"
        ]
      }
    ],
    "timestamp": "1700000000000",
    "treasury": "999999",
    "vdfLimiterInfo": {
      "globalStepNumber": "1000",
      "lastStepCheckpoints": [
        "C5hUUPNfyui9sr2EXzVgiZa733W1TRbUdvJcZLMFXdtw"
      ],
      "nextSeed": "BwrtBnSeoK7hbfXDfPqr8p2aYj5c7JDqX6yJSaG42yFX",
      "nextVdfDifficulty": "55000",
      "output": "Bp2HuBWdciXFKV2CnoC1Z4V44QfCmArCQHdzKpArYJc7",
      "prevOutput": "C1nBL5ufPcQvjkmj6hAmRgoLntHonMuf5WdxVxJenJaE",
      "seed": "Bswb3UyeD1pUTaGiE6WvqwFpJZsQSEY1xhJePCDTHdvp",
      "steps": [
        "C9cmcgqgaD1P1wGjyHpc1SLsHCiD8VHJCKyGciPrGyDe"
      ],
      "vdfDifficulty": "50000"
    }
  }
}"#;

const FIXTURE_V1_GOSSIP_DATA_INGRESS_PROOF: &str = r#"{
  "IngressProof": {
    "version": 1,
    "anchor": "G4uuv9rGsWEX7BnBGcjttD77SQutCB6rbzdKzkzbcHve",
    "chain_id": 1270,
    "data_root": "Fw5KdYvFgue4q1HAQ264JTZax6VUr3jDVBJ1szuQ7dHE",
    "proof": "FzzcmrPGHCwHy6XfqKQybLLMCFhgX7R33axfwNwzrxbw",
    "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv"
  }
}"#;

const FIXTURE_V1_DATA_REQUEST_EXECUTION_PAYLOAD: &str = r#"{
  "ExecutionPayload": "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
}"#;

const FIXTURE_V1_DATA_REQUEST_BLOCK: &str = r#"{
  "Block": "DdqGmK5uamYN5vmuZrzpQhKeehLdwtPLVJdhu5P2iJKC"
}"#;

const FIXTURE_V1_DATA_REQUEST_CHUNK: &str = r#"{
  "Chunk": "EnTJCS15dqbDTU2XywYSMaScoPv4Py4GzExrtY9DQxoD"
}"#;

const FIXTURE_V1_DATA_REQUEST_TRANSACTION: &str = r#"{
  "Transaction": "Fw5KdYvFgue4q1HAQ264JTZax6VUr3jDVBJ1szuQ7dHE"
}"#;

const FIXTURE_V1_GOSSIP_REQUEST: &str = r#"{
  "miner_address": "3NuVdsXK1DmiyJKa1EawMJwxhDdb",
  "data": {
    "Chunk": {
      "dataRoot": "CVDFLCAjXhVWiPXH9nTCTpCgVzmDVoiPzNJYuccr1dqB",
      "dataSize": "262144",
      "dataPath": "3q2-7w",
      "bytes": "yv66vg",
      "txOffset": 0
    }
  }
}"#;

const FIXTURE_V2_GOSSIP_DATA_CHUNK: &str = r#"{
  "Chunk": {
    "dataRoot": "CVDFLCAjXhVWiPXH9nTCTpCgVzmDVoiPzNJYuccr1dqB",
    "dataSize": "262144",
    "dataPath": "3q2-7w",
    "bytes": "yv66vg",
    "txOffset": 0
  }
}"#;

const FIXTURE_V2_GOSSIP_DATA_TRANSACTION: &str = r#"{
  "Transaction": {
    "version": 1,
    "anchor": "8qbHbw2BbbTHBW1sbeqakYXVKRQM8Ne7pLK7m6CVfeR",
    "bundleFormat": "1",
    "chainId": "1270",
    "dataRoot": "GgBaCs3NCBuZN12kCJgAW63ydqohFkHEdfdEXBPzLHq",
    "dataSize": "1048576",
    "headerSize": "256",
    "id": "4vJ9JU1bJJE96FWSJKvHsmmFADCg4gpZQff4P3bkLKi",
    "ledgerId": 1,
    "permFee": "500000",
    "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv",
    "signer": "3S9hEB66TqZ8Ubncq1FwwuVH4dp",
    "termFee": "1000000"
  }
}"#;

const FIXTURE_V2_GOSSIP_DATA_COMMITMENT_V2_STAKE: &str = r#"{
  "CommitmentTransaction": {
    "version": 2,
    "anchor": "6URwbPipuA4MJLG7LCRRZuWnms3JZ9cRG3z9indXWz8G",
    "chainId": "1270",
    "commitmentType": {
      "type": "stake"
    },
    "fee": "5000",
    "id": "6QWeT6FpJrm8AF1btu6WH2k2Xhq6t5vbheKVfQavmeoZ",
    "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv",
    "signer": "29X6dD64ueYorg4VtHniED4za3gV",
    "value": "2000000"
  }
}"#;

const FIXTURE_V2_GOSSIP_DATA_COMMITMENT_V2_PLEDGE: &str = r#"{
  "CommitmentTransaction": {
    "version": 2,
    "anchor": "7Z8ftDAzMvoyXnGEJye8DurzgQQXLAbYCaeeesM7UKHa",
    "chainId": "1270",
    "commitmentType": {
      "pledgeCountBeforeExecuting": "7",
      "type": "pledge"
    },
    "fee": "4000",
    "id": "7VDNjuhymdWkPh1isgKCw36ESFCKf6uieAyzbVJWiyxs",
    "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv",
    "signer": "2NVtzRMCk7gRssvhBNUkcbEG3iQ9",
    "value": "800000"
  }
}"#;

const FIXTURE_V2_GOSSIP_DATA_COMMITMENT_V2_UNPLEDGE: &str = r#"{
  "CommitmentTransaction": {
    "version": 2,
    "anchor": "8dqQB2d9phZbmEGMHkrpsvDCawmk7Baf97K9ax4hReSt",
    "chainId": "1270",
    "commitmentType": {
      "partitionHash": "8mfzTdZB1JA43QmNAMWfTfkj5GC9TJxJFveThi9tvK6J",
      "pledgeCountBeforeExecuting": "2",
      "type": "unpledge"
    },
    "fee": "3000",
    "id": "8Zv72jA9EQGNd91qrTXub3SSLnZYS7tqaheVXa26gK8B",
    "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv",
    "signer": "2bUhMdcLaap3u5ntUTAnzyPXXP7o",
    "value": "400000"
  }
}"#;

const FIXTURE_V2_GOSSIP_DATA_COMMITMENT_V2_UNSTAKE: &str = r#"{
  "CommitmentTransaction": {
    "version": 2,
    "anchor": "9iY8Tr5KHUKDzgGUGY5XXvZQVV8xtCZn5dyeX2nHNycC",
    "chainId": "1270",
    "commitmentType": {
      "type": "unstake"
    },
    "fee": "2000",
    "id": "9ecqKYcJhB1zrb1xqEkcF3neFKvmD8sxXEJzTejgdeHV",
    "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv",
    "signer": "2pTViqsUR3wfvHf5mXrqPMYo13qT",
    "value": "300000"
  }
}"#;

const FIXTURE_V2_GOSSIP_DATA_COMMITMENT_V2_UPDATE_REWARD_ADDRESS: &str = r#"{
  "CommitmentTransaction": {
    "version": 2,
    "anchor": "AoErkfXUkF4rE8GbFKJEBvucQ2WBfDYu2Ae9T7VsLJmW",
    "chainId": "1270",
    "commitmentType": {
      "newRewardAddress": "34FLz8XJcg29KKPYGZDdRPLta56i",
      "type": "updateRewardAddress"
    },
    "fee": "1500",
    "id": "AjKZcN4U9wmd6325p1yJu48r9sHyz9s5TkyVPjTGaySo",
    "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv",
    "signer": "33SJ648cFX5HwVXH4cYsmji4UiZ7",
    "value": "0"
  }
}"#;

const FIXTURE_V2_GOSSIP_DATA_BLOCK_HEADER: &str = r#"{
  "BlockHeader": {
    "version": 1,
    "blockHash": "DdqGmK5uamYN5vmuZrzpQhKeehLdwtPLVJdhu5P2iJKC",
    "chunkHash": "EzDBdLQ7QjUtsjn4HqXCECkuXsYfRA7jfTxq4gH1exmL",
    "cumulativeDiff": "50000",
    "dataLedgers": [
      {
        "expires": "100",
        "ledgerId": 1,
        "totalChunks": "256",
        "txIds": [
          "HMNXdshU7AspkuXpZHwMQqmc5RuhzP9SDkHo6yqyod45"
        ],
        "txRoot": "HHTEVaETWsabcpHK7zcS7xzqqGhWKKTcfLd93boP4HjN"
      }
    ],
    "diff": "1000",
    "emaIrysPrice": {
      "amount": "95"
    },
    "evmBlockHash": "0xf3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3",
    "height": "42",
    "lastDiffTimestamp": "1700000000000",
    "lastEpochHash": "EvHtV2w6pSBfjeXYrYCGwKz9HiLTk6Rv74JB1JEQudSd",
    "minerAddress": "49XF233b4hnYfgyzL5LL3zsZmhbn",
    "oracleIrysPrice": {
      "amount": "100"
    },
    "poa": {
      "chunk": "AQID",
      "dataPath": "Bgc",
      "ledgerId": 1,
      "partitionChunkOffset": 100,
      "partitionHash": "F83muwL8bL5M9vH5ASB2oxJS2By4mHVNnHJ9BSND9dQk",
      "txPath": "BAU"
    },
    "previousBlockHash": "F48Umds812n81q2Zj8r7X5Xfn2ks6DoZDsdV84KcQJ63",
    "previousCumulativeDiff": "49000",
    "previousSolutionHash": "ErNbLjU6E8tSbZH3REsMeTDP3Z8G52k6YedWwvBpAJ7v",
    "rewardAddress": "48iC7xethYqhHs7j88faQMEjgM4B",
    "rewardAmount": "100",
    "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv",
    "solutionHash": "EnTJCS15dqbDTU2XywYSMaScoPv4Py4GzExrtY9DQxoD",
    "systemLedgers": [
      {
        "ledgerId": 0,
        "txIds": [
          "HDXwMGmSvaHNUj2oghHWq6E5b7VJeFmo6vxUzDknJxQf"
        ]
      }
    ],
    "timestamp": "1700000000000",
    "treasury": "999999",
    "vdfLimiterInfo": {
      "globalStepNumber": "1000",
      "lastStepCheckpoints": [
        "C5hUUPNfyui9sr2EXzVgiZa733W1TRbUdvJcZLMFXdtw"
      ],
      "nextSeed": "BwrtBnSeoK7hbfXDfPqr8p2aYj5c7JDqX6yJSaG42yFX",
      "nextVdfDifficulty": "55000",
      "output": "Bp2HuBWdciXFKV2CnoC1Z4V44QfCmArCQHdzKpArYJc7",
      "prevOutput": "C1nBL5ufPcQvjkmj6hAmRgoLntHonMuf5WdxVxJenJaE",
      "seed": "Bswb3UyeD1pUTaGiE6WvqwFpJZsQSEY1xhJePCDTHdvp",
      "steps": [
        "C9cmcgqgaD1P1wGjyHpc1SLsHCiD8VHJCKyGciPrGyDe"
      ],
      "vdfDifficulty": "50000"
    }
  }
}"#;

const FIXTURE_V2_GOSSIP_DATA_BLOCK_BODY: &str = r#"{
  "BlockBody": {
    "block_hash": "DdqGmK5uamYN5vmuZrzpQhKeehLdwtPLVJdhu5P2iJKC",
    "data_transactions": [
      {
        "version": 1,
        "anchor": "8qbHbw2BbbTHBW1sbeqakYXVKRQM8Ne7pLK7m6CVfeR",
        "bundleFormat": "1",
        "chainId": "1270",
        "dataRoot": "GgBaCs3NCBuZN12kCJgAW63ydqohFkHEdfdEXBPzLHq",
        "dataSize": "1048576",
        "headerSize": "256",
        "id": "4vJ9JU1bJJE96FWSJKvHsmmFADCg4gpZQff4P3bkLKi",
        "ledgerId": 1,
        "permFee": "500000",
        "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv",
        "signer": "3S9hEB66TqZ8Ubncq1FwwuVH4dp",
        "termFee": "1000000"
      }
    ],
    "commitment_transactions": [
      {
        "version": 2,
        "anchor": "6URwbPipuA4MJLG7LCRRZuWnms3JZ9cRG3z9indXWz8G",
        "chainId": "1270",
        "commitmentType": {
          "type": "stake"
        },
        "fee": "5000",
        "id": "6QWeT6FpJrm8AF1btu6WH2k2Xhq6t5vbheKVfQavmeoZ",
        "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv",
        "signer": "29X6dD64ueYorg4VtHniED4za3gV",
        "value": "2000000"
      }
    ]
  }
}"#;

const FIXTURE_V2_GOSSIP_DATA_INGRESS_PROOF: &str = r#"{
  "IngressProof": {
    "version": 1,
    "anchor": "G4uuv9rGsWEX7BnBGcjttD77SQutCB6rbzdKzkzbcHve",
    "chain_id": 1270,
    "data_root": "Fw5KdYvFgue4q1HAQ264JTZax6VUr3jDVBJ1szuQ7dHE",
    "proof": "FzzcmrPGHCwHy6XfqKQybLLMCFhgX7R33axfwNwzrxbw",
    "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv"
  }
}"#;

const FIXTURE_V2_DATA_REQUEST_EXECUTION_PAYLOAD: &str = r#"{
  "ExecutionPayload": "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
}"#;

const FIXTURE_V2_DATA_REQUEST_BLOCK_HEADER: &str = r#"{
  "BlockHeader": "DdqGmK5uamYN5vmuZrzpQhKeehLdwtPLVJdhu5P2iJKC"
}"#;

const FIXTURE_V2_DATA_REQUEST_BLOCK_BODY: &str = r#"{
  "BlockBody": "DdqGmK5uamYN5vmuZrzpQhKeehLdwtPLVJdhu5P2iJKC"
}"#;

const FIXTURE_V2_DATA_REQUEST_CHUNK: &str = r#"{
  "Chunk": "EnTJCS15dqbDTU2XywYSMaScoPv4Py4GzExrtY9DQxoD"
}"#;

const FIXTURE_V2_DATA_REQUEST_TRANSACTION: &str = r#"{
  "Transaction": "Fw5KdYvFgue4q1HAQ264JTZax6VUr3jDVBJ1szuQ7dHE"
}"#;

const FIXTURE_V2_GOSSIP_REQUEST: &str = r#"{
  "peer_id": "3chLuAB9CqrCNL42WFwjPLk4GEtr",
  "miner_address": "3NuVdsXK1DmiyJKa1EawMJwxhDdb",
  "data": {
    "Chunk": {
      "dataRoot": "CVDFLCAjXhVWiPXH9nTCTpCgVzmDVoiPzNJYuccr1dqB",
      "dataSize": "262144",
      "dataPath": "3q2-7w",
      "bytes": "yv66vg",
      "txOffset": 0
    }
  }
}"#;

const FIXTURE_HANDSHAKE_REQUEST_V1: &str = r#"{
  "version": "1.2.3",
  "protocol_version": "V1",
  "mining_address": "3NuVdsXK1DmiyJKa1EawMJwxhDdb",
  "chain_id": 1270,
  "address": {
    "gossip": "192.168.1.1:4200",
    "api": "192.168.1.1:4201",
    "execution": {
      "peering_tcp_addr": "192.168.1.1:30303",
      "peer_id": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
    }
  },
  "timestamp": 1700000000000,
  "user_agent": "irys-node/1.2.3",
  "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv"
}"#;

const FIXTURE_HANDSHAKE_REQUEST_V2: &str = r#"{
  "version": "1.2.3",
  "protocol_version": "V2",
  "mining_address": "3NuVdsXK1DmiyJKa1EawMJwxhDdb",
  "peer_id": "3chLuAB9CqrCNL42WFwjPLk4GEtr",
  "chain_id": 1270,
  "address": {
    "gossip": "192.168.1.1:4200",
    "api": "192.168.1.1:4201",
    "execution": {
      "peering_tcp_addr": "192.168.1.1:30303",
      "peer_id": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
    }
  },
  "timestamp": 1700000000000,
  "user_agent": "irys-node/1.2.3",
  "consensus_config_hash": "JEKNVnkbo3jma5nREBBJCDoXFVeKkD56V3xKrvRmWxFG",
  "signature": "11111111111111111111111111111112K3n5t4wSaF5mj27Tw9vStXWLWyRjjiH5Cp3CFLpKVCrAv"
}"#;

const FIXTURE_HANDSHAKE_RESPONSE_V1: &str = r#"{
  "version": "1.2.3",
  "protocol_version": "V1",
  "peers": [
    {
      "gossip": "192.168.1.1:4200",
      "api": "192.168.1.1:4201",
      "execution": {
        "peering_tcp_addr": "192.168.1.1:30303",
        "peer_id": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
      }
    }
  ],
  "timestamp": 1700000000000,
  "message": "Welcome"
}"#;

const FIXTURE_HANDSHAKE_RESPONSE_V2: &str = r#"{
  "version": "1.2.3",
  "protocol_version": "V2",
  "peers": [
    {
      "gossip": "192.168.1.1:4200",
      "api": "192.168.1.1:4201",
      "execution": {
        "peering_tcp_addr": "192.168.1.1:30303",
        "peer_id": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
      }
    }
  ],
  "timestamp": 1700000000000,
  "message": "Welcome",
  "consensus_config_hash": "JEKNVnkbo3jma5nREBBJCDoXFVeKkD56V3xKrvRmWxFG"
}"#;

const FIXTURE_GOSSIP_RESPONSE_ACCEPTED: &str = r#"{
  "Accepted": null
}"#;

const FIXTURE_GOSSIP_RESPONSE_REJECTED_HANDSHAKE_REQUIRED_NONE: &str = r#"{
  "Rejected": {
    "HandshakeRequired": null
  }
}"#;

const FIXTURE_GOSSIP_RESPONSE_REJECTED_HANDSHAKE_REQUIRED_NOT_IN_PEER_LIST: &str = r#"{
  "Rejected": {
    "HandshakeRequired": "RequestOriginIsNotInThePeerList"
  }
}"#;

const FIXTURE_GOSSIP_RESPONSE_REJECTED_HANDSHAKE_REQUIRED_ORIGIN_MISMATCH: &str = r#"{
  "Rejected": {
    "HandshakeRequired": "RequestOriginDoesNotMatchExpected"
  }
}"#;

const FIXTURE_GOSSIP_RESPONSE_REJECTED_HANDSHAKE_REQUIRED_UNKNOWN_MINER: &str = r#"{
  "Rejected": {
    "HandshakeRequired": "MinerAddressIsUnknown"
  }
}"#;

const FIXTURE_GOSSIP_RESPONSE_REJECTED_GOSSIP_DISABLED: &str = r#"{
  "Rejected": "GossipDisabled"
}"#;

const FIXTURE_GOSSIP_RESPONSE_REJECTED_INVALID_DATA: &str = r#"{
  "Rejected": "InvalidData"
}"#;

const FIXTURE_GOSSIP_RESPONSE_REJECTED_RATE_LIMITED: &str = r#"{
  "Rejected": "RateLimited"
}"#;

const FIXTURE_GOSSIP_RESPONSE_REJECTED_UNABLE_TO_VERIFY_ORIGIN: &str = r#"{
  "Rejected": "UnableToVerifyOrigin"
}"#;

const FIXTURE_GOSSIP_RESPONSE_REJECTED_INVALID_CREDENTIALS: &str = r#"{
  "Rejected": "InvalidCredentials"
}"#;

const FIXTURE_GOSSIP_RESPONSE_REJECTED_PROTOCOL_MISMATCH: &str = r#"{
  "Rejected": "ProtocolMismatch"
}"#;

const FIXTURE_GOSSIP_RESPONSE_REJECTED_UNSUPPORTED_PROTOCOL_VERSION: &str = r#"{
  "Rejected": {
    "UnsupportedProtocolVersion": 99
  }
}"#;

const FIXTURE_GOSSIP_RESPONSE_REJECTED_UNSUPPORTED_FEATURE: &str = r#"{
  "Rejected": "UnsupportedFeature"
}"#;

const FIXTURE_NODE_INFO: &str = r#"{
  "version": "1.2.3",
  "peerCount": 5,
  "chainId": "1270",
  "height": "42",
  "blockHash": "DdqGmK5uamYN5vmuZrzpQhKeehLdwtPLVJdhu5P2iJKC",
  "blockIndexHeight": "40",
  "blockIndexHash": "DhkZucYvB4qbE22R1AKjha6QtrYqcx5A3iJMxTRdTddu",
  "pendingBlocks": "2",
  "isSyncing": false,
  "currentSyncHeight": "42",
  "uptimeSecs": "3600",
  "miningAddress": "3NuVdsXK1DmiyJKa1EawMJwxhDdb",
  "cumulativeDifficulty": "50000"
}"#;
