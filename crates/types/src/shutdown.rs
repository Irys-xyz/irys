/// Process exit code emitted for [`ShutdownReason::PartitionRecoveryRestart`]. Distinct and
/// non-zero so a process supervisor (systemd `Restart=`, a Kubernetes / Docker restart policy)
/// relaunches the node after a deep network-partition recovery; every other shutdown reason exits
/// `0`. The value is `EX_TEMPFAIL` from sysexits(3) — "temporary failure, retry" — so it reads as a
/// retryable restart request rather than a crash.
pub const PARTITION_RECOVERY_RESTART_EXIT_CODE: u8 = 75;

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
    /// A deep network-partition recovery rewound the canonical chain past the migration depth. The
    /// node requests a controlled process restart rather than re-anchoring VDF state in place; on
    /// relaunch the ordinary cold-start path rebuilds correct VDF state and resumes mining. Emitted
    /// with a distinct exit code so a supervisor relaunches the node — see [`Self::exit_code`].
    PartitionRecoveryRestart,
}

impl ShutdownReason {
    pub fn as_label(&self) -> &'static str {
        match self {
            Self::FatalError(_) => "fatal_error",
            Self::ServiceCompleted(_) => "service_completed",
            Self::SigInt | Self::SigTerm | Self::CtrlC | Self::CancellationToken => "signal",
            Self::PartitionRecoveryRestart => "partition_recovery_restart",
            Self::ServiceExited
            | Self::RethTaskManager
            | Self::RethExit
            | Self::VdfExited
            | Self::ShutdownChannelClosed
            | Self::ApiReadinessFailed => "lifecycle",
        }
    }

    /// Process exit code for this shutdown reason. Only [`Self::PartitionRecoveryRestart`] exits
    /// non-zero (see [`PARTITION_RECOVERY_RESTART_EXIT_CODE`]) so a supervisor relaunches the node;
    /// every other reason exits `0`, preserving the historical behaviour where graceful shutdown is
    /// success to the OS.
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::PartitionRecoveryRestart => PARTITION_RECOVERY_RESTART_EXIT_CODE,
            _ => 0,
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
            Self::PartitionRecoveryRestart => write!(f, "partition recovery restart"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_partition_recovery_restart_exits_nonzero() {
        assert_ne!(PARTITION_RECOVERY_RESTART_EXIT_CODE, 0);
        assert_eq!(
            ShutdownReason::PartitionRecoveryRestart.exit_code(),
            PARTITION_RECOVERY_RESTART_EXIT_CODE
        );
        // Every non-restart reason must exit 0 (exhaustive over the other variants).
        for reason in [
            ShutdownReason::FatalError("x".into()),
            ShutdownReason::ServiceCompleted("x".into()),
            ShutdownReason::SigInt,
            ShutdownReason::SigTerm,
            ShutdownReason::CtrlC,
            ShutdownReason::CancellationToken,
            ShutdownReason::ServiceExited,
            ShutdownReason::RethTaskManager,
            ShutdownReason::RethExit,
            ShutdownReason::VdfExited,
            ShutdownReason::ShutdownChannelClosed,
            ShutdownReason::ApiReadinessFailed,
        ] {
            assert_eq!(reason.exit_code(), 0, "{reason} must exit 0");
        }
    }
}
