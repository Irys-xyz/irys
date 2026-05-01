use crate::binary::{BinaryKind, ResolvedBinary};
use crate::config::{ConfigParams, NodeRole, PeerEndpoint};
use crate::ports::NodePorts;
use crate::probe::HttpProbe;
use crate::process::{NodeProcess, NodeProcessConfig};
use crate::run_config::RunConfig;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

const READY_TIMEOUT: Duration = Duration::from_secs(120);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const NODE_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Cap on how many blocks we sweep when running the per-block consistency
/// check. The test chains never run for very long (a few minutes max),
/// so 500 covers the entire chain comfortably while bounding work.
const BLOCK_INDEX_LIMIT: u64 = 500;

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
    #[error("data tx error: {0}")]
    DataTx(String),
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
    /// Base TOML template for nodes running the OLD binary. The old ref's
    /// `NodeConfig` schema may differ from HEAD's, so the cluster keeps a
    /// separate template per kind and routes by [`BinaryKind`] at config-gen
    /// time. Set to the same value as `base_config_new` when both versions
    /// share a schema.
    pub base_config_old: String,
    /// Base TOML template for nodes running the NEW binary.
    pub base_config_new: String,
    /// Per-run knobs for cross-version comparison and tx construction —
    /// e.g. field rename pairs, skip lists, fields to keep at default
    /// during signing. See [`RunConfig`] for details.
    pub run_config: RunConfig,
    /// Root directory for all test artifacts (data, configs, logs).
    /// Not cleaned up automatically — caller is responsible for cleanup.
    pub run_dir: PathBuf,
    /// Git ref / label for the "old" binary (if applicable).
    pub old_ref: Option<String>,
    /// Git ref / label for the "new" binary (if applicable).
    pub new_ref: Option<String>,
}

/// Captured consensus state needed to (re)write a peer's `[consensus.Custom]`
/// block during an upgrade. `template_overlay` is `Some` when the new
/// base-config template carries a hand-authored `[consensus.Custom]` block —
/// see [`crate::config::patch_peer_consensus`] for the merge semantics.
struct PeerConsensusPatch {
    genesis_hash: String,
    consensus_json: serde_json::Value,
    template_overlay: Option<toml::map::Map<String, toml::Value>>,
}

/// Snapshot of the running chain's consensus parameters that test code
/// needs to build signed transactions and pace cross-version transitions.
/// Read from the genesis's `/v1/network/config` rather than hard-coded so
/// the tests track whatever schema the actual running cluster uses.
#[derive(Debug, Clone, Copy)]
pub struct ChainParams {
    pub chain_id: u64,
    pub chunk_size: u64,
    /// Number of blocks the chain must advance past a tx-containing block
    /// before that tx is persisted from the in-memory block tree to the
    /// `IrysDataTxHeaders` table on disk.
    pub block_migration_depth: u64,
}

fn read_consensus_u64(consensus: &serde_json::Value, key: &str) -> Result<u64, ClusterError> {
    consensus
        .get(key)
        .and_then(|v| match v {
            serde_json::Value::String(s) => s.parse::<u64>().ok(),
            serde_json::Value::Number(n) => n.as_u64(),
            _ => None,
        })
        .ok_or_else(|| {
            ClusterError::DataTx(format!("missing or non-u64 `{key}` in /v1/network/config"))
        })
}

/// Per-node identity preserved across upgrades so we can regenerate configs
/// from the appropriate template when a node's binary kind changes.
struct NodeIdentity {
    role: NodeRole,
    mining_key: String,
    reward_address: String,
    peers: Vec<String>,
    kind: BinaryKind,
}

pub struct Cluster {
    pub nodes: HashMap<String, NodeProcess>,
    pub probe: HttpProbe,
    run_dir: PathBuf,
    port_map: HashMap<String, NodePorts>,
    height_tolerance: u64,
    old_ref: Option<String>,
    new_ref: Option<String>,
    identities: HashMap<String, NodeIdentity>,
    base_config_old: String,
    base_config_new: String,
    run_config: RunConfig,
}

