use super::{FaultError, FaultGuard, FaultInjector, FaultTarget};
use nix::sys::signal::{self, Signal};

pub struct ProcessFreezer;

impl FaultInjector for ProcessFreezer {
    async fn inject(&self, target: &FaultTarget) -> Result<FaultGuard, FaultError> {
        let pid = extract_pid(target, "ProcessFreezer")?;
        signal::kill(pid, Signal::SIGSTOP)?;
        Ok(FaultGuard::new(move || {
            match signal::kill(pid, Signal::SIGCONT) {
                Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
                Err(e) => Err(e.into()),
            }
        }))
    }
}

pub struct ProcessKiller;

impl FaultInjector for ProcessKiller {
    async fn inject(&self, target: &FaultTarget) -> Result<FaultGuard, FaultError> {
        let pid = extract_pid(target, "ProcessKiller")?;
        signal::kill(pid, Signal::SIGKILL)?;
        Ok(FaultGuard::noop())
    }
}

fn extract_pid(target: &FaultTarget, injector_name: &str) -> Result<nix::unistd::Pid, FaultError> {
    match target {
        FaultTarget::Process { pid } if pid.as_raw() > 0 => Ok(*pid),
        FaultTarget::Process { pid } => Err(FaultError::InjectionFailed(format!(
            "{injector_name} requires positive PID, got {}",
            pid.as_raw()
        ))),
        FaultTarget::Network { .. } => Err(FaultError::InjectionFailed(format!(
            "{injector_name} requires Process target"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::unistd::Pid;
    use rstest::rstest;

    #[rstest]
    #[case::source_100_dest_200(100, 200)]
    #[case::source_0_dest_0(0, 0)]
    #[case::source_max_dest_max(65535, 65535)]
    fn extract_pid_rejects_all_network_variants(#[case] source: u16, #[case] dest: u16) {
        let target = FaultTarget::Network {
            source_port: source,
            dest_port: dest,
        };
        assert!(extract_pid(&target, "test").is_err());
    }

    #[rstest]
    #[case::pid_1(1)]
    #[case::pid_1000(1000)]
    #[case::pid_max(i32::MAX)]
    fn extract_pid_returns_correct_value(#[case] raw: i32) {
        let target = FaultTarget::Process {
            pid: Pid::from_raw(raw),
        };
        let pid = extract_pid(&target, "test").unwrap();
        assert_eq!(pid, Pid::from_raw(raw));
    }

    #[rstest]
    #[case::pid_zero(0)]
    #[case::pid_negative(-1)]
    #[case::pid_min(i32::MIN)]
    fn extract_pid_rejects_non_positive(#[case] raw: i32) {
        let target = FaultTarget::Process {
            pid: Pid::from_raw(raw),
        };
        assert!(extract_pid(&target, "test").is_err());
    }
}
