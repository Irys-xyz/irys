use irys_types::{IrysAddress, IrysSignature, H256, U256};
use serde::{Deserialize, Serialize};

use super::{impl_json_version_tagged_serde, impl_mirror_enum_from, impl_versioned_tx_from};

/// Adding a variant? Update the `impl_mirror_enum_from!` below AND add a
/// fixture entry in `gossip_fixture_tests.rs`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum CommitmentTypeV1 {
    Stake,
    Pledge {
        #[serde(rename = "pledgeCountBeforeExecuting", with = "irys_types::string_u64")]
        pledge_count_before_executing: u64,
    },
    Unpledge {
        #[serde(rename = "pledgeCountBeforeExecuting", with = "irys_types::string_u64")]
        pledge_count_before_executing: u64,
        #[serde(rename = "partitionHash")]
        partition_hash: H256,
    },
    Unstake,
}

/// Adding a variant? Update the `impl_mirror_enum_from!` below AND add a
/// fixture entry in `gossip_fixture_tests.rs`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum CommitmentTypeV2 {
    Stake,
    Pledge {
        #[serde(rename = "pledgeCountBeforeExecuting", with = "irys_types::string_u64")]
        pledge_count_before_executing: u64,
    },
    Unpledge {
        #[serde(rename = "pledgeCountBeforeExecuting", with = "irys_types::string_u64")]
        pledge_count_before_executing: u64,
        #[serde(rename = "partitionHash")]
        partition_hash: H256,
    },
    Unstake,
    UpdateRewardAddress {
        #[serde(rename = "newRewardAddress")]
        new_reward_address: IrysAddress,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommitmentTransactionV1Inner {
    pub id: H256,
    pub anchor: H256,
    pub signer: IrysAddress,
    pub commitment_type: CommitmentTypeV1,
    #[serde(with = "irys_types::string_u64")]
    pub chain_id: u64,
    #[serde(with = "irys_types::string_u64")]
    pub fee: u64,
    pub value: U256,
    pub signature: IrysSignature,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommitmentTransactionV2Inner {
    pub id: H256,
    pub anchor: H256,
    pub signer: IrysAddress,
    pub commitment_type: CommitmentTypeV2,
    #[serde(with = "irys_types::string_u64")]
    pub chain_id: u64,
    #[serde(with = "irys_types::string_u64")]
    pub fee: u64,
    pub value: U256,
    pub signature: IrysSignature,
}

/// Sovereign wire type for CommitmentTransaction.
/// Replaces IntegerTagged flattening with explicit version field + serde.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitmentTransaction {
    V1(CommitmentTransactionV1Inner),
    V2(CommitmentTransactionV2Inner),
}

impl_json_version_tagged_serde!(CommitmentTransaction {
    1 => V1(CommitmentTransactionV1Inner),
    2 => V2(CommitmentTransactionV2Inner),
});

impl_mirror_enum_from!(irys_types::CommitmentTypeV1, CommitmentTypeV1 {
    Stake,
    Pledge { pledge_count_before_executing },
    Unpledge { pledge_count_before_executing, partition_hash },
    Unstake,
});

impl_mirror_enum_from!(irys_types::CommitmentTypeV2, CommitmentTypeV2 {
    Stake,
    Pledge { pledge_count_before_executing },
    Unpledge { pledge_count_before_executing, partition_hash },
    Unstake,
    UpdateRewardAddress { new_reward_address },
});

impl_versioned_tx_from!(
    irys_types::CommitmentTransaction => CommitmentTransaction {
        V1 {
            gossip: CommitmentTransactionV1Inner,
            meta: irys_types::CommitmentV1WithMetadata,
            tx: irys_types::CommitmentTransactionV1,
            fields { id, anchor, signer, chain_id, fee, value, signature }
            convert { commitment_type }
        },
        V2 {
            gossip: CommitmentTransactionV2Inner,
            meta: irys_types::CommitmentV2WithMetadata,
            tx: irys_types::CommitmentTransactionV2,
            fields { id, anchor, signer, chain_id, fee, value, signature }
            convert { commitment_type }
        },
    }
);
