use irys_types::Compact;
use serde::{Deserialize, Serialize};

/// Names for each of the system ledgers as well as their `ledger_id` discriminant
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Compact, PartialOrd, Ord)]
#[repr(u32)]
pub enum SystemLedger {
    /// The commitments ledger, for pledging and staking related transactions
    Commitment = 0,
}

impl Default for SystemLedger {
    fn default() -> Self {
        Self::Commitment
    }
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
    type Error = &'static str;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Commitment),
            _ => Err("Invalid ledger number"),
        }
    }
}
