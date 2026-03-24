use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc};

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("child process has no pid (already exited)")]
    NoPid,
    #[error("pid {raw_pid} exceeds i32 range")]
    PidOutOfRange { raw_pid: u32 },
    #[error("signal delivery failed: {0}")]
    Signal(#[from] nix::Error),
    #[error("process spawn failed: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("failed to open log file: {0}")]
    LogFile(#[source] std::io::Error),
    #[error("shutdown timed out after {timeout:?}")]
    ShutdownTimeout { timeout: Duration },
    #[error("failed to read process info: {0}")]
    ProcInfo(#[source] std::io::Error),
}

pub struct NodeProcessConfig {
    pub name: String,
    pub binary_path: PathBuf,
    pub config_path: PathBuf,
    pub data_dir: PathBuf,
    pub log_file: PathBuf,
    pub api_port: u16,
    pub gossip_port: u16,
    pub reth_port: u16,
    pub version_label: String,
    pub env_vars: Vec<(String, String)>,
}

pub struct NodeProcess {
    pub config: NodeProcessConfig,
    child: Option<Child>,
    log_rx: mpsc::UnboundedReceiver<String>,
    /// Logs preserved from previous process incarnations across respawns.
    buffered_logs: Vec<String>,
}

impl NodeProcess {
    pub fn api_url(&self) -> String {
        crate::node_api_base(self.config.api_port)
    }

    /// Returns the path of the executable actually running in this process
    /// by reading `/proc/<pid>/exe`, rather than relying on cached configuration.
    pub fn runtime_binary_path(&self) -> Result<PathBuf, ProcessError> {
        let pid = self.pid()?;
        std::fs::read_link(format!("/proc/{}/exe", pid.as_raw())).map_err(ProcessError::ProcInfo)
    }

    /// Spawn the node process.
    ///
    /// `port_guards` holds the [`TcpListener`]s that reserve the ports this
    /// node will bind.  They are kept alive until immediately before the
    /// child process is exec'd, closing the TOCTOU window where another
    /// process could steal the ports.
    pub fn spawn(
        config: NodeProcessConfig,
        port_guards: Vec<TcpListener>,
    ) -> Result<Self, ProcessError> {
        let (child, log_rx) = spawn_child_with_logs(&config, port_guards)?;
        Ok(Self {
            config,
            child: Some(child),
            log_rx,
            buffered_logs: Vec::new(),
        })
    }

    pub fn pid(&self) -> Result<Pid, ProcessError> {
        let raw = self
            .child
            .as_ref()
            .and_then(|c| c.id())
            .ok_or(ProcessError::NoPid)?;
        let raw_i32 =
            i32::try_from(raw).map_err(|_| ProcessError::PidOutOfRange { raw_pid: raw })?;
        Ok(Pid::from_raw(raw_i32))
    }

    pub async fn kill(&mut self) -> Result<(), ProcessError> {
        signal::kill(self.pid()?, Signal::SIGKILL)?;
        if let Some(mut child) = self.child.take() {
            let _ = child.wait().await;
        }
        Ok(())
    }

    pub fn freeze(&self) -> Result<(), ProcessError> {
        signal::kill(self.pid()?, Signal::SIGSTOP)?;
        Ok(())
    }

    pub fn unfreeze(&self) -> Result<(), ProcessError> {
        signal::kill(self.pid()?, Signal::SIGCONT)?;
        Ok(())
    }

    pub async fn shutdown(&mut self, timeout: Duration) -> Result<(), ProcessError> {
        let pid = self.pid()?;
        signal::kill(pid, Signal::SIGTERM)?;

        let timed_out = tokio::time::timeout(timeout, async {
            if let Some(child) = self.child.as_mut()
                && let Err(e) = child.wait().await
            {
                tracing::warn!(error = %e, "error waiting for child process");
            }
        })
        .await
        .is_err();

        if timed_out {
            if let Err(e) = signal::kill(pid, Signal::SIGKILL) {
                tracing::warn!(error = %e, "SIGKILL fallback failed during shutdown timeout");
            }
            // Wait for the child to actually exit after SIGKILL so the handle is released
            if let Some(mut child) = self.child.take() {
                let _ = child.wait().await;
            }
            return Err(ProcessError::ShutdownTimeout { timeout });
        }

        self.child = None;
        Ok(())
    }

    pub async fn respawn(&mut self) -> Result<(), ProcessError> {
        if let Some(mut old) = self.child.take() {
            if let Err(e) = old.start_kill() {
                tracing::warn!(error = %e, "failed to kill old process during respawn");
            }
            // Wait for the old process to fully exit so it releases ports and data dirs.
            let _ = old.wait().await;
        }

        // Preserve unread logs from the previous process so drain_logs()
        // can still return them after the receiver is replaced.
        while let Ok(line) = self.log_rx.try_recv() {
            self.buffered_logs.push(line);
        }

        let (child, log_rx) = spawn_child_with_logs(&self.config, Vec::new())?;
        self.child = Some(child);
        self.log_rx = log_rx;
        Ok(())
    }

    pub fn drain_logs(&mut self) -> Vec<String> {
        let mut lines = std::mem::take(&mut self.buffered_logs);
        while let Ok(line) = self.log_rx.try_recv() {
            lines.push(line);
        }
        lines
    }

    pub fn is_running(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        matches!(child.try_wait(), Ok(None))
    }
}

impl Drop for NodeProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take()
            && let Err(e) = child.start_kill()
        {
            tracing::warn!(error = %e, "failed to kill child process on drop");
        }
    }
}

