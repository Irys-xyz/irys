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

use alloy_consensus::Transaction as AlloyTransaction;
use alloy_primitives::keccak256;
use alloy_primitives::{Address, Bytes, FixedBytes, U256};
use borsh::{BorshDeserialize, BorshSerialize};
use std::io::{Read, Write};
use std::sync::LazyLock;
use thiserror::Error;

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
    /// Version 1 shadow transaction format with solution hash support
    ///
    V1 {
        /// The actual shadow transaction packet.
        packet: TransactionPacket,
        /// Solution hash for validation
        solution_hash: FixedBytes<32>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, arbitrary::Arbitrary)]
pub enum TransactionPacket {
    /// Unstake at inclusion: fee-only via priority fee. No amount in packet; log-only.
    UnstakeDebit(UnstakeDebit),
    /// Block reward payment to the block producer (balance increment). Must be validated by CL.
    BlockReward(BlockRewardIncrement),
    /// Stake funds from an account (balance decrement). Used for staking operations.
    Stake(BalanceDecrement),
    /// Collect storage fees from an account (balance decrement). Must match storage usage.
    StorageFees(BalanceDecrement),
    /// Pledge funds to an account (balance decrement). Used for pledging operations.
    Pledge(BalanceDecrement),
    /// Unpledge at inclusion: fee-only via priority fee. No amount in packet.
    /// Refund occurs in epoch via UnpledgeRefund.
    Unpledge(UnpledgeDebit),
    /// Unpledge refund at epoch (balance increment). Emitted with zero priority fee.
    UnpledgeRefund(BalanceIncrement),
    /// Term fee reward to the miners that stored the block
    TermFeeReward(BalanceIncrement),
    /// Ingress proof reward payment to providers who submitted valid proofs (balance increment).
    IngressProofReward(BalanceIncrement),
    /// Permanent fee refund to users whose transactions were not promoted before ledger expiry (balance increment).
    PermFeeRefund(BalanceIncrement),
    /// Unstake funds to an account (balance increment). Executed at epoch for refunds.
    UnstakeRefund(BalanceIncrement),
    /// Set the PD base fee per chunk in EVM state (protocol metadata update).
    PdBaseFeeUpdate(PdBaseFeeUpdate),
    /// Update the IRYS/USD price in EVM state for minimum cost validation.
    IrysUsdPriceUpdate(IrysUsdPriceUpdate),
    /// Deposit funds into the protocol treasury.
    TreasuryDeposit(TreasuryDeposit),
    /// Update reward address at inclusion: fee-only via priority fee. No amount in packet; log-only.
    UpdateRewardAddress(UpdateRewardAddressDebit),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, arbitrary::Arbitrary)]
pub enum EitherIncrementOrDecrement {
    BalanceIncrement(BalanceIncrement),
    BalanceDecrement(BalanceDecrement),
}

/// Inclusion-time Unstake record: fee-only via priority fee, no balance change.
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
pub struct UnstakeDebit {
    /// Target account address (fee payer for priority fee).
    pub target: Address,
    /// Reference to the consensus layer transaction that resulted in this shadow tx.
    pub irys_ref: FixedBytes<32>,
}

/// Inclusion-time UpdateRewardAddress record: fee-only via priority fee, no balance change.
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
pub struct UpdateRewardAddressDebit {
    /// Target account address (fee payer for priority fee).
    pub target: Address,
    /// Reference to the consensus layer transaction that resulted in this shadow tx.
    pub irys_ref: FixedBytes<32>,
    /// New reward address being set.
    pub new_reward_address: Address,
}

