use thiserror::Error;

/// The main error type for the TUI application
#[derive(Debug, Error)]
pub enum TuiError {
    /// Network-related errors from the HTTP client
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    /// Database operation errors
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    /// Terminal I/O errors
    #[error("Terminal error: {0}")]
    Terminal(#[from] std::io::Error),

    /// Configuration loading errors
    #[error("Configuration error: {0}")]
    Config(String),

    /// URL parsing errors
    #[error("Invalid URL: {0}")]
    InvalidUrl(#[from] url::ParseError),

    /// Node operation errors
    #[error("Node error: {0}")]
    Node(NodeError),

    /// Refresh operation errors
    #[error("Refresh error: {0}")]
    Refresh(RefreshError),

    /// Recording operation errors
    #[error("Recording error: {0}")]
    Recording(RecordingError),

    /// Generic errors for compatibility with existing code
    #[error("Operation failed: {0}")]
    Other(String),
}

/// Errors specific to node operations
#[derive(Debug, Error)]
pub enum NodeError {
    #[error("Node not found: {url}")]
    NotFound { url: String },

    #[error("Node is unreachable: {url}")]
    Unreachable { url: String },

    #[error("Cannot remove last node")]
    CannotRemoveLastNode,

    #[error("Node already exists: {url}")]
    AlreadyExists { url: String },

    #[error("Invalid node configuration")]
    InvalidConfig,
}

/// Errors specific to refresh operations
#[derive(Debug, Error)]
pub enum RefreshError {
    #[error("Refresh operation timed out after {seconds} seconds")]
    Timeout { seconds: u64 },

    #[error("Refresh cancelled by user")]
    Cancelled,

    #[error("Failed to fetch data from {node_count} nodes")]
    MultipleFailed { node_count: usize },

    #[error("No nodes available to refresh")]
    NoNodes,
}

/// Errors specific to recording operations
#[derive(Debug, Error)]
pub enum RecordingError {
    #[error("Failed to initialize database: {reason}")]
    InitializationFailed { reason: String },

    #[error("Failed to record {data_type} for node {node}")]
    RecordFailed { data_type: String, node: String },

    #[error("Recording is not enabled")]
    NotEnabled,

    #[error("Database is not initialized")]
    NotInitialized,
}

/// Result type alias using our custom error
pub type TuiResult<T> = Result<T, TuiError>;

/// Conversion from eyre::Error for backward compatibility
impl From<eyre::Error> for TuiError {
    fn from(err: eyre::Error) -> Self {
        Self::Other(err.to_string())
    }
}

/// Extension trait for Results to add context
pub trait ResultExt<T> {
    /// Add context to an error
    fn context(self, msg: impl Into<String>) -> TuiResult<T>;
}

impl<T, E> ResultExt<T> for Result<T, E>
where
    E: Into<TuiError>,
{
    fn context(self, msg: impl Into<String>) -> TuiResult<T> {
        self.map_err(|e| {
            let base_error: TuiError = e.into();
            TuiError::Other(format!("{}: {}", msg.into(), base_error))
        })
    }
}
