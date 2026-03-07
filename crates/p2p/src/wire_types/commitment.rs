use irys_types::{IrysAddress, IrysSignature, H256, U256};
use serde::{Deserialize, Serialize};

use super::impl_json_version_tagged_serde;

/// Wire type for CommitmentTypeV1.
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

/// Wire type for CommitmentTypeV2 (adds `UpdateRewardAddress` variant).
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

/// Inner fields for the versioned CommitmentTransactionV1 wire type.
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

/// Inner fields for the versioned CommitmentTransactionV2 wire type.
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

// -- Conversions --

impl From<&irys_types::CommitmentTypeV1> for CommitmentTypeV1 {
    fn from(ct: &irys_types::CommitmentTypeV1) -> Self {
        match ct {
            irys_types::CommitmentTypeV1::Stake => Self::Stake,
            irys_types::CommitmentTypeV1::Pledge {
                pledge_count_before_executing,
            } => Self::Pledge {
                pledge_count_before_executing: *pledge_count_before_executing,
            },
            irys_types::CommitmentTypeV1::Unpledge {
                pledge_count_before_executing,
                partition_hash,
            } => Self::Unpledge {
                pledge_count_before_executing: *pledge_count_before_executing,
                partition_hash: *partition_hash,
            },
            irys_types::CommitmentTypeV1::Unstake => Self::Unstake,
        }
    }
}

impl From<&irys_types::CommitmentTypeV2> for CommitmentTypeV2 {
    fn from(ct: &irys_types::CommitmentTypeV2) -> Self {
        match ct {
            irys_types::CommitmentTypeV2::Stake => Self::Stake,
            irys_types::CommitmentTypeV2::Pledge {
                pledge_count_before_executing,
            } => Self::Pledge {
                pledge_count_before_executing: *pledge_count_before_executing,
            },
            irys_types::CommitmentTypeV2::Unpledge {
                pledge_count_before_executing,
                partition_hash,
            } => Self::Unpledge {
                pledge_count_before_executing: *pledge_count_before_executing,
                partition_hash: *partition_hash,
            },
            irys_types::CommitmentTypeV2::Unstake => Self::Unstake,
            irys_types::CommitmentTypeV2::UpdateRewardAddress { new_reward_address } => {
                Self::UpdateRewardAddress {
                    new_reward_address: *new_reward_address,
                }
            }
        }
    }
}

impl From<CommitmentTypeV1> for irys_types::CommitmentTypeV1 {
    fn from(ct: CommitmentTypeV1) -> Self {
        match ct {
            CommitmentTypeV1::Stake => Self::Stake,
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing,
            } => Self::Pledge {
                pledge_count_before_executing,
            },
            CommitmentTypeV1::Unpledge {
                pledge_count_before_executing,
                partition_hash,
            } => Self::Unpledge {
                pledge_count_before_executing,
                partition_hash,
            },
            CommitmentTypeV1::Unstake => Self::Unstake,
        }
    }
}

impl From<CommitmentTypeV2> for irys_types::CommitmentTypeV2 {
    fn from(ct: CommitmentTypeV2) -> Self {
        match ct {
            CommitmentTypeV2::Stake => Self::Stake,
            CommitmentTypeV2::Pledge {
                pledge_count_before_executing,
            } => Self::Pledge {
                pledge_count_before_executing,
            },
            CommitmentTypeV2::Unpledge {
                pledge_count_before_executing,
                partition_hash,
            } => Self::Unpledge {
                pledge_count_before_executing,
                partition_hash,
            },
            CommitmentTypeV2::Unstake => Self::Unstake,
            CommitmentTypeV2::UpdateRewardAddress { new_reward_address } => {
                Self::UpdateRewardAddress { new_reward_address }
            }
        }
    }
}

impl From<&irys_types::CommitmentTransaction> for CommitmentTransaction {
    fn from(ct: &irys_types::CommitmentTransaction) -> Self {
        match ct {
            irys_types::CommitmentTransaction::V1(wm) => Self::V1(CommitmentTransactionV1Inner {
                id: wm.tx.id,
                anchor: wm.tx.anchor,
                signer: wm.tx.signer,
                commitment_type: (&wm.tx.commitment_type).into(),
                chain_id: wm.tx.chain_id,
                fee: wm.tx.fee,
                value: wm.tx.value,
                signature: wm.tx.signature,
            }),
            irys_types::CommitmentTransaction::V2(wm) => Self::V2(CommitmentTransactionV2Inner {
                id: wm.tx.id,
                anchor: wm.tx.anchor,
                signer: wm.tx.signer,
                commitment_type: (&wm.tx.commitment_type).into(),
                chain_id: wm.tx.chain_id,
                fee: wm.tx.fee,
                value: wm.tx.value,
                signature: wm.tx.signature,
            }),
        }
    }
}

impl From<CommitmentTransaction> for irys_types::CommitmentTransaction {
    fn from(ct: CommitmentTransaction) -> Self {
        match ct {
            CommitmentTransaction::V1(inner) => Self::V1(irys_types::CommitmentV1WithMetadata {
                tx: irys_types::CommitmentTransactionV1 {
                    id: inner.id,
                    anchor: inner.anchor,
                    signer: inner.signer,
                    commitment_type: inner.commitment_type.into(),
                    chain_id: inner.chain_id,
                    fee: inner.fee,
                    value: inner.value,
                    signature: inner.signature,
                },
                // Metadata is not transmitted over the wire; initialize to default on deserialization.
                metadata: Default::default(),
            }),
            CommitmentTransaction::V2(inner) => Self::V2(irys_types::CommitmentV2WithMetadata {
                tx: irys_types::CommitmentTransactionV2 {
                    id: inner.id,
                    anchor: inner.anchor,
                    signer: inner.signer,
                    commitment_type: inner.commitment_type.into(),
                    chain_id: inner.chain_id,
                    fee: inner.fee,
                    value: inner.value,
                    signature: inner.signature,
                },
                // Metadata is not transmitted over the wire; initialize to default on deserialization.
                metadata: Default::default(),
            }),
        }
    }
}