impl TransactionPacket {
    /// Returns the target address for this transaction packet, if any.
    /// Returns None for BlockReward since it has no explicit target (uses beneficiary).
    pub fn fee_payer_address(&self) -> Option<Address> {
        match self {
            Self::UnstakeRefund(inc) => Some(inc.target),
            Self::UnstakeDebit(dec) => Some(dec.target),
            Self::BlockReward(_) => None, // No target, uses beneficiary
            Self::Stake(dec) => Some(dec.target),
            Self::StorageFees(dec) => Some(dec.target),
            Self::Pledge(dec) => Some(dec.target),
            Self::Unpledge(dec) => Some(dec.target),
            Self::UnpledgeRefund(inc) => Some(inc.target),
            Self::TermFeeReward(inc) => Some(inc.target),
            Self::IngressProofReward(inc) => Some(inc.target),
            Self::PermFeeRefund(inc) => Some(inc.target),
            Self::PdBaseFeeUpdate(_) => None, // Protocol-level update, no fee payer
            Self::IrysUsdPriceUpdate(_) => None, // Protocol-level update, no fee payer
            Self::TreasuryDeposit(_) => None, // Protocol-level update, no fee payer
            Self::UpdateRewardAddress(dec) => Some(dec.target),
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

    pub static UNSTAKE: LazyLock<FixedBytes<32>> = LazyLock::new(|| keccak256("SHADOW_TX_UNSTAKE"));
    pub static UNSTAKE_DEBIT: LazyLock<FixedBytes<32>> =
        LazyLock::new(|| keccak256("SHADOW_TX_UNSTAKE_DEBIT"));
    pub static BLOCK_REWARD: LazyLock<FixedBytes<32>> =
        LazyLock::new(|| keccak256("SHADOW_TX_BLOCK_REWARD"));
    pub static STAKE: LazyLock<FixedBytes<32>> = LazyLock::new(|| keccak256("SHADOW_TX_STAKE"));
    pub static STORAGE_FEES: LazyLock<FixedBytes<32>> =
        LazyLock::new(|| keccak256("SHADOW_TX_STORAGE_FEES"));
    pub static PLEDGE: LazyLock<FixedBytes<32>> = LazyLock::new(|| keccak256("SHADOW_TX_PLEDGE"));
    pub static UNPLEDGE: LazyLock<FixedBytes<32>> =
        LazyLock::new(|| keccak256("SHADOW_TX_UNPLEDGE"));
    pub static UNPLEDGE_REFUND: LazyLock<FixedBytes<32>> =
        LazyLock::new(|| keccak256("SHADOW_TX_UNPLEDGE_REFUND"));
    pub static TERM_FEE_REWARD: LazyLock<FixedBytes<32>> =
        LazyLock::new(|| keccak256("SHADOW_TX_TERM_FEE_REWARD"));
    pub static INGRESS_PROOF_REWARD: LazyLock<FixedBytes<32>> =
        LazyLock::new(|| keccak256("SHADOW_TX_INGRESS_PROOF_REWARD"));
    pub static PERM_FEE_REFUND: LazyLock<FixedBytes<32>> =
        LazyLock::new(|| keccak256("SHADOW_TX_PERM_FEE_REFUND"));
    pub static PD_BASE_FEE_UPDATE: LazyLock<FixedBytes<32>> =
        LazyLock::new(|| keccak256("SHADOW_TX_PD_BASE_FEE_UPDATE"));
    pub static IRYS_USD_PRICE_UPDATE: LazyLock<FixedBytes<32>> =
        LazyLock::new(|| keccak256("SHADOW_TX_IRYS_USD_PRICE_UPDATE"));
    pub static TREASURY_DEPOSIT: LazyLock<FixedBytes<32>> =
        LazyLock::new(|| keccak256("SHADOW_TX_TREASURY_DEPOSIT"));
    pub static UPDATE_REWARD_ADDRESS: LazyLock<FixedBytes<32>> =
        LazyLock::new(|| keccak256("SHADOW_TX_UPDATE_REWARD_ADDRESS"));
}

impl ShadowTransaction {
    /// Create a new V1 shadow transaction with solution hash
    #[must_use]
    pub fn new_v1(packet: TransactionPacket, solution_hash: FixedBytes<32>) -> Self {
        Self::V1 {
            packet,
            solution_hash,
        }
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
            Self::UnstakeRefund(_) => *UNSTAKE,
            Self::UnstakeDebit(_) => *UNSTAKE_DEBIT,
            Self::BlockReward(_) => *BLOCK_REWARD,
            Self::Stake(_) => *STAKE,
            Self::StorageFees(_) => *STORAGE_FEES,
            Self::Pledge(_) => *PLEDGE,
            Self::Unpledge(_) => *UNPLEDGE,
            Self::UnpledgeRefund(_) => *UNPLEDGE_REFUND,
            Self::TermFeeReward(_) => *TERM_FEE_REWARD,
            Self::IngressProofReward(_) => *INGRESS_PROOF_REWARD,
            Self::PermFeeRefund(_) => *PERM_FEE_REFUND,
            Self::PdBaseFeeUpdate(_) => *PD_BASE_FEE_UPDATE,
            Self::IrysUsdPriceUpdate(_) => *IRYS_USD_PRICE_UPDATE,
            Self::TreasuryDeposit(_) => *TREASURY_DEPOSIT,
            Self::UpdateRewardAddress(_) => *UPDATE_REWARD_ADDRESS,
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
pub const PERM_FEE_REFUND_ID: u8 = 0x09;
pub const UNPLEDGE_REFUND_ID: u8 = 0x0A;
pub const UNSTAKE_DEBIT_ID: u8 = 0x0B;
pub const PD_BASE_FEE_UPDATE_ID: u8 = 0x0C;
pub const IRYS_USD_PRICE_UPDATE_ID: u8 = 0x0D;
pub const TREASURY_DEPOSIT_ID: u8 = 0x0E;
pub const UPDATE_REWARD_ADDRESS_ID: u8 = 0x0F;

/// Discriminants for EitherIncrementOrDecrement
pub const EITHER_INCREMENT_ID: u8 = 0x01;
pub const EITHER_DECREMENT_ID: u8 = 0x02;

impl BorshSerialize for ShadowTransaction {
    fn serialize<W: Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        match self {
            Self::V1 {
                packet,
                solution_hash,
            } => {
                writer.write_all(&[SHADOW_TX_VERSION_V1])?;
                packet.serialize(writer)?;
                writer.write_all(solution_hash.as_slice())?;
                Ok(())
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
                let mut hash_buf = [0_u8; 32];
                reader.read_exact(&mut hash_buf)?;
                let solution_hash = FixedBytes::<32>::from_slice(&hash_buf);
                Ok(Self::V1 {
                    packet,
                    solution_hash,
                })
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
            Self::UnstakeRefund(inner) => {
                writer.write_all(&[UNSTAKE_ID])?;
                inner.serialize(writer)
            }
            Self::UnstakeDebit(inner) => {
                writer.write_all(&[UNSTAKE_DEBIT_ID])?;
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
            Self::PermFeeRefund(inner) => {
                writer.write_all(&[PERM_FEE_REFUND_ID])?;
                inner.serialize(writer)
            }
            Self::UnpledgeRefund(inner) => {
                writer.write_all(&[UNPLEDGE_REFUND_ID])?;
                inner.serialize(writer)
            }
            Self::PdBaseFeeUpdate(inner) => {
                writer.write_all(&[PD_BASE_FEE_UPDATE_ID])?;
                inner.serialize(writer)
            }
            Self::IrysUsdPriceUpdate(inner) => {
                writer.write_all(&[IRYS_USD_PRICE_UPDATE_ID])?;
                inner.serialize(writer)
            }
            Self::TreasuryDeposit(inner) => {
                writer.write_all(&[TREASURY_DEPOSIT_ID])?;
                inner.serialize(writer)
            }
            Self::UpdateRewardAddress(inner) => {
                writer.write_all(&[UPDATE_REWARD_ADDRESS_ID])?;
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
            UNSTAKE_ID => Self::UnstakeRefund(BalanceIncrement::deserialize_reader(reader)?),
            UNSTAKE_DEBIT_ID => Self::UnstakeDebit(UnstakeDebit::deserialize_reader(reader)?),
            BLOCK_REWARD_ID => Self::BlockReward(BlockRewardIncrement::deserialize_reader(reader)?),
            STAKE_ID => Self::Stake(BalanceDecrement::deserialize_reader(reader)?),
            STORAGE_FEES_ID => Self::StorageFees(BalanceDecrement::deserialize_reader(reader)?),
            PLEDGE_ID => Self::Pledge(BalanceDecrement::deserialize_reader(reader)?),
            UNPLEDGE_ID => Self::Unpledge(UnpledgeDebit::deserialize_reader(reader)?),
            TERM_FEE_REWARD_ID => {
                Self::TermFeeReward(BalanceIncrement::deserialize_reader(reader)?)
            }
            INGRESS_PROOF_REWARD_ID => {
                Self::IngressProofReward(BalanceIncrement::deserialize_reader(reader)?)
            }
            PERM_FEE_REFUND_ID => {
                Self::PermFeeRefund(BalanceIncrement::deserialize_reader(reader)?)
            }
            UNPLEDGE_REFUND_ID => {
                Self::UnpledgeRefund(BalanceIncrement::deserialize_reader(reader)?)
            }
            PD_BASE_FEE_UPDATE_ID => {
                Self::PdBaseFeeUpdate(PdBaseFeeUpdate::deserialize_reader(reader)?)
            }
            IRYS_USD_PRICE_UPDATE_ID => {
                Self::IrysUsdPriceUpdate(IrysUsdPriceUpdate::deserialize_reader(reader)?)
            }
            TREASURY_DEPOSIT_ID => {
                Self::TreasuryDeposit(TreasuryDeposit::deserialize_reader(reader)?)
            }
            UPDATE_REWARD_ADDRESS_ID => {
                Self::UpdateRewardAddress(UpdateRewardAddressDebit::deserialize_reader(reader)?)
            }
            _ => {
                return Err(borsh::io::Error::new(
                    borsh::io::ErrorKind::InvalidData,
                    "Unknown shadow tx discriminant",
                ));
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
                ));
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

/// Unpledge at inclusion time: fee-only via priority fee, no direct balance change.
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
pub struct UnpledgeDebit {
    /// Target account address (fee payer for priority fee).
    pub target: Address,
    /// Reference to the consensus layer transaction that resulted in this shadow tx.
    pub irys_ref: FixedBytes<32>,
}

impl BorshSerialize for UnpledgeDebit {
    fn serialize<W: Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        writer.write_all(self.target.as_slice())?;
        writer.write_all(self.irys_ref.as_slice())?;
        Ok(())
    }
}

impl BorshDeserialize for UnpledgeDebit {
    fn deserialize_reader<R: Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let mut addr = [0_u8; 20];
        reader.read_exact(&mut addr)?;
        let target = Address::from_slice(&addr);
        let mut ref_buf = [0_u8; 32];
        reader.read_exact(&mut ref_buf)?;
        let irys_ref = FixedBytes::<32>::from_slice(&ref_buf);
        Ok(Self { target, irys_ref })
    }
}

impl BorshSerialize for UnstakeDebit {
    fn serialize<W: Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        writer.write_all(self.target.as_slice())?;
        writer.write_all(self.irys_ref.as_slice())?;
        Ok(())
    }
}

impl BorshDeserialize for UnstakeDebit {
    fn deserialize_reader<R: Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let mut addr = [0_u8; 20];
        reader.read_exact(&mut addr)?;
        let target = Address::from_slice(&addr);
        let mut ref_buf = [0_u8; 32];
        reader.read_exact(&mut ref_buf)?;
        let irys_ref = FixedBytes::<32>::from_slice(&ref_buf);
        Ok(Self { target, irys_ref })
    }
}

impl BorshSerialize for UpdateRewardAddressDebit {
    fn serialize<W: Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        writer.write_all(self.target.as_slice())?;
        writer.write_all(self.irys_ref.as_slice())?;
        writer.write_all(self.new_reward_address.as_slice())?;
        Ok(())
    }
}

impl BorshDeserialize for UpdateRewardAddressDebit {
    fn deserialize_reader<R: Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let mut addr = [0_u8; 20];
        reader.read_exact(&mut addr)?;
        let target = Address::from_slice(&addr);

        let mut ref_buf = [0_u8; 32];
        reader.read_exact(&mut ref_buf)?;
        let irys_ref = FixedBytes::<32>::from_slice(&ref_buf);

        let mut new_addr = [0_u8; 20];
        reader.read_exact(&mut new_addr)?;
        let new_reward_address = Address::from_slice(&new_addr);

        Ok(Self {
            target,
            irys_ref,
            new_reward_address,
        })
    }
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

/// PD base fee update: sets the protocol-wide PD base fee per chunk.
/// This is a metadata-only shadow tx that updates EVM state without transferring value.
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
pub struct PdBaseFeeUpdate {
    /// Base fee per PD chunk (tokens, 1e18 scale)
    pub per_chunk: U256,
}

impl BorshSerialize for PdBaseFeeUpdate {
    fn serialize<W: Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        writer.write_all(&self.per_chunk.to_be_bytes::<32>())?;
        Ok(())
    }
}

impl BorshDeserialize for PdBaseFeeUpdate {
    fn deserialize_reader<R: Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let mut buf = [0_u8; 32];
        reader.read_exact(&mut buf)?;
        let per_chunk = U256::from_be_bytes(buf);
        Ok(Self { per_chunk })
    }
}

/// IRYS/USD price update: sets the protocol-wide IRYS token price in USD.
/// Used for converting USD-denominated minimum fees to IRYS amounts.
/// This is a metadata-only shadow tx that updates EVM state without transferring value.
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
pub struct IrysUsdPriceUpdate {
    /// Price of 1 IRYS token in USD (1e18 scale, e.g., 1e18 = $1.00)
    pub price: U256,
}

impl BorshSerialize for IrysUsdPriceUpdate {
    fn serialize<W: Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        writer.write_all(&self.price.to_be_bytes::<32>())?;
        Ok(())
    }
}

