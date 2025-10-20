#[derive(Debug, Copy, Clone)]
pub enum TxKnownStatus {
    Valid,       // we have a local copy, and it's valid (i.e in valid_submit_txs)
    ValidSeen,   // we've seen this tx before, and we know it's valid
    Migrated,    // it's valid and in the database
    InvalidSeen, // we've seen it and it's invalid (i.e invalid_txs)
    Unknown,     // we don't know about this transaction
}
impl TxKnownStatus {
    // true if the transaction is known to us, and is valid - does not indicate whether we have a local copy.
    pub fn is_known_and_valid(&self) -> bool {
        match self {
            Self::InvalidSeen | Self::Unknown => false,
            &Self::Valid | Self::ValidSeen | Self::Migrated => true,
        }
    }

    // true if a transaction is known, valid, and we have a copy locally
    pub fn is_known_valid_and_present(&self) -> bool {
        matches!(self, Self::Valid | Self::Migrated)
    }
    // true if we've seen this transaction in any capacity (even if we know it as invalid)
    pub fn is_known(&self) -> bool {
        !matches!(self, Self::Unknown)
    }
}
