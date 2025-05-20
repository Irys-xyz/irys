use alloy_primitives::Address;
use alloy_primitives::U256;
use alloy_rlp::Decodable;
use alloy_rlp::Encodable;
use alloy_rlp::{Error as RlpError, Result as RlpResult};
use alloy_rlp::{RlpDecodable, RlpEncodable};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, arbitrary::Arbitrary)]
pub enum SystemTransaction {
    ReleaseStake(BalanceIncrement),
    BlockReward(BalanceIncrement),
    Stake(BalanceDecrement),
    StorageFees(BalanceDecrement),
}

/// Stable 1-byte discriminants
pub const RELEASE_STAKE_ID: u8 = 0x00;
pub const BLOCK_REWARD_ID: u8 = 0x01;
pub const STAKE_ID: u8 = 0x02;
pub const STORAGE_FEES_ID: u8 = 0x03;

impl Encodable for SystemTransaction {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        match self {
            SystemTransaction::ReleaseStake(bi) => {
                out.put_u8(RELEASE_STAKE_ID);
                bi.encode(out);
            }
            SystemTransaction::BlockReward(bi) => {
                out.put_u8(BLOCK_REWARD_ID);
                bi.encode(out);
            }
            SystemTransaction::Stake(bd) => {
                out.put_u8(STAKE_ID);
                bd.encode(out);
            }
            SystemTransaction::StorageFees(bd) => {
                out.put_u8(STORAGE_FEES_ID);
                bd.encode(out);
            }
        }
    }

    fn length(&self) -> usize {
        1 + match self {
            SystemTransaction::ReleaseStake(bi) => bi.length(),
            SystemTransaction::BlockReward(bi) => bi.length(),
            SystemTransaction::Stake(bd) => bd.length(),
            SystemTransaction::StorageFees(bd) => bd.length(),
        }
    }
}

impl Decodable for SystemTransaction {
    fn decode(buf: &mut &[u8]) -> RlpResult<Self> {
        if buf.is_empty() {
            return Err(RlpError::InputTooShort);
        }

        let disc = buf[0];
        *buf = &buf[1..]; // advance past the discriminant byte

        match disc {
            RELEASE_STAKE_ID => {
                let inner = BalanceIncrement::decode(buf)?;
                Ok(SystemTransaction::ReleaseStake(inner))
            }
            BLOCK_REWARD_ID => {
                let inner = BalanceIncrement::decode(buf)?;
                Ok(SystemTransaction::BlockReward(inner))
            }
            STAKE_ID => {
                let inner = BalanceDecrement::decode(buf)?;
                Ok(SystemTransaction::Stake(inner))
            }
            STORAGE_FEES_ID => {
                let inner = BalanceDecrement::decode(buf)?;
                Ok(SystemTransaction::StorageFees(inner))
            }
            _ => Err(RlpError::Custom("invalid system-transaction discriminant")),
        }
    }
}

impl Default for SystemTransaction {
    fn default() -> Self {
        unimplemented!("relying on the default impl for `SYSTEM_TX` is a critical bug")
    }
}

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
    pub amount: U256,
    pub target: Address,
}

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
    pub amount: U256,
    pub target: Address,
}
