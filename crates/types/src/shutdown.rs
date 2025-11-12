#[derive(Debug, Clone)]
pub enum ShutdownReason {
    FatalError(String),
    Signal(String),
    ServiceCompleted(String),
    #[cfg(any(test, feature = "test-utils"))]
    TestComplete,
}

impl std::fmt::Display for ShutdownReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FatalError(e) => write!(f, "fatal error: {}", e),
            Self::Signal(sig) => write!(f, "OS signal ({})", sig),
            Self::ServiceCompleted(service) => write!(f, "service completed: {}", service),
            #[cfg(any(test, feature = "test-utils"))]
            Self::TestComplete => write!(f, "test complete"),
        }
    }
}