fn spawn_child_with_logs(
    config: &NodeProcessConfig,
    port_guards: Vec<TcpListener>,
) -> Result<(Child, mpsc::UnboundedReceiver<String>), ProcessError> {
    let mut cmd = Command::new(&config.binary_path);
    cmd.env("CONFIG", &config.config_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);

    for (key, value) in &config.env_vars {
        cmd.env(key, value);
    }

    // Open the log file before spawning so that a failure here cannot orphan
    // the child process (kill_on_drop is false).
    let log_writer = open_log_file(&config.log_file).map_err(ProcessError::LogFile)?;

    // Release the port-reservation listeners immediately before exec so the
    // child can bind to the same ports.  Doing this here — rather than
    // earlier in the call chain — minimises the TOCTOU window.
    drop(port_guards);

    let mut child = cmd.spawn().map_err(ProcessError::Spawn)?;
    let (tx, rx) = mpsc::unbounded_channel();
    tracing::info!(
        node = %config.name,
        path = %config.log_file.display(),
        "streaming node logs to file"
    );

    spawn_log_forwarder(
        &config.name,
        "stdout",
        child.stdout.take(),
        &tx,
        &log_writer,
    );
    spawn_log_forwarder(
        &config.name,
        "stderr",
        child.stderr.take(),
        &tx,
        &log_writer,
    );

    Ok((child, rx))
}

type SharedLogWriter = Arc<Mutex<tokio::fs::File>>;

fn open_log_file(path: &Path) -> Result<SharedLogWriter, std::io::Error> {
    // Open synchronously so we fail fast if the path is bad. The file is then
    // wrapped in an async-friendly handle for the forwarder tasks.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let std_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    Ok(Arc::new(Mutex::new(tokio::fs::File::from_std(std_file))))
}

