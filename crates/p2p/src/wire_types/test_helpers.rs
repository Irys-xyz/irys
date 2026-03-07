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
    BlockBody, CommitmentTransactionMetadata, DataTransactionLedger, IrysAddress, IrysSignature,
    Signature, SystemTransactionLedger, TxChunkOffset, UnixTimestampMs, H256, U256,
};
use reth::revm::primitives::B256;

// =============================================================================
// Primitive builders
// =============================================================================

pub(crate) fn test_h256(byte: u8) -> H256 {
    H256::from([byte; 32])
}

pub(crate) fn test_address(byte: u8) -> IrysAddress {
    IrysAddress::from_slice(&[byte; 20])
}

pub(crate) fn test_signature() -> IrysSignature {
    IrysSignature::new(Signature::new(
        reth::revm::primitives::U256::from(1_u64),
        reth::revm::primitives::U256::from(2_u64),
        false,
    ))
}

pub(crate) fn test_base64(bytes: &[u8]) -> Base64 {
    Base64(bytes.to_vec())
}

pub(crate) fn test_unix_timestamp() -> UnixTimestampMs {
    UnixTimestampMs::from(1_700_000_000_000_u128)
}

// =============================================================================
// Canonical type builders
//
// These use the exact values from the gossip fixture tests, which are locked
// by the `fixtures/gossip_fixtures.json` golden file.
// =============================================================================

pub(crate) fn canonical_unpacked_chunk() -> UnpackedChunk {
    UnpackedChunk {
        data_root: test_h256(0xAA),
        data_size: 262144,
        data_path: test_base64(&[0xDE, 0xAD, 0xBE, 0xEF]),
        bytes: test_base64(&[0xCA, 0xFE, 0xBA, 0xBE]),
        tx_offset: TxChunkOffset(0),
    }
}

pub(crate) fn canonical_ingress_proof() -> IngressProof {
    IngressProof::V1(IngressProofV1 {
        signature: test_signature(),
        data_root: test_h256(0xDD),
        proof: test_h256(0xDE),
        chain_id: 1270,
        anchor: test_h256(0xDF),
    })
}

pub(crate) fn canonical_commitment_v1_stake() -> CommitmentTransaction {
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

pub(crate) fn canonical_commitment_v1_pledge() -> CommitmentTransaction {
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

pub(crate) fn canonical_commitment_v1_unpledge() -> CommitmentTransaction {
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

pub(crate) fn canonical_commitment_v1_unstake() -> CommitmentTransaction {
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

pub(crate) fn canonical_commitment_v2_stake() -> CommitmentTransaction {
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

pub(crate) fn canonical_commitment_v2_pledge() -> CommitmentTransaction {
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

pub(crate) fn canonical_commitment_v2_unpledge() -> CommitmentTransaction {
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

pub(crate) fn canonical_commitment_v2_unstake() -> CommitmentTransaction {
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

pub(crate) fn canonical_commitment_v2_update_reward_address() -> CommitmentTransaction {
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

pub(crate) fn canonical_data_tx_header_v1_inner() -> DataTransactionHeaderV1 {
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

pub(crate) fn canonical_data_tx_header() -> DataTransactionHeader {
    DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
        tx: canonical_data_tx_header_v1_inner(),
        metadata: DataTransactionMetadata::new(),
    })
}

pub(crate) fn canonical_block_header() -> IrysBlockHeader {
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

pub(crate) fn canonical_block_body() -> BlockBody {
    BlockBody {
        block_hash: test_h256(0xBB),
        data_transactions: vec![canonical_data_tx_header()],
        commitment_transactions: vec![canonical_commitment_v2_stake()],
    }
}
