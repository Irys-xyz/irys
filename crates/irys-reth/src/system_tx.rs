//! # System Transactions
//!
//! This module defines the system transaction types used in the Irys protocol. System transactions
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

/// Version constants for SystemTransaction
pub const SYSTEM_TX_VERSION_V1: u8 = 1;

/// Current version of SystemTransaction
pub const CURRENT_SYSTEM_TX_VERSION: u8 = SYSTEM_TX_VERSION_V1;

/// A versioned system transaction, valid for a single block, encoding a protocol-level action.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, arbitrary::Arbitrary)]
#[non_exhaustive]
pub enum SystemTransaction {
    /// Version 1 system transaction format
    ///
    V1 {
        /// The block height for which this system tx is valid.
        valid_for_block_height: u64,
        /// The parent block hash to ensure the tx is not replayed on forks.
        parent_blockhash: FixedBytes<32>,
        /// The actual system transaction packet.
        packet: TransactionPacket,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, arbitrary::Arbitrary)]
pub enum TransactionPacket {
    /// Unstake funds to an account (balance increment). Used for unstaking or protocol rewards.
    Unstake(BalanceIncrement),
    /// Block reward payment to the block producer (balance increment). Must be validated by CL.
    BlockReward(BalanceIncrement),
    /// Stake funds from an account (balance decrement). Used for staking operations.
    Stake(BalanceDecrement),
    /// Collect storage fees from an account (balance decrement). Must match storage usage.
    StorageFees(BalanceDecrement),
    /// Pledge funds to an account (balance increment). Used for pledging operations.
    Pledge(BalanceIncrement),
    /// Unpledge funds from an account (balance decrement). Used for unpledging operations.
    Unpledge(BalanceDecrement),
}

/// Topics for system transaction logs
#[expect(
    clippy::module_name_repetitions,
    reason = "module name in type name provides clarity"
)]
pub mod system_tx_topics {
    use super::*;

    pub static UNSTAKE: LazyLock<[u8; 32]> = LazyLock::new(|| keccak256("SYSTEM_TX_UNSTAKE").0);
    pub static BLOCK_REWARD: LazyLock<[u8; 32]> =
        LazyLock::new(|| keccak256("SYSTEM_TX_BLOCK_REWARD").0);
    pub static STAKE: LazyLock<[u8; 32]> = LazyLock::new(|| keccak256("SYSTEM_TX_STAKE").0);
    pub static STORAGE_FEES: LazyLock<[u8; 32]> =
        LazyLock::new(|| keccak256("SYSTEM_TX_STORAGE_FEES").0);
    pub static PLEDGE: LazyLock<[u8; 32]> = LazyLock::new(|| keccak256("SYSTEM_TX_PLEDGE").0);
    pub static UNPLEDGE: LazyLock<[u8; 32]> = LazyLock::new(|| keccak256("SYSTEM_TX_UNPLEDGE").0);
}

impl SystemTransaction {
    /// Create a new V1 system transaction
    #[must_use]
    pub fn new_v1(
        valid_for_block_height: u64,
        parent_blockhash: FixedBytes<32>,
        packet: TransactionPacket,
    ) -> Self {
        Self::V1 {
            valid_for_block_height,
            parent_blockhash,
            packet,
        }
    }

    /// Get the version of this system transaction
    #[must_use]
    pub fn version(&self) -> u8 {
        match self {
            Self::V1 { .. } => SYSTEM_TX_VERSION_V1,
        }
    }

    /// Get the block height for which this system tx is valid
    #[must_use]
    pub fn valid_for_block_height(&self) -> u64 {
        match self {
            Self::V1 {
                valid_for_block_height,
                ..
            } => *valid_for_block_height,
        }
    }

    /// Get the parent block hash
    #[must_use]
    pub fn parent_blockhash(&self) -> FixedBytes<32> {
        match self {
            Self::V1 {
                parent_blockhash, ..
            } => *parent_blockhash,
        }
    }

    /// Get the underlying transaction packet if this is a V1 transaction
    #[must_use]
    pub fn as_v1(&self) -> Option<&TransactionPacket> {
        match self {
            Self::V1 { packet, .. } => Some(packet),
        }
    }

    /// Get the topic for this system transaction.
    #[must_use]
    pub fn topic(&self) -> FixedBytes<32> {
        match self {
            Self::V1 { packet, .. } => packet.topic(),
        }
    }

    /// Get the encoded topic for this system transaction.
    #[must_use]
    pub fn encoded_topic(&self) -> [u8; 32] {
        match self {
            Self::V1 { packet, .. } => packet.encoded_topic(),
        }
    }
}