fn spawn_log_forwarder<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    name: &str,
    stream: &str,
    reader: Option<R>,
    tx: &mpsc::UnboundedSender<String>,
    log_writer: &SharedLogWriter,
) {
    let Some(reader) = reader else { return };
    let prefix = format!("[{name}:{stream}]");
    let tx = tx.clone(); // clone: moved into spawned task
    let writer = Arc::clone(log_writer); // clone: moved into spawned task
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let formatted = format!("{prefix} {line}");
            // Write to log file (best-effort)
            {
                let mut f = writer.lock().await;
                let _ = f.write_all(formatted.as_bytes()).await;
                let _ = f.write_all(b"\n").await;
            }
            if tx.send(formatted).is_err() {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_testing_utils::TempDirBuilder;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::LazyLock;
    use tempfile::TempDir;

    struct StubBinary {
        _dir: TempDir,
        path: PathBuf,
    }

    // Workaround for ETXTBSY: create the stub script once so all parallel
    // tests share a single file that is never reopened for writing.
    static SHARED_STUB: LazyLock<StubBinary> = LazyLock::new(|| {
        let dir = TempDirBuilder::new().prefix("stub-bin-").build();
        let script_path = dir.path().join("stub-node");
        std::fs::write(&script_path, "#!/bin/sh\nsleep 3600\n").unwrap();
        std::fs::set_permissions(&script_path, PermissionsExt::from_mode(0o755)).unwrap();
        StubBinary {
            _dir: dir,
            path: script_path,
        }
    });

    fn stub_path() -> &'static Path {
        &SHARED_STUB.path
    }

    fn stub_config(name: &str, binary_path: &Path) -> (NodeProcessConfig, TempDir) {
        let dir = TempDirBuilder::new().prefix("stub-data-").build();
        let data_dir = dir.path().to_path_buf();
        let config = NodeProcessConfig {
            name: name.to_owned(),
            binary_path: binary_path.to_path_buf(),
            config_path: PathBuf::from("unused"),
            log_file: data_dir.join(format!("{name}.log")),
            data_dir,
            api_port: 0,
            gossip_port: 0,
            reth_port: 0,
            version_label: "test-v1".to_owned(),
            env_vars: vec![],
        };
        (config, dir)
    }

    #[test]
    fn spawn_nonexistent_binary_returns_error() {
        let dir = TempDirBuilder::new().prefix("stub-noexist-").build();
        let data_dir = dir.path().to_path_buf();
        let config = NodeProcessConfig {
            name: "test-node".to_owned(),
            binary_path: PathBuf::from("/nonexistent/binary/that/does/not/exist"),
            config_path: PathBuf::from("unused"),
            data_dir: data_dir.clone(),
            log_file: data_dir.join("test-node.log"),
            api_port: 0,
            gossip_port: 0,
            reth_port: 0,
            version_label: "test".to_owned(),
            env_vars: vec![],
        };
        let result = NodeProcess::spawn(config, Vec::new());
        assert!(matches!(result, Err(ProcessError::Spawn(_))));
    }

    #[tokio::test]
    async fn spawn_and_kill() {
        let (config, _dir) = stub_config("kill-test", stub_path());
        let mut proc = NodeProcess::spawn(config, Vec::new()).unwrap();
        assert!(proc.is_running());
        assert!(proc.pid().is_ok());
        proc.kill().await.unwrap();
        assert!(!proc.is_running());
    }

    #[tokio::test]
    async fn pid_after_kill_returns_no_pid() {
        let (config, _dir) = stub_config("no-pid-test", stub_path());
        let mut proc = NodeProcess::spawn(config, Vec::new()).unwrap();
        proc.kill().await.unwrap();
        assert!(matches!(proc.pid(), Err(ProcessError::NoPid)));
    }

    #[tokio::test]
    async fn respawn_replaces_child_process() {
        let (config, _dir) = stub_config("respawn-test", stub_path());
        let mut proc = NodeProcess::spawn(config, Vec::new()).unwrap();
        let first_pid = proc.pid().unwrap();
        proc.respawn().await.unwrap();
        let second_pid = proc.pid().unwrap();
        assert_ne!(first_pid, second_pid);
        proc.kill().await.unwrap();
    }

    #[tokio::test]
    async fn freeze_and_unfreeze() {
        let (config, _dir) = stub_config("freeze-test", stub_path());
        let mut proc = NodeProcess::spawn(config, Vec::new()).unwrap();
        proc.freeze().unwrap();
        proc.unfreeze().unwrap();
        assert!(proc.is_running());
        proc.kill().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_terminates_process() {
        let (config, _dir) = stub_config("shutdown-test", stub_path());
        let mut proc = NodeProcess::spawn(config, Vec::new()).unwrap();
        let result = proc.shutdown(Duration::from_secs(5)).await;
        assert!(result.is_ok());
        assert!(!proc.is_running());
    }
}
