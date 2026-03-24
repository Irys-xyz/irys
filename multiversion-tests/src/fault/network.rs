use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::{FaultError, FaultGuard, FaultInjector};
use super::FaultTarget;
use tokio::process::Command;

/// Global counter to ensure unique chain names even within the same process.
static INSTANCE_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Returns a chain name unique to this process + instance, e.g. "IRYS_T_12345_3".
/// iptables chain names are limited to 28 chars; this format stays well within that.
fn make_chain_name() -> String {
    let pid = std::process::id();
    let seq = INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("IRYS_T_{pid}_{seq}")
}

/// Simulates network partitions between nodes by installing iptables DROP rules.
///
/// Each `NetworkPartitioner` manages its own iptables chain (e.g. `IRYS_T_12345_0`)
/// to isolate its rules from the system and from other partitioner instances. When
/// [`FaultInjector::inject`] is called with a [`FaultTarget::Network`], bidirectional
/// TCP DROP rules are added for the given port pair, blocking traffic in both
/// directions. The returned [`FaultGuard`] removes those rules when dropped or when
/// [`FaultGuard::undo`] is called explicitly.
///
/// Reference counting (`active_injections`) tracks how many injections are live.
/// The iptables chain is only flushed and deleted once the last injection is undone,
/// so multiple concurrent partitions can safely share one partitioner instance.
pub struct NetworkPartitioner {
    /// Unique iptables chain name scoped to this instance (e.g. `IRYS_T_<pid>_<seq>`).
    chain_name: String,
    /// Number of currently active DROP rule pairs. The chain is cleaned up when this
    /// reaches zero.
    active_injections: Arc<AtomicUsize>,
}

