use thiserror::Error;

#[derive(Error, Debug)]
pub enum PackingError {
    #[error("Storage sync failed: {0}")]
    StorageSync(#[source] eyre::Error),

    #[error("Remote packing exhausted after {attempts} attempts")]
    RemoteExhausted { attempts: usize },

    #[error("Remote host {url} unavailable: {source}")]
    RemoteUnavailable {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("Chunk stream corrupted at offset {offset}")]
    CorruptedStream { offset: u32 },

    #[error("Invalid partition assignment for SM {sm_id}")]
    InvalidAssignment { sm_id: usize },

    #[error("Invalid chunk range: {requested:?} exceeds max {max}")]
    InvalidRange {
        requested: irys_types::PartitionChunkRange,
        max: u64,
    },
}

pub type PackingResult<T> = Result<T, PackingError>;
