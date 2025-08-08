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
use borsh::{BorshDeserialize, BorshSerialize};
use std::io::{Read, Write};
use std::sync::LazyLock;

/// Version constants for ShadowTransaction
pub const SHADOW_TX_VERSION_V1: u8 = 1;

/// Current version of ShadowTransaction
pub const CURRENT_SHADOW_TX_VERSION: u8 = SHADOW_TX_VERSION_V1;

/// Prefix used to identify encoded shadow transactions in a regular
/// transaction's input field.
pub const IRYS_SHADOW_EXEC: &[u8; 16] = b"irys-shadow-exec";

/// Address that all shadow transactions must target.
///
/// This ensures shadow transactions cannot be executed accidentally by regular
/// EVM tooling.
pub static SHADOW_TX_DESTINATION_ADDR: LazyLock<Address> =
    LazyLock::new(|| Address::from_word(keccak256("irys_shadow_tx_processor")));

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
    Unstake(EitherIncrementOrDecrement),
    /// Block reward payment to the block producer (balance increment). Must be validated by CL.
    BlockReward(BlockRewardIncrement),
    /// Stake funds from an account (balance decrement). Used for staking operations.
    Stake(BalanceDecrement),
    /// Collect storage fees from an account (balance decrement). Must match storage usage.
    StorageFees(BalanceDecrement),
    /// Pledge funds to an account (balance decrement). Used for pledging operations.
    Pledge(BalanceDecrement),
    /// Unpledge funds from an account (balance increment). Used for unpledging operations.
    Unpledge(EitherIncrementOrDecrement),
    /// Term fee reward payment to ingress proof providers and block producer (balance increment).
    TermFeeReward(BalanceIncrement),
    /// Ingress proof reward payment to providers who submitted valid proofs (balance increment).
    IngressProofReward(BalanceIncrement),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, arbitrary::Arbitrary)]
pub enum EitherIncrementOrDecrement {
    BalanceIncrement(BalanceIncrement),
    BalanceDecrement(BalanceDecrement),
}

impl TransactionPacket {
    /// Returns the target address for this transaction packet, if any.
    /// Returns None for BlockReward since it has no explicit target (uses beneficiary).
    pub fn fee_payer_address(&self) -> Option<Address> {
        match self {
            Self::Unstake(either) => match either {
                EitherIncrementOrDecrement::BalanceIncrement(inc) => Some(inc.target),
                EitherIncrementOrDecrement::BalanceDecrement(dec) => Some(dec.target),
            },
            Self::BlockReward(_) => None, // No target, uses beneficiary
            Self::Stake(dec) => Some(dec.target),
            Self::StorageFees(dec) => Some(dec.target),
            Self::Pledge(dec) => Some(dec.target),
            Self::Unpledge(either) => match either {
                EitherIncrementOrDecrement::BalanceIncrement(inc) => Some(inc.target),
                EitherIncrementOrDecrement::BalanceDecrement(dec) => Some(dec.target),
            },
            Self::TermFeeReward(inc) => Some(inc.target),
            Self::IngressProofReward(inc) => Some(inc.target),
        }
    }
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
    pub static TERM_FEE_REWARD: LazyLock<[u8; 32]> =
        LazyLock::new(|| keccak256("SHADOW_TX_TERM_FEE_REWARD").0);
    pub static INGRESS_PROOF_REWARD: LazyLock<[u8; 32]> =
        LazyLock::new(|| keccak256("SHADOW_TX_INGRESS_PROOF_REWARD").0);
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

    /// Decode a shadow transaction from a buffer that contains the
    /// [`IRYS_SHADOW_EXEC`] prefix followed by the borsh-encoded transaction.
    #[expect(
        clippy::indexing_slicing,
        reason = "prefix length checked before slicing"
    )]
    pub fn decode(buf: &mut &[u8]) -> borsh::io::Result<Self> {
        if buf.len() < IRYS_SHADOW_EXEC.len() || &buf[..IRYS_SHADOW_EXEC.len()] != IRYS_SHADOW_EXEC
        {
            return Err(borsh::io::Error::new(
                borsh::io::ErrorKind::InvalidData,
                "Missing shadow tx prefix",
            ));
        }
        *buf = &buf[IRYS_SHADOW_EXEC.len()..];
        <Self as BorshDeserialize>::deserialize_reader(buf)
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
            Self::TermFeeReward(_) => (*TERM_FEE_REWARD).into(),
            Self::IngressProofReward(_) => (*INGRESS_PROOF_REWARD).into(),
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
pub const TERM_FEE_REWARD_ID: u8 = 0x07;
pub const INGRESS_PROOF_REWARD_ID: u8 = 0x08;

/// Discriminants for EitherIncrementOrDecrement
pub const EITHER_INCREMENT_ID: u8 = 0x01;
pub const EITHER_DECREMENT_ID: u8 = 0x02;

impl BorshSerialize for ShadowTransaction {
    fn serialize<W: Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        match self {
            Self::V1 { packet } => {
                writer.write_all(&[SHADOW_TX_VERSION_V1])?;
                packet.serialize(writer)
            }
        }
    }
}

