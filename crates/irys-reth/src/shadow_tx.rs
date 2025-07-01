//! # Shadow Transactions
//!
//! This module defines the shadow transaction types used in the Irys protocol. Shadow transactions
//! are special EVM transactions that encode protocol-level actions, such as block rewards, storage
//! fee collection, stake and pledge management. The Irys Consensus Layer (CL) is responsible
//! for validating these transactions in every block, ensuring protocol rules are enforced:
//!
//! - **Block rewards** must go to the Irys block producer
//! - **Balance increments** correspond to rewards
//! - **Balance decrements** correspond to storage transaction fees

use alloy_primitives::keccak256;
use alloy_primitives::Address;
use alloy_primitives::FixedBytes;
use alloy_primitives::U256;
use alloy_rlp::Decodable;
use alloy_rlp::Encodable;
use alloy_rlp::{RlpDecodable, RlpEncodable};
use bytes;
use std::sync::LazyLock;

/// Version constants for ShadowTransaction
pub const SHADOW_TX_VERSION_V1: u8 = 1;

/// Current version of ShadowTransaction
pub const CURRENT_SHADOW_TX_VERSION: u8 = SHADOW_TX_VERSION_V1;

/// A versioned shadow transaction, valid for a single block, encoding a protocol-level action.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, arbitrary::Arbitrary)]
#[non_exhaustive]
pub enum ShadowTransaction {
    /// Version 1 shadow transaction format
    ///
    V1 {
        /// The actual shadow transaction packet.
        packet: TransactionPacket,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, arbitrary::Arbitrary)]
pub enum TransactionPacket {
    /// Unstake funds to an account (balance increment). Used for unstaking or protocol rewards.
    Unstake(BalanceIncrement),
    /// Block reward payment to the block producer (balance increment). Must be validated by CL.
    BlockReward(BlockRewardIncrement),
    /// Stake funds from an account (balance decrement). Used for staking operations.
    Stake(BalanceDecrement),
    /// Collect storage fees from an account (balance decrement). Must match storage usage.
    StorageFees(BalanceDecrement),
    /// Pledge funds to an account (balance decrement). Used for pledging operations.
    Pledge(BalanceDecrement),
    /// Unpledge funds from an account (balance increment). Used for unpledging operations.
    Unpledge(BalanceIncrement),
}

/// Topics for shadow transaction logs
#[expect(
    clippy::module_name_repetitions,
    reason = "module name in type name provides clarity"
)]
pub mod shadow_tx_topics {
    use super::*;

    pub static UNSTAKE: LazyLock<[u8; 32]> = LazyLock::new(|| keccak256("SHADOW_TX_UNSTAKE").0);
    pub static BLOCK_REWARD: LazyLock<[u8; 32]> =
        LazyLock::new(|| keccak256("SHADOW_TX_BLOCK_REWARD").0);
    pub static STAKE: LazyLock<[u8; 32]> = LazyLock::new(|| keccak256("SHADOW_TX_STAKE").0);
    pub static STORAGE_FEES: LazyLock<[u8; 32]> =
        LazyLock::new(|| keccak256("SHADOW_TX_STORAGE_FEES").0);
    pub static PLEDGE: LazyLock<[u8; 32]> = LazyLock::new(|| keccak256("SHADOW_TX_PLEDGE").0);
    pub static UNPLEDGE: LazyLock<[u8; 32]> = LazyLock::new(|| keccak256("SHADOW_TX_UNPLEDGE").0);
}

impl ShadowTransaction {
    /// Create a new V1 shadow transaction
    #[must_use]
    pub fn new_v1(packet: TransactionPacket) -> Self {
        Self::V1 { packet }
    }

    /// Get the version of this shadow transaction
    #[must_use]
    pub fn version(&self) -> u8 {
        match self {
            Self::V1 { .. } => SHADOW_TX_VERSION_V1,
        }
    }

    /// Get the underlying transaction packet if this is a V1 transaction
    #[must_use]
    pub fn as_v1(&self) -> Option<&TransactionPacket> {
        match self {
            Self::V1 { packet, .. } => Some(packet),
        }
    }

    /// Get the topic for this shadow transaction.
    #[must_use]
    pub fn topic(&self) -> FixedBytes<32> {
        match self {
            Self::V1 { packet, .. } => packet.topic(),
        }
    }
}

impl TransactionPacket {
    /// Get the topic for this transaction packet.
    #[must_use]
    pub fn topic(&self) -> FixedBytes<32> {
        use shadow_tx_topics::*;
        match self {
            Self::Unstake(_) => (*UNSTAKE).into(),
            Self::BlockReward(_) => (*BLOCK_REWARD).into(),
            Self::Stake(_) => (*STAKE).into(),
            Self::StorageFees(_) => (*STORAGE_FEES).into(),
            Self::Pledge(_) => (*PLEDGE).into(),
            Self::Unpledge(_) => (*UNPLEDGE).into(),
        }
    }
}

/// Stable 1-byte discriminants
pub const UNSTAKE_ID: u8 = 0x01;
pub const BLOCK_REWARD_ID: u8 = 0x02;
pub const STAKE_ID: u8 = 0x03;
pub const STORAGE_FEES_ID: u8 = 0x04;
pub const PLEDGE_ID: u8 = 0x05;
pub const UNPLEDGE_ID: u8 = 0x06;

#[expect(
    clippy::arithmetic_side_effects,
    reason = "length calculation is safe for small values"
)]
impl Encodable for ShadowTransaction {
    fn length(&self) -> usize {
        1 + // version byte
        match self {
            Self::V1 { packet } => packet.length()
        }
    }

