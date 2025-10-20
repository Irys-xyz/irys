#[derive(Debug, Copy, Clone)]
pub enum TxKnownStatus {
    Valid,       // we have a local copy, and it's valid (i.e in valid_submit_txs)
    ValidSeen,   // we've seen this tx before, and we know it's valid
    Migrated,    // it's valid and in the database
    InvalidSeen, // we've seen it and it's invalid (i.e invalid_txs)
    Unknown,     // we don't know about this transaction
}
impl TxKnownStatus {
    pub fn is_known_and_valid(&self) -> bool {
        match self {
            Self::InvalidSeen | Self::Unknown => false,
            &Self::Valid | Self::ValidSeen | Self::Migrated => true,
        }
    }

    pub fn is_known(&self) -> bool {
        !matches!(self, Self::Unknown)
    }
}
