use crate::{Compact, SystemTransactionLedger};
use eyre::eyre;
use serde::{Deserialize, Serialize};
use std::ops::{Index, IndexMut};

/// Names for each of the system ledgers as well as their `ledger_id` discriminant
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Compact, PartialOrd, Ord, Hash,
)]
#[repr(u32)]
#[derive(Default)]
pub enum SystemLedger {
    /// The commitments ledger, for pledging and staking related transactions
    #[default]
    Commitment = 0,
}

impl SystemLedger {
    /// An array of all the System Ledgers, suitable for enumeration
    pub const ALL: [Self; 1] = [Self::Commitment];

    /// Make it possible to iterate over all the System ledgers in order
    pub fn iter() -> impl Iterator<Item = Self> {
        Self::ALL.iter().copied()
    }
    /// get the associated numeric SystemLedger ID
    pub const fn get_id(&self) -> u32 {
        *self as u32
    }
}

impl From<SystemLedger> for u32 {
    fn from(system_ledger: SystemLedger) -> Self {
        system_ledger as Self
    }
}

impl TryFrom<u32> for SystemLedger {
    type Error = eyre::Report;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Commitment),
            _ => Err(eyre!("Invalid ledger number")),
        }
    }
}

impl PartialEq<u32> for SystemLedger {
    fn eq(&self, other: &u32) -> bool {
        self.get_id() == *other
    }
}

impl PartialEq<SystemLedger> for u32 {
    fn eq(&self, other: &SystemLedger) -> bool {
        *self == other.get_id()
    }
}

impl Index<SystemLedger> for Vec<SystemTransactionLedger> {
    type Output = SystemTransactionLedger;

    fn index(&self, ledger: SystemLedger) -> &Self::Output {
        self.iter()
            .find(|tx_ledger| tx_ledger.ledger_id == ledger as u32)
            .expect("No system transaction ledger found for given ledger type")
    }
}

impl IndexMut<SystemLedger> for Vec<SystemTransactionLedger> {
    fn index_mut(&mut self, ledger: SystemLedger) -> &mut Self::Output {
        self.iter_mut()
            .find(|tx_ledger| tx_ledger.ledger_id == ledger as u32)
            .expect("No system transaction ledger found for given ledger type")
    }
}