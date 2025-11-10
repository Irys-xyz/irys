#[derive(Debug, Clone)]
pub enum ShutdownReason {
    FatalError(String), // Critical error requiring shutdown
    Reth,
    Vdf,
    Signal(String),   // OS signal (SIGTERM, SIGINT, etc.)
    ServiceCompleted, // Service future completed successfully
    #[cfg(any(test, feature = "test-utils"))]
    TestComplete, // Test finished, cleanup
}

impl std::fmt::Display for ShutdownReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FatalError(e) => write!(f, "fatal error: {}", e),
            Self::Reth => write!(f, "reth shutdown"),
            Self::Vdf => write!(f, "VDF shutdown"),
            Self::Signal(sig) => write!(f, "OS signal ({})", sig),
            Self::ServiceCompleted => write!(f, "service completed successfully"),
            #[cfg(any(test, feature = "test-utils"))]
            Self::TestComplete => write!(f, "test complete"),
        }
    }
}
