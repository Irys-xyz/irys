use crate::binary::ResolvedBinary;
use crate::config::{ConfigParams, NodeRole, PeerEndpoint, patch_node_mode, read_node_role};
use crate::ports::NodePorts;
use crate::probe::HttpProbe;
use crate::process::{NodeProcess, NodeProcessConfig};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

const READY_TIMEOUT: Duration = Duration::from_secs(120);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const NODE_POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Error)]
pub enum ClusterError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("process error: {0}")]
    Process(#[from] crate::process::ProcessError),
    #[error("probe error: {0}")]
    Probe(#[from] crate::probe::ProbeError),
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("port allocation: {0}")]
    Port(#[from] crate::ports::PortError),
    #[error("node '{0}' not found")]
    NodeNotFound(String),
    #[error("duplicate node name: '{0}'")]
    DuplicateNodeName(String),
    #[error("unknown peer '{0}' referenced in node spec")]
    UnknownPeer(String),
    #[error("node '{name}' crashed during startup:\n{logs}")]
    NodeCrashed { name: String, logs: String },
    #[error("node '{name}' did not become ready at {url} after {elapsed:?}:\n{logs}")]
    NodeStartupTimeout {
        name: String,
        url: String,
        elapsed: Duration,
        logs: String,
    },
    #[error("probe initialization: {0}")]
    ProbeInit(reqwest::Error),
    #[error("peers present but no genesis node in cluster spec")]
    MissingGenesis,
    #[error("expected exactly 1 genesis node but found {0}")]
    MultipleGenesis(usize),
}

pub struct NodeSpec {
    pub name: String,
    pub binary: ResolvedBinary,
    pub role: NodeRole,
    pub peers: Vec<String>,
    pub mining_key: String,
    pub reward_address: String,
}

pub struct ClusterSpec {
    pub nodes: Vec<NodeSpec>,
    pub height_tolerance: u64,
    pub base_config: String,
    /// Root directory for all test artifacts (data, configs, logs).
    /// Not cleaned up automatically — caller is responsible for cleanup.
    pub run_dir: PathBuf,
    /// Git ref / label for the "old" binary (if applicable).
    pub old_ref: Option<String>,
    /// Git ref / label for the "new" binary (if applicable).
    pub new_ref: Option<String>,
}

pub struct Cluster {
    pub nodes: HashMap<String, NodeProcess>,
    pub probe: HttpProbe,
    run_dir: PathBuf,
    port_map: HashMap<String, NodePorts>,
    height_tolerance: u64,
    old_ref: Option<String>,
    new_ref: Option<String>,
}

impl Cluster {
    pub async fn start(spec: ClusterSpec) -> Result<Self, ClusterError> {
        let run_dir = spec.run_dir.clone();
        std::fs::create_dir_all(&run_dir)?;
        eprintln!("test artifacts: {}", run_dir.display());

        // Write initial status marker — stays RUNNING if the test panics,
        // overwritten with PASSED in shutdown(). The xtask aggregates these
        // into a summary status.txt after the run.
        let _ = std::fs::write(
            run_dir.join(".status"),
            format_status("RUNNING", spec.old_ref.as_deref(), spec.new_ref.as_deref()),
        );

        let probe = HttpProbe::new().map_err(ClusterError::ProbeInit)?;

        let mut port_map = allocate_ports(&spec.nodes)?;
        let nodes = spawn_and_wait_for_nodes(&spec, &mut port_map, &run_dir, &probe).await?;

        Ok(Self {
            nodes,
            probe,
            run_dir,
            port_map,
            height_tolerance: spec.height_tolerance,
            old_ref: spec.old_ref,
            new_ref: spec.new_ref,
        })
    }

    pub fn node_mut(&mut self, name: &str) -> Result<&mut NodeProcess, ClusterError> {
        self.nodes
            .get_mut(name)
            .ok_or_else(|| ClusterError::NodeNotFound(name.into()))
    }

    pub fn api_urls(&mut self) -> Vec<String> {
        self.nodes
            .values_mut()
            .filter_map(|n| {
                if n.is_running() {
                    Some(n.api_url())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn port_map(&self) -> &HashMap<String, NodePorts> {
        &self.port_map
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// Returns the maximum tip height across all running nodes.
    pub async fn get_max_height(&mut self) -> Result<u64, ClusterError> {
        let urls = self.api_urls();
        let mut max_height = 0u64;
        for url in &urls {
            let info = self.probe.get_info(url).await?;
            max_height = max_height.max(info.height);
        }
        Ok(max_height)
    }

    /// Polls until all running nodes report a height strictly above `baseline`.
    pub async fn wait_for_height_above(
        &mut self,
        baseline: u64,
        timeout: Duration,
    ) -> Result<(), ClusterError> {
        let urls = self.api_urls();
        let target = baseline + 1;
        for url in &urls {
            self.probe.wait_for_height(url, target, timeout).await?;
        }
        Ok(())
    }

    pub async fn wait_for_convergence(&mut self, timeout: Duration) -> Result<(), ClusterError> {
        for (name, node) in &mut self.nodes {
            if !node.is_running() {
                let raw_logs = node.drain_logs().join("\n");
                let logs = strip_ansi_codes(&raw_logs);
                eprintln!("\n--- node '{name}' crashed ---\n{logs}\n---");
                return Err(ClusterError::NodeCrashed {
                    name: name.clone(), // clone: used in error construction
                    logs,
                });
            }
        }
        let urls = self.api_urls();
        self.probe
            .wait_for_convergence(&urls, self.height_tolerance, timeout)
            .await?;
        Ok(())
    }

    pub async fn upgrade_node(
        &mut self,
        name: &str,
        new_binary: &ResolvedBinary,
    ) -> Result<(), ClusterError> {
        let node = self
            .nodes
            .get_mut(name)
            .ok_or_else(|| ClusterError::NodeNotFound(name.into()))?;

        if node.is_running() {
            node.shutdown(SHUTDOWN_TIMEOUT).await?;
        }

        let current_role = read_node_role(node.config_path())?;

        node.set_binary(new_binary.path.clone()); // clone: stored as new binary path
        node.set_version_label(new_binary.label.clone()); // clone: stored as new label

        // Preserve the node's original role. Only strip the GENESIS env var
        // and switch to Peer mode when the node was already a peer.
        match current_role {
            NodeRole::Genesis => {
                patch_node_mode(node.config_path(), NodeRole::Genesis)?;
            }
            NodeRole::Peer => {
                node.remove_env_var("GENESIS");
                patch_node_mode(node.config_path(), NodeRole::Peer)?;
            }
        }
        node.respawn().await?;

        let url = node.api_url();
        wait_for_node_ready(&self.probe, node, &url, READY_TIMEOUT).await?;
        tracing::info!(node = %name, version = %new_binary.label, "node upgraded and ready");
        Ok(())
    }

    pub async fn shutdown(&mut self) {
        for (name, node) in &mut self.nodes {
            if node.is_running()
                && let Err(e) = node.shutdown(SHUTDOWN_TIMEOUT).await
            {
                tracing::warn!(node = %name, error = %e, "graceful shutdown failed, sending SIGKILL");
                if let Err(kill_err) = node.kill().await {
                    tracing::error!(node = %name, error = %kill_err, "SIGKILL also failed");
                }
            }
        }

        // Mark test as passed — we only reach here if no assertion panicked.
        let _ = std::fs::write(
            self.run_dir.join(".status"),
            format_status("PASSED", self.old_ref.as_deref(), self.new_ref.as_deref()),
        );
    }
}

fn allocate_ports(nodes: &[NodeSpec]) -> Result<HashMap<String, NodePorts>, ClusterError> {
    let mut port_map = HashMap::with_capacity(nodes.len());
    for node_spec in nodes {
        if port_map.contains_key(&node_spec.name) {
            return Err(ClusterError::DuplicateNodeName(node_spec.name.clone())); // clone: error message
        }
        port_map.insert(
            node_spec.name.clone(), // clone: HashMap key
            NodePorts::allocate()?,
        );
    }
    Ok(port_map)
}

async fn cleanup_nodes(nodes: &mut HashMap<String, NodeProcess>) {
    for (name, node) in nodes.iter_mut() {
        if node.is_running() {
            if let Err(e) = node.shutdown(SHUTDOWN_TIMEOUT).await {
                tracing::warn!(node = %name, error = %e, "cleanup: graceful shutdown failed, sending SIGKILL");
                if let Err(kill_err) = node.kill().await {
                    tracing::error!(node = %name, error = %kill_err, "cleanup: SIGKILL also failed");
                }
            }
        }
    }
}

async fn spawn_and_wait_for_nodes(
    spec: &ClusterSpec,
    port_map: &mut HashMap<String, NodePorts>,
    run_dir: &Path,
    probe: &HttpProbe,
) -> Result<HashMap<String, NodeProcess>, ClusterError> {
    let mut nodes = HashMap::new();
    match spawn_and_wait_for_nodes_inner(spec, port_map, run_dir, probe, &mut nodes).await {
        Ok(()) => Ok(nodes),
        Err(e) => {
            cleanup_nodes(&mut nodes).await;
            Err(e)
        }
    }
}

async fn spawn_and_wait_for_nodes_inner(
    spec: &ClusterSpec,
    port_map: &mut HashMap<String, NodePorts>,
    run_dir: &Path,
    probe: &HttpProbe,
    nodes: &mut HashMap<String, NodeProcess>,
) -> Result<(), ClusterError> {
    let log_dir = run_dir.join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let genesis_count = spec
        .nodes
        .iter()
        .filter(|n| matches!(n.role, NodeRole::Genesis))
        .count();
    if genesis_count > 1 {
        return Err(ClusterError::MultipleGenesis(genesis_count));
    }

    let mut genesis_api_url: Option<String> = None;

    for node_spec in spec
        .nodes
        .iter()
        .filter(|n| matches!(n.role, NodeRole::Genesis))
    {
        let mut proc =
            build_node_process(node_spec, port_map, run_dir, &spec.base_config, &log_dir)?;
        let url = node_api_url(port_map, &node_spec.name)?;
        wait_for_node_ready(probe, &mut proc, &url, READY_TIMEOUT).await?;
        genesis_api_url = Some(url);
        nodes.insert(node_spec.name.clone(), proc); // clone: HashMap key
        tracing::info!(node = %node_spec.name, "genesis node ready");
    }

    let peer_specs: Vec<&NodeSpec> = spec
        .nodes
        .iter()
        .filter(|n| matches!(n.role, NodeRole::Peer))
        .collect();

    if !peer_specs.is_empty() {
        let genesis_url = genesis_api_url
            .as_deref()
            .ok_or(ClusterError::MissingGenesis)?;

        for node_spec in &peer_specs {
            generate_node_config(node_spec, port_map, run_dir, &spec.base_config)?;
        }

        patch_peer_configs(probe, genesis_url, &peer_specs, run_dir).await?;
    }

    for node_spec in &peer_specs {
        let config_path = run_dir.join(format!("config-{}.toml", node_spec.name));
        let data_dir = run_dir.join(format!("data-{}", node_spec.name));
        let proc = spawn_node(node_spec, port_map, config_path, data_dir, &log_dir)?;
        nodes.insert(node_spec.name.clone(), proc); // clone: HashMap key
    }
    for node_spec in &peer_specs {
        let url = node_api_url(port_map, &node_spec.name)?;
        let proc = nodes
            .get_mut(&node_spec.name)
            .ok_or_else(|| ClusterError::NodeNotFound(node_spec.name.clone()))?;
        wait_for_node_ready(probe, proc, &url, READY_TIMEOUT).await?;
        tracing::info!(node = %node_spec.name, "peer node ready");
    }

    Ok(())
}

/// Fetches the consensus config and genesis hash from the running genesis node,
/// then patches each peer's config file to use `[consensus.Custom]` with the
/// real genesis hash (peers require `expected_genesis_hash` to pass validation).
async fn patch_peer_configs(
    probe: &HttpProbe,
    genesis_url: &str,
    peer_specs: &[&NodeSpec],
    temp_base: &Path,
) -> Result<(), ClusterError> {
    let genesis_hash = probe.get_genesis_hash(genesis_url).await?;
    let consensus_json = probe.get_network_config(genesis_url).await?;
    tracing::info!(genesis_hash = %genesis_hash, "fetched genesis hash for peer configs");

    for node_spec in peer_specs {
        let config_path = temp_base.join(format!("config-{}.toml", node_spec.name));
        crate::config::patch_peer_consensus(&config_path, &consensus_json, &genesis_hash)?;
        tracing::info!(node = %node_spec.name, "patched peer consensus config");
    }
    Ok(())
}

fn node_api_url(port_map: &HashMap<String, NodePorts>, name: &str) -> Result<String, ClusterError> {
    let ports = port_map
        .get(name)
        .ok_or_else(|| ClusterError::NodeNotFound(name.into()))?;
    Ok(crate::node_api_base(ports.api))
}

fn build_peer_endpoints(
    peers: &[String],
    port_map: &HashMap<String, NodePorts>,
) -> Result<Vec<PeerEndpoint>, ClusterError> {
    peers
        .iter()
        .map(|peer_name| {
            let p = port_map
                .get(peer_name)
                .ok_or_else(|| ClusterError::UnknownPeer(peer_name.clone()))?; // clone: error message
            Ok(PeerEndpoint {
                gossip_addr: format!("127.0.0.1:{}", p.gossip),
                api_addr: format!("127.0.0.1:{}", p.api),
                reth_addr: format!("127.0.0.1:{}", p.reth_network),
            })
        })
        .collect()
}

fn generate_node_config(
    node_spec: &NodeSpec,
    port_map: &HashMap<String, NodePorts>,
    temp_base: &Path,
    base_config: &str,
) -> Result<(PathBuf, PathBuf), ClusterError> {
    let ports = port_map
        .get(&node_spec.name)
        .ok_or_else(|| ClusterError::NodeNotFound(node_spec.name.clone()))?;
    let data_dir = temp_base.join(format!("data-{}", node_spec.name));
    std::fs::create_dir_all(&data_dir)?;

    let peer_endpoints = build_peer_endpoints(&node_spec.peers, port_map)?;
    let config_path = temp_base.join(format!("config-{}.toml", node_spec.name));

    crate::config::generate_config(&ConfigParams {
        base_template: base_config,
        role: node_spec.role,
        api_port: ports.api,
        gossip_port: ports.gossip,
        reth_port: ports.reth_network,
        data_dir: &data_dir,
        mining_key: &node_spec.mining_key,
        reward_address: &node_spec.reward_address,
        peer_endpoints: &peer_endpoints,
        output_path: &config_path,
    })?;

    Ok((config_path, data_dir))
}

fn spawn_node(
    node_spec: &NodeSpec,
    port_map: &mut HashMap<String, NodePorts>,
    config_path: PathBuf,
    data_dir: PathBuf,
    log_dir: &Path,
) -> Result<NodeProcess, ClusterError> {
    // Release the bound listeners so the child process can bind to these ports.
    port_map
        .get_mut(&node_spec.name)
        .ok_or_else(|| ClusterError::NodeNotFound(node_spec.name.clone()))?
        .release_guards();
    let ports = port_map
        .get(&node_spec.name)
        .ok_or_else(|| ClusterError::NodeNotFound(node_spec.name.clone()))?;
    let mut env_vars = vec![("RUST_LOG".to_owned(), "debug".to_owned())];
    if matches!(node_spec.role, NodeRole::Genesis) {
        env_vars.push(("GENESIS".to_owned(), "true".to_owned()));
    }

    let log_file = log_dir.join(format!("{}.log", node_spec.name));
    let proc = NodeProcess::spawn(NodeProcessConfig {
        name: node_spec.name.clone(), // clone: stored in NodeProcess
        binary_path: node_spec.binary.path.clone(), // clone: stored in NodeProcess
        config_path,
        data_dir,
        log_file,
        api_port: ports.api,
        gossip_port: ports.gossip,
        reth_port: ports.reth_network,
        version_label: node_spec.binary.label.clone(), // clone: stored in NodeProcess
        env_vars,
    })?;

    Ok(proc)
}

fn build_node_process(
    node_spec: &NodeSpec,
    port_map: &mut HashMap<String, NodePorts>,
    temp_base: &Path,
    base_config: &str,
    log_dir: &Path,
) -> Result<NodeProcess, ClusterError> {
    let (config_path, data_dir) =
        generate_node_config(node_spec, port_map, temp_base, base_config)?;
    spawn_node(node_spec, port_map, config_path, data_dir, log_dir)
}

async fn wait_for_node_ready(
    probe: &HttpProbe,
    proc: &mut NodeProcess,
    api_url: &str,
    timeout: Duration,
) -> Result<(), ClusterError> {
    let start = tokio::time::Instant::now();
    let name = proc.name().to_owned();

    loop {
        if !proc.is_running() {
            let logs = format_log_tail(proc);
            let log_path = proc.log_file().display();
            eprintln!(
                "\n--- node '{name}' crashed during startup (full logs: {log_path}) ---\n{logs}\n---"
            );
            return Err(ClusterError::NodeCrashed { name, logs });
        }

        match probe.get_info(api_url).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                tracing::trace!(node = %name, error = %e, "probe poll failed");
            }
        }

        let elapsed = start.elapsed();
        if elapsed >= timeout {
            let logs = format_log_tail(proc);
            let log_path = proc.log_file().display();
            eprintln!(
                "\n--- node '{name}' startup timeout after {elapsed:?} at {api_url} (full logs: {log_path}) ---\n{logs}\n---"
            );
            return Err(ClusterError::NodeStartupTimeout {
                name,
                url: api_url.to_owned(),
                elapsed,
                logs,
            });
        }

        tokio::time::sleep(NODE_POLL_INTERVAL).await;
    }
}

fn format_log_tail(proc: &mut NodeProcess) -> String {
    let lines = proc.drain_logs();
    let tail: Vec<&String> = lines.iter().rev().take(50).collect();
    let mut out: Vec<&str> = tail.iter().rev().map(|s| s.as_str()).collect();
    if lines.len() > 50 {
        out.insert(0, "... (truncated)");
    }
    let joined = out.join("\n");
    strip_ansi_codes(&joined)
}

fn format_status(status: &str, old_ref: Option<&str>, new_ref: Option<&str>) -> String {
    let mut out = status.to_owned();
    if let Some(r) = old_ref {
        out.push_str(&format!("\nold_ref: {r}"));
    }
    if let Some(r) = new_ref {
        out.push_str(&format!("\nnew_ref: {r}"));
    }
    out
}

fn strip_ansi_codes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip until we hit a letter (the terminator of an ANSI escape sequence)
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
