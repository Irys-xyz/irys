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
use std::io::Write;
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

impl ShadowTransaction {
    pub fn encode<W: Write>(&self, mut w: W) -> std::io::Result<()> {
        match self {
            Self::V1 { packet } => {
                w.write_all(&[SHADOW_TX_VERSION_V1])?;
                packet.encode(&mut w)
            }
        }
    }

    pub fn decode(input: &mut &[u8]) -> std::io::Result<Self> {
        if input.is_empty() {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "empty"));
        }
        let version = input[0];
        *input = &input[1..];
        match version {
            SHADOW_TX_VERSION_V1 => {
                let packet = TransactionPacket::decode(input)?;
                Ok(Self::V1 { packet })
            }
            _ => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "unknown version")),
        }
    }
}

impl TransactionPacket {
    fn encode<W: Write>(&self, mut w: W) -> std::io::Result<()> {
        match self {
            Self::Unstake(inner) => {
                w.write_all(&[UNSTAKE_ID])?;
                inner.encode(&mut w)
            }
            Self::BlockReward(inner) => {
                w.write_all(&[BLOCK_REWARD_ID])?;
                inner.encode(&mut w)
            }
            Self::Stake(inner) => {
                w.write_all(&[STAKE_ID])?;
                inner.encode(&mut w)
            }
            Self::StorageFees(inner) => {
                w.write_all(&[STORAGE_FEES_ID])?;
                inner.encode(&mut w)
            }
            Self::Pledge(inner) => {
                w.write_all(&[PLEDGE_ID])?;
                inner.encode(&mut w)
            }
            Self::Unpledge(inner) => {
                w.write_all(&[UNPLEDGE_ID])?;
                inner.encode(&mut w)
            }
        }
    }

    fn decode(input: &mut &[u8]) -> std::io::Result<Self> {
        if input.is_empty() {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "empty"));
        }
        let disc = input[0];
        *input = &input[1..];
        match disc {
            UNSTAKE_ID => Ok(Self::Unstake(BalanceIncrement::decode(input)?)),
            BLOCK_REWARD_ID => Ok(Self::BlockReward(BlockRewardIncrement::decode(input)?)),
            STAKE_ID => Ok(Self::Stake(BalanceDecrement::decode(input)?)),
            STORAGE_FEES_ID => Ok(Self::StorageFees(BalanceDecrement::decode(input)?)),
            PLEDGE_ID => Ok(Self::Pledge(BalanceDecrement::decode(input)?)),
            UNPLEDGE_ID => Ok(Self::Unpledge(BalanceIncrement::decode(input)?)),
            _ => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "unknown packet")),
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
    arbitrary::Arbitrary,
)]
pub struct BlockRewardIncrement {
    /// Amount to increment to the target account.
    pub amount: U256,
    /// Target account address.
    pub target: Address,
}

impl BalanceDecrement {
    fn encode<W: Write>(&self, mut w: W) -> std::io::Result<()> {
        w.write_all(&self.amount.to_le_bytes::<32>())?;
        w.write_all(self.target.as_slice())?;
        w.write_all(self.irys_ref.as_slice())?;
        Ok(())
    }

    fn decode(input: &mut &[u8]) -> std::io::Result<Self> {
        if input.len() < 84 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short"));
        }
        let mut amount_bytes = [0u8; 32];
        amount_bytes.copy_from_slice(&input[..32]);
        *input = &input[32..];
        let amount = U256::from_le_bytes(amount_bytes);

        let mut addr_bytes = [0u8; 20];
        addr_bytes.copy_from_slice(&input[..20]);
        *input = &input[20..];
        let target = Address(FixedBytes::from_slice(&addr_bytes));

        let mut ref_bytes = [0u8; 32];
        ref_bytes.copy_from_slice(&input[..32]);
        *input = &input[32..];
        let irys_ref = FixedBytes::from_slice(&ref_bytes);

        Ok(Self { amount, target, irys_ref })
    }
}

impl BalanceIncrement {
    fn encode<W: Write>(&self, mut w: W) -> std::io::Result<()> {
        w.write_all(&self.amount.to_le_bytes::<32>())?;
        w.write_all(self.target.as_slice())?;
        w.write_all(self.irys_ref.as_slice())?;
        Ok(())
    }

    fn decode(input: &mut &[u8]) -> std::io::Result<Self> {
        if input.len() < 84 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short"));
        }
        let mut amount_bytes = [0u8; 32];
        amount_bytes.copy_from_slice(&input[..32]);
        *input = &input[32..];
        let amount = U256::from_le_bytes(amount_bytes);

        let mut addr_bytes = [0u8; 20];
        addr_bytes.copy_from_slice(&input[..20]);
        *input = &input[20..];
        let target = Address(FixedBytes::from_slice(&addr_bytes));

        let mut ref_bytes = [0u8; 32];
        ref_bytes.copy_from_slice(&input[..32]);
        *input = &input[32..];
        let irys_ref = FixedBytes::from_slice(&ref_bytes);

        Ok(Self { amount, target, irys_ref })
    }
}

impl BlockRewardIncrement {
    fn encode<W: Write>(&self, mut w: W) -> std::io::Result<()> {
        w.write_all(&self.amount.to_le_bytes::<32>())?;
        w.write_all(self.target.as_slice())?;
        Ok(())
    }

    fn decode(input: &mut &[u8]) -> std::io::Result<Self> {
        if input.len() < 52 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short"));
        }
        let mut amount_bytes = [0u8; 32];
        amount_bytes.copy_from_slice(&input[..32]);
        *input = &input[32..];
        let amount = U256::from_le_bytes(amount_bytes);

        let mut addr_bytes = [0u8; 20];
        addr_bytes.copy_from_slice(&input[..20]);
        *input = &input[20..];
        let target = Address(FixedBytes::from_slice(&addr_bytes));

        Ok(Self { amount, target })
    }
}