impl BorshDeserialize for ShadowTransaction {
    fn deserialize_reader<R: Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let mut version = [0_u8; 1];
        reader.read_exact(&mut version)?;
        match version[0] {
            SHADOW_TX_VERSION_V1 => {
                let packet = TransactionPacket::deserialize_reader(reader)?;
                Ok(Self::V1 { packet })
            }
            _ => Err(borsh::io::Error::new(
                borsh::io::ErrorKind::InvalidData,
                "Unknown shadow transaction version",
            )),
        }
    }
}

impl BorshSerialize for TransactionPacket {
    fn serialize<W: Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        match self {
            Self::Unstake(inner) => {
                writer.write_all(&[UNSTAKE_ID])?;
                inner.serialize(writer)
            }
            Self::BlockReward(inner) => {
                writer.write_all(&[BLOCK_REWARD_ID])?;
                inner.serialize(writer)
            }
            Self::Stake(inner) => {
                writer.write_all(&[STAKE_ID])?;
                inner.serialize(writer)
            }
            Self::StorageFees(inner) => {
                writer.write_all(&[STORAGE_FEES_ID])?;
                inner.serialize(writer)
            }
            Self::Pledge(inner) => {
                writer.write_all(&[PLEDGE_ID])?;
                inner.serialize(writer)
            }
            Self::Unpledge(inner) => {
                writer.write_all(&[UNPLEDGE_ID])?;
                inner.serialize(writer)
            }
            Self::TermFeeReward(inner) => {
                writer.write_all(&[TERM_FEE_REWARD_ID])?;
                inner.serialize(writer)
            }
            Self::IngressProofReward(inner) => {
                writer.write_all(&[INGRESS_PROOF_REWARD_ID])?;
                inner.serialize(writer)
            }
        }
    }
}

impl BorshDeserialize for TransactionPacket {
    fn deserialize_reader<R: Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let mut disc = [0_u8; 1];
        reader.read_exact(&mut disc)?;
        Ok(match disc[0] {
            UNSTAKE_ID => Self::Unstake(EitherIncrementOrDecrement::deserialize_reader(reader)?),
            BLOCK_REWARD_ID => Self::BlockReward(BlockRewardIncrement::deserialize_reader(reader)?),
            STAKE_ID => Self::Stake(BalanceDecrement::deserialize_reader(reader)?),
            STORAGE_FEES_ID => Self::StorageFees(BalanceDecrement::deserialize_reader(reader)?),
            PLEDGE_ID => Self::Pledge(BalanceDecrement::deserialize_reader(reader)?),
            UNPLEDGE_ID => Self::Unpledge(EitherIncrementOrDecrement::deserialize_reader(reader)?),
            TERM_FEE_REWARD_ID => {
                Self::TermFeeReward(BalanceIncrement::deserialize_reader(reader)?)
            }
            INGRESS_PROOF_REWARD_ID => {
                Self::IngressProofReward(BalanceIncrement::deserialize_reader(reader)?)
            }
            _ => {
                return Err(borsh::io::Error::new(
                    borsh::io::ErrorKind::InvalidData,
                    "Unknown shadow tx discriminant",
                ))
            }
        })
    }
}

impl BorshSerialize for EitherIncrementOrDecrement {
    fn serialize<W: Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        match self {
            Self::BalanceIncrement(inner) => {
                writer.write_all(&[EITHER_INCREMENT_ID])?;
                inner.serialize(writer)
            }
            Self::BalanceDecrement(inner) => {
                writer.write_all(&[EITHER_DECREMENT_ID])?;
                inner.serialize(writer)
            }
        }
    }
}

impl BorshDeserialize for EitherIncrementOrDecrement {
    fn deserialize_reader<R: Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let mut disc = [0_u8; 1];
        reader.read_exact(&mut disc)?;
        Ok(match disc[0] {
            EITHER_INCREMENT_ID => {
                Self::BalanceIncrement(BalanceIncrement::deserialize_reader(reader)?)
            }
            EITHER_DECREMENT_ID => {
                Self::BalanceDecrement(BalanceDecrement::deserialize_reader(reader)?)
            }
            _ => {
                return Err(borsh::io::Error::new(
                    borsh::io::ErrorKind::InvalidData,
                    "Unknown EitherIncrementOrDecrement discriminant",
                ))
            }
        })
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
    // manual Borsh impls below
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

impl BorshSerialize for BalanceDecrement {
    fn serialize<W: Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        writer.write_all(&self.amount.to_be_bytes::<32>())?;
        writer.write_all(self.target.as_slice())?;
        writer.write_all(self.irys_ref.as_slice())?;
        Ok(())
    }
}