    fn encode(&self, out: &mut dyn bytes::BufMut) {
        match self {
            Self::V1 { packet } => {
                out.put_u8(SHADOW_TX_VERSION_V1);
                packet.encode(out);
            }
        }
    }
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "length calculation is safe for small values"
)]
impl Encodable for TransactionPacket {
    fn length(&self) -> usize {
        1 + match self {
            Self::Unstake(bi) | Self::Unpledge(bi) => bi.length(),
            Self::BlockReward(br) => br.length(),
            Self::Stake(bd) | Self::StorageFees(bd) | Self::Pledge(bd) => bd.length(),
        }
    }

    fn encode(&self, out: &mut dyn bytes::BufMut) {
        match self {
            Self::Unstake(inner) => {
                out.put_u8(UNSTAKE_ID);
                inner.encode(out);
            }
            Self::BlockReward(inner) => {
                out.put_u8(BLOCK_REWARD_ID);
                inner.encode(out);
            }
            Self::Stake(inner) => {
                out.put_u8(STAKE_ID);
                inner.encode(out);
            }
            Self::StorageFees(inner) => {
                out.put_u8(STORAGE_FEES_ID);
                inner.encode(out);
            }
            Self::Pledge(inner) => {
                out.put_u8(PLEDGE_ID);
                inner.encode(out);
            }
            Self::Unpledge(inner) => {
                out.put_u8(UNPLEDGE_ID);
                inner.encode(out);
            }
        }
    }
}

#[expect(
    clippy::indexing_slicing,
    reason = "buffer bounds are checked before indexing"
)]
impl Decodable for ShadowTransaction {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        if buf.is_empty() {
            return Err(alloy_rlp::Error::InputTooShort);
        }
        let version = buf[0];
        *buf = &buf[1..]; // advance past the version byte

        match version {
            SHADOW_TX_VERSION_V1 => {
                let packet = TransactionPacket::decode(buf)?;
                Ok(Self::V1 { packet })
            }
            _ => Err(alloy_rlp::Error::Custom(
                "Unknown shadow transaction version",
            )),
        }
    }
}

#[expect(
    clippy::indexing_slicing,
    reason = "buffer bounds are checked before indexing"
)]
impl Decodable for TransactionPacket {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        if buf.is_empty() {
            return Err(alloy_rlp::Error::InputTooShort);
        }
        let disc = buf[0];
        *buf = &buf[1..]; // advance past the discriminant byte

        match disc {
            UNSTAKE_ID => {
                let inner = BalanceIncrement::decode(buf)?;
                Ok(Self::Unstake(inner))
            }
            BLOCK_REWARD_ID => {
                let inner = BlockRewardIncrement::decode(buf)?;
                Ok(Self::BlockReward(inner))
            }
            STAKE_ID => {
                let inner = BalanceDecrement::decode(buf)?;
                Ok(Self::Stake(inner))
            }
            STORAGE_FEES_ID => {
                let inner = BalanceDecrement::decode(buf)?;
                Ok(Self::StorageFees(inner))
            }
            PLEDGE_ID => {
                let inner = BalanceDecrement::decode(buf)?;
                Ok(Self::Pledge(inner))
            }
            UNPLEDGE_ID => {
                let inner = BalanceIncrement::decode(buf)?;
                Ok(Self::Unpledge(inner))
            }
            _ => Err(alloy_rlp::Error::Custom(
                "Unknown shadow transaction discriminant",
            )),
        }
    }
}

#[expect(
    clippy::unimplemented,
    reason = "intentional panic to prevent silent bugs"
)]
impl Default for ShadowTransaction {
    fn default() -> Self {
        unimplemented!("relying on the default impl for `ShadowTransaction` is a critical bug")
    }
}

#[expect(
    clippy::unimplemented,
    reason = "intentional panic to prevent silent bugs"
)]
impl Default for TransactionPacket {
    fn default() -> Self {
        unimplemented!("relying on the default impl for `TransactionPacket` is a critical bug")
    }
}

/// Balance decrement: used for staking and storage fee collection shadow txs.
#[derive(
    serde::Deserialize,
    serde::Serialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    RlpEncodable,
    RlpDecodable,
    arbitrary::Arbitrary,
)]
pub struct BalanceDecrement {
    /// Amount to decrement from the target account.
    pub amount: U256,
    /// Target account address.
    pub target: Address,
    /// Reference to the consensus layer transaction that resulted in this shadow tx.
    pub irys_ref: FixedBytes<32>,
}

/// Balance increment: used for unstake shadow txs.
#[derive(
    serde::Deserialize,
    serde::Serialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    RlpEncodable,
    RlpDecodable,
    arbitrary::Arbitrary,
)]
pub struct BalanceIncrement {
    /// Amount to increment to the target account.
    pub amount: U256,
    /// Target account address.
    pub target: Address,
    /// Reference to the consensus layer transaction that resulted in this shadow tx.
    pub irys_ref: FixedBytes<32>,
}

/// Block reward increment: used for block reward shadow txs (no irys_ref needed).
#[derive(
    serde::Deserialize,
    serde::Serialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    RlpEncodable,
    RlpDecodable,
    arbitrary::Arbitrary,
)]
pub struct BlockRewardIncrement {
    /// Amount to increment to the target account.
    pub amount: U256,
    /// Target account address.
    pub target: Address,
}
