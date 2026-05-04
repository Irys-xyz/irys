#[derive(Debug, Clone)]
pub enum ShutdownReason {
    FatalError(String),
    ServiceCompleted(String),
    // OS / external signals
    SigInt,
    SigTerm,
    CtrlC,
    CancellationToken,
    // Internal lifecycle events
    ServiceExited,
    RethTaskManager,
    RethExit,
    VdfExited,
    ShutdownChannelClosed,
    ApiReadinessFailed,
}

impl ShutdownReason {
    pub fn as_label(&self) -> &'static str {
        match self {
            Self::FatalError(_) => "fatal_error",
            Self::ServiceCompleted(_) => "service_completed",
            Self::SigInt | Self::SigTerm | Self::CtrlC | Self::CancellationToken => "signal",
            Self::ServiceExited
            | Self::RethTaskManager
            | Self::RethExit
            | Self::VdfExited
            | Self::ShutdownChannelClosed
            | Self::ApiReadinessFailed => "lifecycle",
        }
    }
}

impl std::fmt::Display for ShutdownReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FatalError(e) => write!(f, "fatal error: {e}"),
            Self::ServiceCompleted(service) => write!(f, "service completed: {service}"),
            Self::SigInt => write!(f, "SIGINT"),
            Self::SigTerm => write!(f, "SIGTERM"),
            Self::CtrlC => write!(f, "ctrl-c"),
            Self::CancellationToken => write!(f, "cancellation token"),
            Self::ServiceExited => write!(f, "service exited"),
            Self::RethTaskManager => write!(f, "reth task manager exited"),
            Self::RethExit => write!(f, "reth node exited"),
            Self::VdfExited => write!(f, "VDF thread exited"),
            Self::ShutdownChannelClosed => write!(f, "shutdown channel closed"),
            Self::ApiReadinessFailed => write!(f, "API readiness check failed"),
        }
    }
}