impl BorshDeserialize for BalanceDecrement {
    fn deserialize_reader<R: Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let mut amount_buf = [0_u8; 32];
        reader.read_exact(&mut amount_buf)?;
        let amount = U256::from_be_bytes(amount_buf);
        let mut addr = [0_u8; 20];
        reader.read_exact(&mut addr)?;
        let target = Address::from_slice(&addr);
        let mut ref_buf = [0_u8; 32];
        reader.read_exact(&mut ref_buf)?;
        let irys_ref = FixedBytes::<32>::from_slice(&ref_buf);
        Ok(Self {
            amount,
            target,
            irys_ref,
        })
    }
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
    // manual Borsh impls below
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

impl BorshSerialize for BalanceIncrement {
    fn serialize<W: Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        writer.write_all(&self.amount.to_be_bytes::<32>())?;
        writer.write_all(self.target.as_slice())?;
        writer.write_all(self.irys_ref.as_slice())?;
        Ok(())
    }
}

impl BorshDeserialize for BalanceIncrement {
    fn deserialize_reader<R: Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let mut amount_buf = [0_u8; 32];
        reader.read_exact(&mut amount_buf)?;
        let amount = U256::from_be_bytes(amount_buf);
        let mut addr = [0_u8; 20];
        reader.read_exact(&mut addr)?;
        let target = Address::from_slice(&addr);
        let mut ref_buf = [0_u8; 32];
        reader.read_exact(&mut ref_buf)?;
        let irys_ref = FixedBytes::<32>::from_slice(&ref_buf);
        Ok(Self {
            amount,
            target,
            irys_ref,
        })
    }
}

/// Block reward increment: used for block reward shadow txs (no irys_ref needed).
/// The target is always the block beneficiary and is determined during execution.
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
    // manual Borsh impls below
    arbitrary::Arbitrary,
)]
pub struct BlockRewardIncrement {
    /// Amount to increment to the beneficiary account.
    pub amount: U256,
}

impl BorshSerialize for BlockRewardIncrement {
    fn serialize<W: Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        writer.write_all(&self.amount.to_be_bytes::<32>())?;
        Ok(())
    }
}

impl BorshDeserialize for BlockRewardIncrement {
    fn deserialize_reader<R: Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let mut amount_buf = [0_u8; 32];
        reader.read_exact(&mut amount_buf)?;
        let amount = U256::from_be_bytes(amount_buf);
        Ok(Self { amount })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm_primitives::hex_literal::hex;

    /// Serialize and deserialize a `BlockReward` packet to ensure
    /// the borsh encoding stays stable.
    #[test]
    fn block_reward_roundtrip() {
        let tx = ShadowTransaction::new_v1(TransactionPacket::BlockReward(BlockRewardIncrement {
            amount: U256::from(123_u64),
        }));
        let mut buf = Vec::new();
        tx.serialize(&mut buf).unwrap();
        let expected = hex!(
            "01" "02"
            "000000000000000000000000000000000000000000000000000000000000007b"
        );
        assert_eq!(buf, expected);
        let decoded = ShadowTransaction::deserialize_reader(&mut &buf[..]).unwrap();
        assert_eq!(decoded, tx);
    }

    /// Check that `Stake` packets roundtrip correctly through borsh
    /// serialization and deserialization.
    #[test]
    fn stake_roundtrip() {
        let tx = ShadowTransaction::new_v1(TransactionPacket::Stake(BalanceDecrement {
            amount: U256::from(123456789_u64),
            target: Address::repeat_byte(0x22),
            irys_ref: FixedBytes::<32>::from_slice(&[0xaa; 32]),
        }));
        let mut buf = Vec::new();
        tx.serialize(&mut buf).unwrap();
        let expected = hex!(
            "01" "03"
            "00000000000000000000000000000000000000000000000000000000075bcd15"
            "2222222222222222222222222222222222222222"
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(buf, expected, "encoding mismatch");
        let decoded = ShadowTransaction::deserialize_reader(&mut &buf[..]).unwrap();
        assert_eq!(decoded, tx, "decoding mismatch");
    }

    /// Verify that a shadow transaction prefixed with `IRYS_SHADOW_EXEC`
    /// can be decoded via `decode_prefixed`.
    #[test]
    fn decode_prefixed_roundtrip() {
        let tx = ShadowTransaction::new_v1(TransactionPacket::BlockReward(BlockRewardIncrement {
            amount: U256::from(1_u64),
        }));
        let mut buf = Vec::from(IRYS_SHADOW_EXEC.as_slice());
        tx.serialize(&mut buf).unwrap();
        let decoded = ShadowTransaction::decode(&mut &buf[..]).unwrap();
        assert_eq!(decoded, tx, "decoding mismatch");
    }
}