impl NetworkPartitioner {
    /// Creates a new partitioner with a unique iptables chain name and no active injections.
    pub fn new() -> Self {
        Self {
            chain_name: make_chain_name(),
            active_injections: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Returns `true` if passwordless `sudo iptables` is available on this host.
    /// Used as a precondition check — tests that need network partitioning can skip
    /// early if iptables isn't usable (e.g. non-root CI, containers without NET_ADMIN).
    pub async fn is_available() -> bool {
        Command::new("sudo")
            .args(["-n", "iptables", "-L"])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

impl Default for NetworkPartitioner {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for NetworkPartitioner {
    fn drop(&mut self) {
        // Safety cleanup: if no active injections remain, flush the chain.
        // The primary cleanup path is decrement_and_maybe_cleanup().
        if self.active_injections.load(Ordering::Acquire) == 0 {
            sync_cleanup_chain(&self.chain_name);
        }
    }
}

/// Decrements the injection counter and runs iptables chain cleanup when it reaches zero.
fn decrement_and_maybe_cleanup(active: &AtomicUsize, chain: &str) {
    let prev = active.fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        sync_cleanup_chain(chain);
    }
}

impl FaultInjector for NetworkPartitioner {
    /// Installs bidirectional TCP DROP rules for the given port pair.
    ///
    /// Steps:
    /// 1. Ensure the iptables chain exists (create it + jump from INPUT if needed).
    /// 2. Increment the active injection counter.
    /// 3. Add `sport→dport` DROP rule; on failure, decrement and clean up.
    /// 4. Add the reverse `dport→sport` DROP rule; on failure, remove the first
    ///    rule, decrement, and clean up.
    /// 5. Return a [`FaultGuard`] whose undo closure removes both rules and
    ///    triggers chain cleanup when the last injection is undone.
    async fn inject(&self, target: &FaultTarget) -> Result<FaultGuard, FaultError> {
        let (source_port, dest_port) = extract_ports(target)?;

        setup_chain(&self.chain_name).await?;
        self.active_injections.fetch_add(1, Ordering::AcqRel);

        if let Err(e) = add_drop_rule(&self.chain_name, source_port, dest_port).await {
            decrement_and_maybe_cleanup(&self.active_injections, &self.chain_name);
            return Err(e);
        }
        if let Err(e) = add_drop_rule(&self.chain_name, dest_port, source_port).await {
            let _ = sync_remove_drop_rule(&self.chain_name, source_port, dest_port);
            decrement_and_maybe_cleanup(&self.active_injections, &self.chain_name);
            return Err(e);
        }

        let chain = self.chain_name.clone();
        let active = Arc::clone(&self.active_injections);
        Ok(FaultGuard::new(move || {
            let r1 = sync_remove_drop_rule(&chain, source_port, dest_port);
            let r2 = sync_remove_drop_rule(&chain, dest_port, source_port);
            decrement_and_maybe_cleanup(&active, &chain);
            r1?;
            r2?;
            Ok(())
        }))
    }
}

fn extract_ports(target: &FaultTarget) -> Result<(u16, u16), FaultError> {
    match target {
        FaultTarget::Network {
            source_port,
            dest_port,
        } => Ok((*source_port, *dest_port)),
        _ => Err(FaultError::InjectionFailed(
            "NetworkPartitioner requires Network target".into(),
        )),
    }
}

/// Creates the iptables chain and inserts the INPUT jump atomically.
///
/// If `-N` succeeds (chain created), we insert the jump unconditionally.
/// If the chain already exists, we check for the jump and add it if missing.
/// This avoids the TOCTOU of a separate check-then-create sequence.
async fn setup_chain(chain: &str) -> Result<(), FaultError> {
    match run_raw_iptables(&["-N", chain]).await {
        Ok(()) => {
            // Chain freshly created — insert jump unconditionally.
            run_raw_iptables(&["-I", "INPUT", "-j", chain]).await?;
        }
        Err(FaultError::InjectionFailed(ref msg)) if msg.contains("Chain already exists") => {
            // Chain exists (e.g. from a prior inject call on this instance).
            // Only add the jump if it isn't already present.
            if run_raw_iptables(&["-C", "INPUT", "-j", chain]).await.is_err() {
                run_raw_iptables(&["-I", "INPUT", "-j", chain]).await?;
            }
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

fn sync_cleanup_chain(chain: &str) {
    if let Err(e) = sync_run_raw_iptables(&["-F", chain]) {
        tracing::warn!(error = %e, chain, "failed to flush iptables chain");
    }
    // Remove ALL INPUT jump rules pointing at our chain. Concurrent setup_chain()
    // calls can race on the -C check and insert duplicate jumps, so we loop until
    // -D fails (meaning no more matching rules remain).
    loop {
        if sync_run_raw_iptables(&["-D", "INPUT", "-j", chain]).is_err() {
            break;
        }
    }
    if let Err(e) = sync_run_raw_iptables(&["-X", chain]) {
        tracing::warn!(error = %e, chain, "failed to delete iptables chain");
    }
}

fn check_iptables_output(output: &std::process::Output) -> Result<(), FaultError> {
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("permission denied") || lower.contains("operation not permitted") {
        return Err(FaultError::InsufficientPrivileges);
    }
    Err(FaultError::InjectionFailed(stderr.into_owned()))
}

async fn run_raw_iptables(args: &[&str]) -> Result<(), FaultError> {
    let mut cmd_args = vec!["-n", "iptables"];
    cmd_args.extend(args);

    let output = Command::new("sudo").args(&cmd_args).output().await?;

    check_iptables_output(&output)
}

fn sync_run_raw_iptables(args: &[&str]) -> Result<(), FaultError> {
    let mut cmd_args = vec!["-n", "iptables"];
    cmd_args.extend(args);

    let output = std::process::Command::new("sudo")
        .args(&cmd_args)
        .output()?;

    check_iptables_output(&output)
}

async fn add_drop_rule(chain: &str, sport: u16, dport: u16) -> Result<(), FaultError> {
    let sport_str = sport.to_string();
    let dport_str = dport.to_string();

    run_raw_iptables(&[
        "-A", chain, "-p", "tcp", "--sport", &sport_str, "--dport", &dport_str, "-j", "DROP",
    ])
    .await
}

fn sync_remove_drop_rule(chain: &str, sport: u16, dport: u16) -> Result<(), FaultError> {
    let sport_str = sport.to_string();
    let dport_str = dport.to_string();

    sync_run_raw_iptables(&[
        "-D", chain, "-p", "tcp", "--sport", &sport_str, "--dport", &dport_str, "-j", "DROP",
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::unistd::Pid;
    use rstest::rstest;

    #[rstest]
    #[case::network(
        FaultTarget::Network { source_port: 8080, dest_port: 9090 },
        Ok((8080, 9090))
    )]
    #[case::process(
        FaultTarget::Process { pid: Pid::from_raw(1) },
        Err(())
    )]
    fn extract_ports_cases(
        #[case] target: FaultTarget,
        #[case] expected: Result<(u16, u16), ()>,
    ) {
        let result = extract_ports(&target).map_err(|_| ());
        assert_eq!(result, expected);
    }

    #[test]
    fn chain_names_are_unique() {
        let a = NetworkPartitioner::new();
        let b = NetworkPartitioner::new();
        assert_ne!(a.chain_name, b.chain_name);
        // Both contain current PID
        let pid = std::process::id().to_string();
        assert!(a.chain_name.contains(&pid));
        assert!(b.chain_name.contains(&pid));
    }
}