impl Cluster {
    pub async fn start(spec: ClusterSpec) -> Result<Self, ClusterError> {
        let run_dir = spec.run_dir.clone();
        std::fs::create_dir_all(&run_dir)?;
        eprintln!("test artifacts: {}", run_dir.display());

        // Write initial status marker — stays RUNNING if the test panics,
        // overwritten with PASSED in shutdown(). The xtask aggregates these
        // into a summary status.txt after the run.
        std::fs::write(
            run_dir.join(".status"),
            format_status("RUNNING", spec.old_ref.as_deref(), spec.new_ref.as_deref()),
        )?;

        let probe = HttpProbe::new().map_err(ClusterError::ProbeInit)?;

        let identities: HashMap<String, NodeIdentity> = spec
            .nodes
            .iter()
            .map(|n| {
                (
                    n.name.clone(),
                    NodeIdentity {
                        role: n.role,
                        mining_key: n.mining_key.clone(),
                        reward_address: n.reward_address.clone(),
                        peers: n.peers.clone(),
                        kind: n.binary.kind,
                    },
                )
            })
            .collect();

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
            identities,
            base_config_old: spec.base_config_old,
            base_config_new: spec.base_config_new,
            run_config: spec.run_config,
        })
    }

    fn template_for(&self, kind: BinaryKind) -> &str {
        match kind {
            BinaryKind::Old => &self.base_config_old,
            BinaryKind::New => &self.base_config_new,
        }
    }

    pub fn node_mut(&mut self, name: &str) -> Result<&mut NodeProcess, ClusterError> {
        self.nodes
            .get_mut(name)
            .ok_or_else(|| ClusterError::NodeNotFound(name.into()))
    }

    /// Returns API URLs for **every** node in the cluster, regardless of
    /// whether the node is still running. Use this when you need all configured
    /// endpoints (e.g. to verify that a stopped node is truly unreachable).
    ///
    /// If you need to guarantee that every node is healthy, call
    /// [`checked_api_urls`](Self::checked_api_urls) instead — it returns an
    /// error when any node has crashed.
    pub fn api_urls(&self) -> Vec<String> {
        self.nodes
            .values()
            .map(super::process::NodeProcess::api_url)
            .collect()
    }

    /// Returns API URLs for all nodes, returning an error if any node has
    /// crashed. Use this when your test requires every node to be healthy;
    /// a single crashed node will short-circuit with [`ClusterError::NodeCrashed`].
    ///
    /// For a best-effort list that includes stopped nodes, use
    /// [`api_urls`](Self::api_urls).
    pub fn checked_api_urls(&mut self) -> Result<Vec<String>, ClusterError> {
        let mut urls = Vec::with_capacity(self.nodes.len());
        for (name, node) in &mut self.nodes {
            if !node.is_running() {
                let raw_logs = node.drain_logs().join("\n");
                let logs = strip_ansi_codes(&raw_logs);
                return Err(ClusterError::NodeCrashed {
                    name: name.clone(),
                    logs,
                });
            }
            urls.push(node.api_url());
        }
        Ok(urls)
    }

    pub fn port_map(&self) -> &HashMap<String, NodePorts> {
        &self.port_map
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// Returns the maximum tip height across all nodes.
    /// Returns an error if any node has crashed.
    pub async fn get_max_height(&mut self) -> Result<u64, ClusterError> {
        let urls = self.checked_api_urls()?;
        let mut max_height = 0_u64;
        for url in &urls {
            let info = self.probe.get_info(url).await?;
            max_height = max_height.max(info.height);
        }
        Ok(max_height)
    }

    /// Polls until all nodes report a height strictly above `baseline`.
    /// Returns an error if any node has crashed.
    ///
    /// The timeout is applied once across the whole cluster — not per node.
    pub async fn wait_for_height_above(
        &mut self,
        baseline: u64,
        timeout: Duration,
    ) -> Result<(), ClusterError> {
        self.wait_for_height_at_least(baseline + 1, timeout).await
    }

    /// Polls until all nodes report a height >= `target`. Used by upgrade
    /// tests to wait for the chain to advance past `block_migration_depth`,
    /// which is how a tx submitted into the block tree finally lands in the
    /// persistent `IrysDataTxHeaders` table — without that wait, a binary
    /// swap exercises only in-memory state and migrations on disk are no-ops.
    pub async fn wait_for_height_at_least(
        &mut self,
        target: u64,
        timeout: Duration,
    ) -> Result<(), ClusterError> {
        let urls = self.checked_api_urls()?;
        let start = tokio::time::Instant::now();
        for url in &urls {
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return Err(crate::probe::ProbeError::Timeout {
                    condition: format!("cluster height >= {target}"),
                    elapsed: timeout,
                }
                .into());
            }
            self.probe
                .wait_for_height(url, target, timeout.checked_sub(elapsed).unwrap())
                .await?;
        }
        Ok(())
    }

    pub async fn wait_for_convergence(&mut self, timeout: Duration) -> Result<(), ClusterError> {
        let urls = self.checked_api_urls()?;
        self.probe
            .wait_for_convergence(&urls, self.height_tolerance, timeout)
            .await?;
        Ok(())
    }

    /// Submits a data transaction to one node and asserts every running node
    /// in the cluster eventually serves it on `/v1/tx/{id}`. The chain has to
    /// produce at least one block after the submit for the tx to be visible
    /// via the API, so we re-use the cluster's existing `wait_for_*` budget
    /// for that.
    ///
    /// Used by every test path so we can prove the cluster is *functional*
    /// (data flows through gossip, the mempool, block production, and the
    /// HTTP API), not merely that the binaries booted.
    pub async fn submit_and_verify_data(
        &mut self,
        target_node: &str,
        data: Vec<u8>,
        chain_id: u64,
        chunk_size: u64,
        timeout: Duration,
    ) -> Result<irys_types::DataTransaction, ClusterError> {
        let signer = crate::data_tx::dev_signer(chain_id, chunk_size)
            .map_err(|e| ClusterError::DataTx(format!("dev_signer: {e}")))?;

        let target_url = self
            .nodes
            .get_mut(target_node)
            .ok_or_else(|| ClusterError::NodeNotFound(target_node.into()))?
            .api_url();

        let client = reqwest::Client::new();
        let tx = crate::data_tx::submit_data_tx(
            &client,
            &target_url,
            &signer,
            data,
            &self.run_config.tx_build,
        )
        .await
        .map_err(|e| ClusterError::DataTx(e.to_string()))?;
        let tx_id = tx.header.id;
        tracing::info!(
            tx_id = %tx_id,
            target = %target_node,
            "submitted data tx; waiting for cluster-wide visibility"
        );

        let urls = self.checked_api_urls()?;
        for url in &urls {
            crate::data_tx::wait_for_tx_visible(&client, url, tx_id, timeout)
                .await
                .map_err(|e| ClusterError::DataTx(e.to_string()))?;
        }
        Ok(tx)
    }

    /// End-to-end submit-and-promote helper: submits the tx, uploads its
    /// chunks (so storage nodes can produce ingress proofs), and waits
    /// for the genesis to report a non-`None` `promotion_height`. Returns
    /// the promoted [`DataTransaction`] — its `header.id` is the tx_id
    /// the rest of the test should track.
    ///
    /// Promotion is the path that exercises the V1→V2 migration's
    /// `promoted_height` move: when a tx is promoted, its
    /// `promoted_height` is set in `IrysDataTxMetadata` (NEW) or in the
    /// inline header (OLD). After a rollback, an OLD binary that doesn't
    /// know about the new metadata table will silently report the tx as
    /// not-promoted — see [`assert_tx_promoted_on_all_nodes`].
    pub async fn submit_promote_and_verify(
        &mut self,
        target_node: &str,
        data: Vec<u8>,
        chain_id: u64,
        chunk_size: u64,
        timeout: Duration,
    ) -> Result<irys_types::DataTransaction, ClusterError> {
        let signer = crate::data_tx::dev_signer(chain_id, chunk_size)
            .map_err(|e| ClusterError::DataTx(format!("dev_signer: {e}")))?;

        let target_url = self
            .nodes
            .get_mut(target_node)
            .ok_or_else(|| ClusterError::NodeNotFound(target_node.into()))?
            .api_url();

        let client = reqwest::Client::new();
        let tx = crate::data_tx::submit_data_tx(
            &client,
            &target_url,
            &signer,
            data,
            &self.run_config.tx_build,
        )
        .await
        .map_err(|e| ClusterError::DataTx(e.to_string()))?;
        let tx_id = tx.header.id;
        tracing::info!(
            tx_id = %tx_id,
            target = %target_node,
            "submitted data tx; uploading chunks for promotion"
        );

        crate::data_tx::upload_chunks_for_tx(&client, &target_url, &tx)
            .await
            .map_err(|e| ClusterError::DataTx(format!("uploading chunks: {e}")))?;

        let urls = self.checked_api_urls()?;
        for url in &urls {
            crate::data_tx::wait_for_tx_visible(&client, url, tx_id, timeout)
                .await
                .map_err(|e| ClusterError::DataTx(e.to_string()))?;
        }

        let genesis_url = self.find_running_genesis_url()?;
        let height = crate::data_tx::wait_for_promotion(&client, &genesis_url, tx_id, timeout)
            .await
            .map_err(|e| ClusterError::DataTx(format!("wait_for_promotion: {e}")))?;
        tracing::info!(tx_id = %tx_id, promotion_height = height, "tx promoted");

        // Wait for the chain to advance past `block_migration_depth` after
        // the promotion block. Until then, peer nodes have only seen the
        // promotion in their in-memory block tree — `IrysDataTxMetadata`
        // (the table backing `/v1/tx/{id}/promotion-status`) is only
        // written when the block_migration_service migrates a block past
        // that depth. Without this wait, assertions that poll every node
        // for promotion can race the migration on slower peers (the case
        // we hit immediately after upgrading the only mining node).
        let consensus = self.probe.get_network_config(&genesis_url).await?;
        let migration_depth = read_consensus_u64(&consensus, "blockMigrationDepth").unwrap_or(6);
        self.wait_for_height_at_least(height + migration_depth + 2, timeout)
            .await?;

        Ok(tx)
    }

    /// Asserts every running node reports a non-`None` `promotion_height`
    /// for `tx_id`. Polls each node up to `timeout` to allow promotion
    /// information to gossip across the cluster after the genesis sets it.
    ///
    /// Critical post-rollback assertion: V1→V2 migration moves
    /// `promoted_height` out of the inline tx header into the
    /// `IrysDataTxMetadata` table. An OLD binary rolled back on top of
    /// that schema doesn't know to look in the new table, so it would
    /// silently report `promotion_height: None` for every previously-
    /// promoted tx. This check fires the alarm.
    pub async fn assert_tx_promoted_on_all_nodes(
        &mut self,
        tx_id: irys_types::H256,
        timeout: Duration,
    ) -> Result<(), ClusterError> {
        let client = reqwest::Client::new();
        let urls = self.checked_api_urls()?;
        for url in &urls {
            crate::data_tx::wait_for_promotion(&client, url, tx_id, timeout)
                .await
                .map_err(|e| {
                    ClusterError::DataTx(format!(
                        "tx {tx_id} did not report promoted at {url}: {e}"
                    ))
                })?;
        }
        Ok(())
    }

    /// Strict assertion that **every** running node returns content for the
    /// given transaction that exactly matches what we originally signed and
    /// submitted. The plain `submit_and_verify_data` flow only checks that
    /// `/v1/tx/{id}` responds 2xx — that's enough to catch missing data
    /// but not silent corruption from cross-version mis-decoding. Use this
    /// after operations that change a node's binary version on top of a
    /// populated database (in particular: rollbacks).
    pub async fn assert_tx_matches_on_all_nodes(
        &mut self,
        expected: &irys_types::DataTransaction,
    ) -> Result<(), ClusterError> {
        let client = reqwest::Client::new();
        let urls = self.checked_api_urls()?;
        for url in &urls {
            crate::data_tx::assert_tx_matches_original(
                &client,
                url,
                expected,
                &self.run_config.tx_header,
            )
            .await
            .map_err(|e| ClusterError::DataTx(format!("on {url}: {e}")))?;
        }
        Ok(())
    }

    /// Asserts every block the cluster has finalized is served byte-
    /// identically by every running node. Enumerates `/v1/block-index`
    /// from the genesis, then for each block hash fetches the full
    /// `/v1/block/{hash}` from every other node and diffs against the
    /// genesis's view via [`crate::data_tx::compare_full_object`].
    ///
    /// This catches a class of cross-version drift the tx-header check
    /// misses: `BlockHeader` carries enum-tagged sub-fields (PoA proofs,
    /// ledger metadata, signatures) whose `Compact`/JSON shape can drift
    /// across schema versions independently of the tx-header layout.
    /// Iterating the whole index — rather than tracking per-tx block
    /// hashes via `/v1/tx/{id}/status` — keeps the check working on
    /// OLD-only and OLD/NEW-mixed clusters, since `block-index` exists
    /// on both versions but `/status` is HEAD-only.
    pub async fn assert_block_index_consistent(&mut self) -> Result<(), ClusterError> {
        let client = reqwest::Client::new();
        let genesis_url = self.find_running_genesis_url()?;
        let block_hashes =
            crate::data_tx::fetch_block_index_hashes(&client, &genesis_url, BLOCK_INDEX_LIMIT)
                .await
                .map_err(|e| {
                    ClusterError::DataTx(format!("fetching block-index from genesis: {e}"))
                })?;

        let urls = self.checked_api_urls()?;
        for block_hash in &block_hashes {
            let reference = crate::data_tx::fetch_block_header(&client, &genesis_url, block_hash)
                .await
                .map_err(|e| {
                    ClusterError::DataTx(format!(
                        "fetching reference block {block_hash} from genesis: {e}"
                    ))
                })?;

            for url in &urls {
                if url == &genesis_url {
                    continue;
                }
                let actual = crate::data_tx::fetch_block_header(&client, url, block_hash)
                    .await
                    .map_err(|e| {
                        ClusterError::DataTx(format!("fetching block {block_hash} from {url}: {e}"))
                    })?;
                crate::data_tx::compare_full_object(
                    &format!("block {block_hash} at {url} vs genesis"),
                    &reference,
                    &actual,
                    &self.run_config.block_header,
                )
                .map_err(|e| ClusterError::DataTx(e.to_string()))?;
            }
        }
        Ok(())
    }

    /// Runtime view of the chain parameters that test code needs to
    /// construct signed payloads *and* pace cross-version transitions —
    /// queried once from the running genesis's `/v1/network/config` instead
    /// of being hard-coded as constants in test source. The migration depth
    /// in particular has to come from the live chain because rollback tests
    /// must wait that many blocks past a tx submission to force the on-disk
    /// `IrysDataTxHeaders` table to be populated before a binary swap.
    pub async fn fetch_chain_params(&mut self) -> Result<ChainParams, ClusterError> {
        let genesis_url = self.find_running_genesis_url()?;
        let consensus = self.probe.get_network_config(&genesis_url).await?;
        Ok(ChainParams {
            chain_id: read_consensus_u64(&consensus, "chainId")?,
            chunk_size: read_consensus_u64(&consensus, "chunkSize")?,
            block_migration_depth: read_consensus_u64(&consensus, "blockMigrationDepth")?,
        })
    }

    pub async fn upgrade_node(
        &mut self,
        name: &str,
        new_binary: &ResolvedBinary,
    ) -> Result<(), ClusterError> {
        // Snapshot identity + template up front so we don't hold a borrow on
        // self.identities or self.probe while we mutably borrow self.nodes
        // below.
        let identity = self
            .identities
            .get(name)
            .ok_or_else(|| ClusterError::NodeNotFound(name.into()))?;
        let role = identity.role;
        let mining_key = identity.mining_key.clone();
        let reward_address = identity.reward_address.clone();
        let peer_names = identity.peers.clone();
        let new_kind = new_binary.kind;
        let template = self.template_for(new_kind).to_owned();
        let peer_endpoints = build_peer_endpoints(&peer_names, &self.port_map)?;

        // Every node — peer or genesis — needs a fresh `[consensus.Custom]`
        // written into its config before the new binary starts, so it sees
        // the chain's actual `expected_genesis_hash` and a consensus shape
        // it can parse. We resolve this *before* shutting the node down so
        // the genesis (still up — possibly the very node we're about to
        // upgrade) can answer.
        //
        // The cross-version escape hatch: if the new base-config template
        // carries a hand-authored `[consensus.Custom]` block, those keys are
        // overlaid on top of the genesis-served values. This lets users
        // backfill fields the old genesis doesn't know about (e.g. new
        // consensus parameters added between versions) without losing the
        // genesis's runtime values for shared fields like `chain_id` or the
        // `genesis` block. Template absence falls through to plain re-fetch,
        // matching the initial-startup `patch_peer_configs` flow.
        //
        // Genesis upgrades use the same path because the running genesis is
        // its own source of truth — we want to preserve its on-chain
        // consensus state across the binary swap. Querying it before
        // shutdown is the same network call the peers make.
        let template_overlay = crate::config::extract_consensus_custom_from_template(&template);
        let genesis_url = self.find_running_genesis_url()?;
        let genesis_hash = self.probe.get_genesis_hash(&genesis_url).await?;
        let consensus_json = self.probe.get_network_config(&genesis_url).await?;
        let peer_consensus_patch = PeerConsensusPatch {
            genesis_hash,
            consensus_json,
            template_overlay,
        };

        let node = self
            .nodes
            .get_mut(name)
            .ok_or_else(|| ClusterError::NodeNotFound(name.into()))?;

        if node.is_running() {
            node.shutdown(SHUTDOWN_TIMEOUT).await?;
        }

        node.config.binary_path = new_binary.path.clone(); // clone: stored as new binary path
        node.config.version_label = new_binary.label.clone(); // clone: stored as new label

        // Schemas may differ across releases (fields added/removed), so the
        // on-disk config is regenerated from scratch using the template that
        // matches the new binary's kind. We do this *before* the new binary
        // starts so it never sees the old-shaped file.
        crate::config::generate_config(&ConfigParams {
            base_template: &template,
            role,
            api_port: node.config.api_port,
            gossip_port: node.config.gossip_port,
            reth_port: node.config.reth_port,
            data_dir: &node.config.data_dir,
            mining_key: &mining_key,
            reward_address: &reward_address,
            peer_endpoints: &peer_endpoints,
            output_path: &node.config.config_path,
        })?;

        // Re-patch consensus on every node — see the rationale where we
        // build `peer_consensus_patch` above.
        crate::config::patch_peer_consensus(
            &node.config.config_path,
            &peer_consensus_patch.consensus_json,
            &peer_consensus_patch.genesis_hash,
            peer_consensus_patch.template_overlay.as_ref(),
        )?;

        match role {
            NodeRole::Genesis => {
                // Genesis upgrades keep the GENESIS env var; nothing to strip.
            }
            NodeRole::Peer => {
                node.config.env_vars.retain(|(k, _)| k != "GENESIS");
            }
        }

        node.respawn().await?;

        let url = node.api_url();
        wait_for_node_ready(&self.probe, node, &url, READY_TIMEOUT).await?;

        // Update the in-memory identity to reflect the new kind so subsequent
        // upgrades pick the right template.
        if let Some(identity) = self.identities.get_mut(name) {
            identity.kind = new_kind;
        }

        tracing::info!(node = %name, version = %new_binary.label, "node upgraded and ready");
        Ok(())
    }

    /// Finds the API URL of the cluster's genesis node, if it's currently
    /// running. Used by peer upgrades to re-fetch the chain's consensus
    /// config + genesis hash. Takes `&mut self` because `NodeProcess::is_running`
    /// reaps the child process, which requires mutable access.
    fn find_running_genesis_url(&mut self) -> Result<String, ClusterError> {
        let genesis_name = self
            .identities
            .iter()
            .find_map(|(name, identity)| {
                matches!(identity.role, NodeRole::Genesis).then(|| name.clone())
            })
            .ok_or(ClusterError::MissingGenesis)?;

        let node = self
            .nodes
            .get_mut(&genesis_name)
            .ok_or(ClusterError::MissingGenesis)?;
        if !node.is_running() {
            return Err(ClusterError::MissingGenesis);
        }
        Ok(node.api_url())
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

fn pick_template(spec: &ClusterSpec, kind: BinaryKind) -> &str {
    match kind {
        BinaryKind::Old => &spec.base_config_old,
        BinaryKind::New => &spec.base_config_new,
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
        if node.is_running()
            && let Err(e) = node.shutdown(SHUTDOWN_TIMEOUT).await
        {
            tracing::warn!(node = %name, error = %e, "cleanup: graceful shutdown failed, sending SIGKILL");
            if let Err(kill_err) = node.kill().await {
                tracing::error!(node = %name, error = %kill_err, "cleanup: SIGKILL also failed");
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
        let template = pick_template(spec, node_spec.binary.kind);
        let mut proc = build_node_process(node_spec, port_map, run_dir, template, &log_dir)?;
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
            let template = pick_template(spec, node_spec.binary.kind);
            generate_node_config(node_spec, port_map, run_dir, template)?;
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
        crate::config::patch_peer_consensus(&config_path, &consensus_json, &genesis_hash, None)?;
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
    // Take the port-reservation listeners — they will be held alive and
    // threaded into `NodeProcess::spawn` so they are only dropped
    // immediately before the child process is exec'd, minimising the TOCTOU
    // window where another process could steal the ports.
    let entry = port_map
        .get_mut(&node_spec.name)
        .ok_or_else(|| ClusterError::NodeNotFound(node_spec.name.clone()))?;
    let port_guards = entry.take_guards();
    let ports = &*entry;
    let mut env_vars = vec![("RUST_LOG".to_owned(), "debug".to_owned())];
    if matches!(node_spec.role, NodeRole::Genesis) {
        env_vars.push(("GENESIS".to_owned(), "true".to_owned()));
    }

    let log_file = log_dir.join(format!("{}.log", node_spec.name));
    let proc = NodeProcess::spawn(
        NodeProcessConfig {
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
        },
        port_guards,
    )?;

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
    let name = proc.config.name.clone();

    loop {
        if !proc.is_running() {
            let logs = format_log_tail(proc);
            let log_path = proc.config.log_file.display();
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
            let log_path = proc.config.log_file.display();
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
    let start = lines.len().saturating_sub(50);
    let mut out: Vec<&str> = lines
        .iter()
        .skip(start)
        .map(std::string::String::as_str)
        .collect();
    if start > 0 {
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