impl TransactionPacket {
    /// Get the topic for this transaction packet.
    #[must_use]
    pub fn topic(&self) -> FixedBytes<32> {
        use system_tx_topics::*;
        match self {
            Self::Unstake(_) => (*UNSTAKE).into(),
            Self::BlockReward(_) => (*BLOCK_REWARD).into(),
            Self::Stake(_) => (*STAKE).into(),
            Self::StorageFees(_) => (*STORAGE_FEES).into(),
            Self::Pledge(_) => (*PLEDGE).into(),
            Self::Unpledge(_) => (*UNPLEDGE).into(),
        }
    }

    /// Get the encoded topic for this transaction packet.
    #[must_use]
    pub fn encoded_topic(&self) -> [u8; 32] {
        match self {
            Self::Unstake(bi) | Self::BlockReward(bi) | Self::Pledge(bi) => {
                use alloy_dyn_abi::DynSolValue;
                DynSolValue::Tuple(vec![
                    DynSolValue::Uint(bi.amount, 256),
                    DynSolValue::Address(bi.target),
                ])
                .abi_encode_packed()
                .try_into()
                .unwrap_or_default()
            }
            Self::Stake(bd) | Self::StorageFees(bd) | Self::Unpledge(bd) => {
                use alloy_dyn_abi::DynSolValue;
                DynSolValue::Tuple(vec![
                    DynSolValue::Uint(bd.amount, 256),
                    DynSolValue::Address(bd.target),
                ])
                .abi_encode_packed()
                .try_into()
                .unwrap_or_default()
            }
        }
    }
}

/// Stable 1-byte discriminants
pub const UNSTAKE_ID: u8 = 0x00;
pub const BLOCK_REWARD_ID: u8 = 0x01;
pub const STAKE_ID: u8 = 0x02;
pub const STORAGE_FEES_ID: u8 = 0x03;
pub const PLEDGE_ID: u8 = 0x04;
pub const UNPLEDGE_ID: u8 = 0x05;

#[expect(
    clippy::arithmetic_side_effects,
    reason = "length calculation is safe for small values"
)]
impl Encodable for SystemTransaction {
    fn length(&self) -> usize {
        1 + // version byte
        match self {
            Self::V1 { valid_for_block_height, parent_blockhash, packet } => {
                valid_for_block_height.length() +
                parent_blockhash.length() +
                packet.length()
            }
        }
    }

    fn encode(&self, out: &mut dyn bytes::BufMut) {
        match self {
            Self::V1 {
                valid_for_block_height,
                parent_blockhash,
                packet,
            } => {
                out.put_u8(SYSTEM_TX_VERSION_V1);
                valid_for_block_height.encode(out);
                parent_blockhash.encode(out);
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
            Self::Unstake(bi) | Self::BlockReward(bi) | Self::Pledge(bi) => bi.length(),
            Self::Stake(bd) | Self::StorageFees(bd) | Self::Unpledge(bd) => bd.length(),
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
impl Decodable for SystemTransaction {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        if buf.is_empty() {
            return Err(alloy_rlp::Error::InputTooShort);
        }
        let version = buf[0];
        *buf = &buf[1..]; // advance past the version byte

        match version {
            SYSTEM_TX_VERSION_V1 => {
                let valid_for_block_height = u64::decode(buf)?;
                let parent_blockhash = FixedBytes::<32>::decode(buf)?;
                let packet = TransactionPacket::decode(buf)?;
                Ok(Self::V1 {
                    valid_for_block_height,
                    parent_blockhash,
                    packet,
                })
            }
            _ => Err(alloy_rlp::Error::Custom(
                "Unknown system transaction version",
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
                let inner = BalanceIncrement::decode(buf)?;
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
                let inner = BalanceIncrement::decode(buf)?;
                Ok(Self::Pledge(inner))
            }
            UNPLEDGE_ID => {
                let inner = BalanceDecrement::decode(buf)?;
                Ok(Self::Unpledge(inner))
            }
            _ => Err(alloy_rlp::Error::Custom(
                "Unknown system transaction discriminant",
            )),
        }
    }
}

#[expect(
    clippy::unimplemented,
    reason = "intentional panic to prevent silent bugs"
)]
impl Default for SystemTransaction {
    fn default() -> Self {
        unimplemented!("relying on the default impl for `SystemTransaction` is a critical bug")
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

/// Balance decrement: used for staking and storage fee collection system txs.
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
}

/// Balance increment: used for block rewards and unstake system txs.
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
}
