//! # System Transactions
//!
//! This module defines the system transaction types used in the Irys protocol. System transactions
//! are special EVM transactions that encode protocol-level actions, such as block rewards, storage
//! fee collection, stake management, and nonce resets. The Irys Consensus Layer (CL) is responsible
//! for validating these transactions in every block, ensuring protocol rules are enforced:
//!
//! - **Block rewards** must go to the Irys block producer
//! - **Balance increments** correspond to rewards
//! - **Balance decrements** correspond to storage transaction fees
//! - **Every block must end with a nonce reset system tx**

use alloy_primitives::keccak256;
use alloy_primitives::Address;
use alloy_primitives::FixedBytes;
use alloy_primitives::U256;
use alloy_rlp::Decodable;
use alloy_rlp::Encodable;
use alloy_rlp::{RlpDecodable, RlpEncodable};
use bytes;
use std::sync::LazyLock;

/// A system transaction, valid for a single block, encoding a protocol-level action.
#[derive(
    Debug, Clone, RlpEncodable, RlpDecodable, PartialEq, Eq, PartialOrd, Ord, arbitrary::Arbitrary,
)]
pub struct SystemTransaction {
    /// The block height for which this system tx is valid.
    pub valid_for_block_height: u64,
    /// The parent block hash to ensure the tx is not replayed on forks.
    pub parent_blockhash: FixedBytes<32>,
    /// The actual system transaction packet (see `TransactionPacket`).
    pub inner: TransactionPacket,
}

/// Enum of all supported system transaction types in Irys protocol.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, arbitrary::Arbitrary)]
pub enum TransactionPacket {
    /// Release staked funds to an account (balance increment). Used for unstaking or protocol rewards.
    ReleaseStake(BalanceIncrement),
    /// Block reward payment to the block producer (balance increment). Must be validated by CL.
    BlockReward(BalanceIncrement),
    /// Stake funds from an account (balance decrement). Used for staking operations.
    Stake(BalanceDecrement),
    /// Collect storage fees from an account (balance decrement). Must match storage usage.
    StorageFees(BalanceDecrement),
    /// Reset the system tx nonce for the block producer. Must always be the last system tx in a block.
    ResetSystemTxNonce(ResetSystemTxNonce),
}

/// Topics for system transaction logs
#[expect(
    clippy::module_name_repetitions,
    reason = "module name in type name provides clarity"
)]
pub mod system_tx_topics {
    use super::*;

    pub static RELEASE_STAKE: LazyLock<[u8; 32]> =
        LazyLock::new(|| keccak256("SYSTEM_TX_RELEASE_STAKE").0);
    pub static BLOCK_REWARD: LazyLock<[u8; 32]> =
        LazyLock::new(|| keccak256("SYSTEM_TX_BLOCK_REWARD").0);
    pub static STAKE: LazyLock<[u8; 32]> = LazyLock::new(|| keccak256("SYSTEM_TX_STAKE").0);
    pub static STORAGE_FEES: LazyLock<[u8; 32]> =
        LazyLock::new(|| keccak256("SYSTEM_TX_STORAGE_FEES").0);
    pub static RESET_SYSTEM_TX_NONCE: LazyLock<[u8; 32]> =
        LazyLock::new(|| keccak256("RESET_SYSTEM_TX_NONCE").0);
}

impl TransactionPacket {
    /// Get the topic for this transaction packet.
    #[must_use]
    pub fn topic(&self) -> FixedBytes<32> {
        use system_tx_topics::*;
        match self {
            Self::ReleaseStake(_) => (*RELEASE_STAKE).into(),
            Self::BlockReward(_) => (*BLOCK_REWARD).into(),
            Self::Stake(_) => (*STAKE).into(),
            Self::StorageFees(_) => (*STORAGE_FEES).into(),
            Self::ResetSystemTxNonce(_) => (*RESET_SYSTEM_TX_NONCE).into(),
        }
    }

    /// Get the encoded topic for this transaction packet.
    #[must_use]
    pub fn encoded_topic(&self) -> [u8; 32] {
        match self {
            Self::ReleaseStake(bi) | Self::BlockReward(bi) => {
                use alloy_dyn_abi::DynSolValue;
                DynSolValue::Tuple(vec![
                    DynSolValue::Uint(bi.amount, 256),
                    DynSolValue::Address(bi.target),
                ])
                .abi_encode_packed()
                .try_into()
                .unwrap_or_default()
            }
            Self::Stake(bd) | Self::StorageFees(bd) => {
                use alloy_dyn_abi::DynSolValue;
                DynSolValue::Tuple(vec![
                    DynSolValue::Uint(bd.amount, 256),
                    DynSolValue::Address(bd.target),
                ])
                .abi_encode_packed()
                .try_into()
                .unwrap_or_default()
            }
            Self::ResetSystemTxNonce(inner) => {
                use alloy_dyn_abi::DynSolValue;
                DynSolValue::Uint(U256::from(inner.decrement_nonce_by), 256)
                    .abi_encode_packed()
                    .try_into()
                    .unwrap_or_default()
            }
        }
    }
}

/// Stable 1-byte discriminants
pub const RELEASE_STAKE_ID: u8 = 0x00;
pub const BLOCK_REWARD_ID: u8 = 0x01;
pub const STAKE_ID: u8 = 0x02;
pub const STORAGE_FEES_ID: u8 = 0x03;
pub const RESET_SYS_SIGNER_NONCE_ID: u8 = 0x04;

#[expect(
    clippy::arithmetic_side_effects,
    reason = "length calculation is safe for small values"
)]
impl Encodable for TransactionPacket {
    fn length(&self) -> usize {
        1 + match self {
            Self::ReleaseStake(bi) | Self::BlockReward(bi) => bi.length(),
            Self::Stake(bd) | Self::StorageFees(bd) => bd.length(),
            Self::ResetSystemTxNonce(inner) => inner.length(),
        }
    }

    fn encode(&self, out: &mut dyn bytes::BufMut) {
        match self {
            Self::ReleaseStake(inner) => {
                out.put_u8(RELEASE_STAKE_ID);
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
            Self::ResetSystemTxNonce(inner) => {
                out.put_u8(RESET_SYS_SIGNER_NONCE_ID);
                inner.encode(out);
            }
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
            RELEASE_STAKE_ID => {
                let inner = BalanceIncrement::decode(buf)?;
                Ok(Self::ReleaseStake(inner))
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
            RESET_SYS_SIGNER_NONCE_ID => {
                let inner = ResetSystemTxNonce::decode(buf)?;
                Ok(Self::ResetSystemTxNonce(inner))
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
impl Default for TransactionPacket {
    fn default() -> Self {
        unimplemented!("relying on the default impl for `SYSTEM_TX` is a critical bug")
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

/// Balance increment: used for block rewards and stake release system txs.
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

/// Nonce reset: decrements the system tx nonce for the block producer. Must be the last system tx in a block.
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
pub struct ResetSystemTxNonce {
    /// Amount to decrement the nonce by.
    pub decrement_nonce_by: u64,
}