impl BorshDeserialize for IrysUsdPriceUpdate {
    fn deserialize_reader<R: Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let mut buf = [0_u8; 32];
        reader.read_exact(&mut buf)?;
        let price = U256::from_be_bytes(buf);
        Ok(Self { price })
    }
}

/// Treasury deposit: adds funds to the protocol treasury.
/// This is a protocol-level operation to seed or top-up the treasury.
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
pub struct TreasuryDeposit {
    /// Amount to deposit into treasury (tokens, 1e18 scale)
    pub amount: U256,
}

impl BorshSerialize for TreasuryDeposit {
    fn serialize<W: Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        writer.write_all(&self.amount.to_be_bytes::<32>())?;
        Ok(())
    }
}

impl BorshDeserialize for TreasuryDeposit {
    fn deserialize_reader<R: Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let mut buf = [0_u8; 32];
        reader.read_exact(&mut buf)?;
        let amount = U256::from_be_bytes(buf);
        Ok(Self { amount })
    }
}

/// Errors for shadow transaction detection and decoding.
#[derive(Debug, Error)]
pub enum ShadowTxError {
    #[error("missing shadow tx prefix")]
    MissingPrefix,
    #[error("decode error: {0}")]
    Decode(#[from] borsh::io::Error),
    #[error("shadow tx found after non-shadow tx")]
    ShadowTxAfterNonShadow,
}

/// True if `to` matches `SHADOW_TX_DESTINATION_ADDR`.
pub fn is_shadow_destination(to: Option<Address>) -> bool {
    matches!(to, Some(addr) if addr == *SHADOW_TX_DESTINATION_ADDR)
}

/// True if `input` starts with `IRYS_SHADOW_EXEC`.
pub fn has_shadow_prefix(input: &[u8]) -> bool {
    input.starts_with(IRYS_SHADOW_EXEC)
}

/// Encodes a `ShadowTransaction` into call data with the required prefix.
pub fn encode_prefixed_input(tx: &ShadowTransaction) -> Bytes {
    let mut buf = Vec::with_capacity(IRYS_SHADOW_EXEC.len() + 512);
    buf.extend_from_slice(IRYS_SHADOW_EXEC);
    tx.serialize(&mut buf).expect("Vec::write is infallible");
    buf.into()
}

/// Decodes a prefixed input payload into a `ShadowTransaction`.
/// Returns `MissingPrefix` if the input does not start with the required prefix.
pub fn try_decode_prefixed(input: &[u8]) -> Result<ShadowTransaction, ShadowTxError> {
    if !has_shadow_prefix(input) {
        return Err(ShadowTxError::MissingPrefix);
    }
    let mut slice = input;
    ShadowTransaction::decode(&mut slice).map_err(ShadowTxError::from)
}

/// Detects and decodes a shadow tx from `(to, input)`.
/// Returns `Ok(None)` if it is not a shadow tx; otherwise decodes or returns a decode error.
pub fn detect_and_decode_from_parts(
    to: Option<Address>,
    input: &[u8],
) -> Result<Option<ShadowTransaction>, ShadowTxError> {
    if !is_shadow_destination(to) || !has_shadow_prefix(input) {
        return Ok(None);
    }
    Ok(Some(try_decode_prefixed(input)?))
}

pub trait ShadowTxSource {
    fn to_addr(&self) -> Option<Address>;
    fn input(&self) -> &[u8];
}

impl<T> ShadowTxSource for T
where
    T: AlloyTransaction,
{
    fn to_addr(&self) -> Option<Address> {
        AlloyTransaction::to(self)
    }
    fn input(&self) -> &[u8] {
        AlloyTransaction::input(self)
    }
}

pub fn detect_and_decode<T: ShadowTxSource>(
    src: &T,
) -> Result<Option<ShadowTransaction>, ShadowTxError> {
    detect_and_decode_from_parts(src.to_addr(), src.input())
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm_primitives::hex_literal::hex;

    /// Serialize and deserialize a `BlockReward` packet to ensure
    /// the borsh encoding stays stable.
    #[test]
    fn block_reward_roundtrip() {
        let solution_hash = FixedBytes::<32>::from_slice(&[0xbb; 32]);
        let tx = ShadowTransaction::new_v1(
            TransactionPacket::BlockReward(BlockRewardIncrement {
                amount: U256::from(123_u64),
            }),
            solution_hash,
        );
        let mut buf = Vec::new();
        tx.serialize(&mut buf).unwrap();
        let expected = hex!(
            "01" "02"
            "000000000000000000000000000000000000000000000000000000000000007b"
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
        assert_eq!(buf, expected);
        let decoded = ShadowTransaction::deserialize_reader(&mut &buf[..]).unwrap();
        assert_eq!(decoded, tx);
    }

    /// Check that `Stake` packets roundtrip correctly through borsh
    /// serialization and deserialization.
    #[test]
    fn stake_roundtrip() {
        let solution_hash = FixedBytes::<32>::from_slice(&[0xcc; 32]);
        let tx = ShadowTransaction::new_v1(
            TransactionPacket::Stake(BalanceDecrement {
                amount: U256::from(123456789_u64),
                target: Address::repeat_byte(0x22),
                irys_ref: FixedBytes::<32>::from_slice(&[0xaa; 32]),
            }),
            solution_hash,
        );
        let mut buf = Vec::new();
        tx.serialize(&mut buf).unwrap();
        let expected = hex!(
            "01" "03"
            "00000000000000000000000000000000000000000000000000000000075bcd15"
            "2222222222222222222222222222222222222222"
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
        );
        assert_eq!(buf, expected, "encoding mismatch");
        let decoded = ShadowTransaction::deserialize_reader(&mut &buf[..]).unwrap();
        assert_eq!(decoded, tx, "decoding mismatch");
    }

    /// Verify that a shadow transaction prefixed with `IRYS_SHADOW_EXEC`
    /// can be decoded via `decode_prefixed`.
    #[test]
    fn decode_prefixed_roundtrip() {
        let solution_hash = FixedBytes::<32>::from_slice(&[0xdd; 32]);
        let tx = ShadowTransaction::new_v1(
            TransactionPacket::BlockReward(BlockRewardIncrement {
                amount: U256::from(1_u64),
            }),
            solution_hash,
        );
        let mut buf = Vec::from(IRYS_SHADOW_EXEC.as_slice());
        tx.serialize(&mut buf).unwrap();
        let decoded = ShadowTransaction::decode(&mut &buf[..]).unwrap();
        assert_eq!(decoded, tx, "decoding mismatch");
    }

    /// Check that `PermFeeRefund` packets roundtrip correctly through borsh
    /// serialization and deserialization.
    #[test]
    fn perm_fee_refund_roundtrip() {
        let solution_hash = FixedBytes::<32>::from_slice(&[0xdd; 32]);
        let tx = ShadowTransaction::new_v1(
            TransactionPacket::PermFeeRefund(BalanceIncrement {
                amount: U256::from(500000_u64),
                target: Address::repeat_byte(0x33),
                irys_ref: FixedBytes::<32>::from_slice(&[0xbb; 32]),
            }),
            solution_hash,
        );
        let mut buf = Vec::new();
        tx.serialize(&mut buf).unwrap();
        // Version (01) + Discriminant (09) + Amount (32 bytes) + Target (20 bytes) + IrysRef (32 bytes) + SolutionHash (32 bytes)
        let expected = hex!(
            "01" "09"
            "000000000000000000000000000000000000000000000000000000000007a120"
            "3333333333333333333333333333333333333333"
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
        );
        assert_eq!(buf, expected, "encoding mismatch");
        let decoded = ShadowTransaction::deserialize_reader(&mut &buf[..]).unwrap();
        assert_eq!(decoded, tx, "decoding mismatch");
    }

    /// Verify that `PermFeeRefund` has the correct topic
    #[test]
    fn perm_fee_refund_topic() {
        let solution_hash = FixedBytes::<32>::from_slice(&[0xee; 32]);
        let tx = ShadowTransaction::new_v1(
            TransactionPacket::PermFeeRefund(BalanceIncrement {
                amount: U256::from(1000_u64),
                target: Address::repeat_byte(0x44),
                irys_ref: FixedBytes::<32>::from_slice(&[0xcc; 32]),
            }),
            solution_hash,
        );
        let topic = tx.topic();
        let expected_topic = keccak256("SHADOW_TX_PERM_FEE_REFUND");
        assert_eq!(topic, expected_topic, "topic mismatch");
    }

    /// Test all transaction packet types with solution hash roundtrip
    #[test]
    fn all_packet_types_with_solution_hash_roundtrip() {
        let solution_hash = FixedBytes::<32>::from_slice(&[0xee; 32]);
        let test_address = Address::repeat_byte(0x33);
        let test_ref = FixedBytes::<32>::from_slice(&[0xff; 32]);

        let packets = vec![
            TransactionPacket::BlockReward(BlockRewardIncrement {
                amount: U256::from(100_u64),
            }),
            TransactionPacket::Stake(BalanceDecrement {
                amount: U256::from(200_u64),
                target: test_address,
                irys_ref: test_ref,
            }),
            TransactionPacket::Pledge(BalanceDecrement {
                amount: U256::from(300_u64),
                target: test_address,
                irys_ref: test_ref,
            }),
            TransactionPacket::UnstakeDebit(UnstakeDebit {
                target: test_address,
                irys_ref: test_ref,
            }),
            TransactionPacket::UnstakeRefund(BalanceIncrement {
                amount: U256::from(400_u64),
                target: test_address,
                irys_ref: test_ref,
            }),
            TransactionPacket::Unpledge(UnpledgeDebit {
                target: test_address,
                irys_ref: test_ref,
            }),
            TransactionPacket::StorageFees(BalanceDecrement {
                amount: U256::from(600_u64),
                target: test_address,
                irys_ref: test_ref,
            }),
            TransactionPacket::IngressProofReward(BalanceIncrement {
                amount: U256::from(700_u64),
                target: test_address,
                irys_ref: test_ref,
            }),
            TransactionPacket::TermFeeReward(BalanceIncrement {
                amount: U256::from(800_u64),
                target: test_address,
                irys_ref: test_ref,
            }),
            TransactionPacket::PermFeeRefund(BalanceIncrement {
                amount: U256::from(900_u64),
                target: test_address,
                irys_ref: test_ref,
            }),
            TransactionPacket::UnpledgeRefund(BalanceIncrement {
                amount: U256::from(999_u64),
                target: test_address,
                irys_ref: test_ref,
            }),
        ];

        for packet in packets {
            let tx = ShadowTransaction::new_v1(packet.clone(), solution_hash);
            let mut buf = Vec::new();
            tx.serialize(&mut buf).unwrap();

            let decoded = ShadowTransaction::deserialize_reader(&mut &buf[..]).unwrap();
            assert_eq!(decoded, tx, "Packet {packet:?} failed roundtrip");

            // Verify solution hash is preserved
            let ShadowTransaction::V1 {
                solution_hash: decoded_hash,
                ..
            } = decoded;
            assert_eq!(decoded_hash, solution_hash, "Solution hash mismatch");
        }
    }

    /// Test backward compatibility detection - old format without solution hash should fail
    #[test]
    fn reject_old_format_without_solution_hash() {
        // Create a buffer with V1 marker but old format (no solution hash)
        let mut buf = vec![SHADOW_TX_VERSION_V1];

        // Add a block reward packet in old format
        buf.push(0x02); // BlockReward discriminant
        buf.extend_from_slice(&[0_u8; 32]); // amount

        let result = ShadowTransaction::decode(&mut &buf[..]);
        assert!(result.is_err(), "should fail with missing solution hash");
    }
}
