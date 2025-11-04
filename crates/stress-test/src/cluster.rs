use irys_chain::IrysNode;
use irys_types::irys::IrysSigner;
use irys_types::{Address, NodeConfig, NodeMode, PeerAddress, SyncMode};
use alloy_genesis::{GenesisAccount};
use alloy_primitives::private::rand;

pub(crate) struct NodeHandle {
    config: NodeConfig,
    thread_handle: Option<std::thread::JoinHandle<()>>,
    kill_signal: Option<tokio::sync::oneshot::Sender<NodeCommand>>,
}

impl NodeHandle {
    pub(crate) fn new(config: NodeConfig) -> Self {
        Self {
            config,
            thread_handle: None,
            kill_signal: None,
        }
    }

    pub(crate) fn set_trusted_peers(&mut self, trusted_peers: &[PeerAddress]) {
        self.config.trusted_peers = trusted_peers.to_vec();
    }
}

impl From<NodeBuilder> for NodeHandle {
    fn from(builder: NodeBuilder) -> Self {
        builder.build()
    }
}

pub(crate) enum NodeCommand {
    Kill,
    Stop,
}

pub(crate) struct NodeBuilder {
    config: NodeConfig,
    /// Those fields are used to regenerate peer configs
    sync_mode: SyncMode,
    /// If set to true, the config won't be regenerated
    custom: bool,
}

impl NodeBuilder {
    pub(crate) fn new(base_config: &NodeConfig, sync_mode: SyncMode) -> Self {
        let mut this = Self {
            config: NodeConfig::testing(),
            sync_mode,
            custom: false,
        };
        this.regenerate_peer_config(base_config);
        this
    }

    pub(crate) fn custom(config: NodeConfig) -> Self {
        let sync_mode = config.sync_mode;
        Self {
            config,
            sync_mode,
            custom: true,
        }
    }

    pub(crate) fn regenerate_peer_config(&mut self, base_config: &NodeConfig) {
        let peer_signer = IrysSigner::random_signer(&base_config.consensus_config());

        let node_config = base_config;

        if matches!(node_config.node_mode, NodeMode::Peer) {
            panic!("Can only create a peer from a genesis config");
        }

        let mut peer_config = node_config.clone();
        peer_config.mining_key = peer_signer.signer.clone();
        peer_config.reward_address = peer_signer.address();

        // Set peer mode and expected genesis hash via consensus config
        peer_config.node_mode = NodeMode::Peer;
        // TODO: fix that
        // peer_config.consensus.get_mut().expected_genesis_hash = Some(self.node_ctx.genesis_hash);

        // Make sure this peer does port randomization instead of copying the genesis ports
        peer_config.http.bind_port = 0;
        peer_config.http.public_port = 0;
        peer_config.gossip.bind_port = 0;
        peer_config.gossip.public_port = 0;

        peer_config.sync_mode = self.sync_mode;
    }

    pub(crate) fn build(self) -> NodeHandle {
        NodeHandle::new(self.config)
    }
}

/// Builder for a cluster of Irys nodes.
/// If you just want one genesis node, you can do `ClusterBuilder::new().build(false)`.
/// To add peers that are going to connect to the genesis node, use `add_peers`.
pub(crate) struct ClusterBuilder {
    genesis_config: NodeConfig,
    nodes: Vec<NodeBuilder>,
    funded_accounts: Vec<IrysSigner>,
    unfunded_accounts: Vec<Address>,
}

impl ClusterBuilder {
    /// Crates a new cluster builder with one genesis node with default testing config and one
    /// funded account.
    pub(crate) fn new() -> Self {
        let mut this = Self {
            genesis_config: NodeConfig::testing(),
            nodes: Vec::new(),
            funded_accounts: Vec::new(),
            unfunded_accounts: Vec::new(),
        };
        this.add_genesis_account(1_000_000_000_000_000_000_u128)
    }

    pub(crate) fn with_custom_genesis_config(mut self, config: NodeConfig) -> Self {
        self.genesis_config = config;
        self.regenerate_all_peers();
        self
    }

    pub(crate) fn add_peers(mut self, count: usize, sync_mode: SyncMode) -> Self {
        for _ in 0..count {
            let node_builder = NodeBuilder::new(&self.genesis_config, sync_mode);
            self.nodes.push(node_builder);
        }
        self
    }

    pub(crate) fn add_custom_node_config(mut self, config: NodeConfig) -> Self {
        self.nodes.push(NodeBuilder::custom(config));
        self
    }

    pub(crate) fn build(mut self, regenerate_all_peers: bool) -> Cluster {
        if regenerate_all_peers {
            self.regenerate_all_peers();
        }
        let mut nodes = vec![NodeHandle::new(self.genesis_config)];
        nodes.append(
            self.nodes
                .into_iter()
                .map(NodeHandle::from)
                .collect::<Vec<_>>()
                .as_mut(),
        );
        let accounts = self.funded_accounts.clone();
        Cluster { nodes, accounts }
    }

    fn regenerate_all_peers(&mut self) {
        self.nodes
            .iter_mut()
            .for_each(|node| node.regenerate_peer_config(&self.genesis_config));
    }

    fn add_genesis_account(mut self, funds: u128) -> Self {
        let signer = IrysSigner::random_signer(&self.genesis_config.consensus_config());
        let address = signer.address();
        self.genesis_config.consensus.extend_genesis_accounts(
            vec![(signer.address(), GenesisAccount {
                balance: alloy_primitives::U256::from(funds),
                ..Default::default()
            })],
        );
        self.funded_accounts.push(signer.clone());
        self.regenerate_all_peers();
        self
    }
}

pub(crate) struct Cluster {
    nodes: Vec<NodeHandle>,
    accounts: Vec<IrysSigner>
}

impl Cluster {
    /// Starts all nodes in the cluster sequentially, starting with the genesis node - it is stored
    /// at index 0.
    pub(crate) async fn start_all(&mut self) {
        for node_id in 0..self.nodes.len() {
            self.start_node(node_id).await;
        }
    }

    pub(crate) fn set_trusted_peers(&mut self, node_id: usize, trusted_peers: &[PeerAddress]) {
        let handle = self.nodes.get_mut(node_id).unwrap();
        handle.set_trusted_peers(trusted_peers)
    }

    pub(crate) fn set_trusted_peers_for_all(&mut self, trusted_peers: &[PeerAddress]) {
        for handle in self.nodes.iter_mut() {
            handle.set_trusted_peers(trusted_peers)
        }
    }

    pub(crate) async fn start_node(&mut self, node_id: usize) {
        let config = self.nodes.get(node_id).unwrap().config.clone();
        let (kill_sender, mut kill_receiver) = tokio::sync::oneshot::channel::<NodeCommand>();
        let (readiness_sender, readiness_receiver) = tokio::sync::oneshot::channel::<()>();

        let thread_handle = std::thread::spawn(move || {
            // Create a Tokio runtime inside this thread
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("Failed to build Tokio runtime for the node thread");

            rt.block_on(async move {
                let node_context = IrysNode::new(config).unwrap().start().await.unwrap();

                // CloneableJoinHandle<()>, join is blocking -> move it to a blocking pool
                let reth_thread_handle = node_context.reth_thread_handle.clone().unwrap();
                let mut join_task = tokio::task::spawn_blocking(move || reth_thread_handle.join());

                // We may need to move the context only on kill
                let mut node_ctx_opt = Some(node_context);
                readiness_sender.send(()).unwrap();

                tokio::select! {
                    // Wait until the reth thread finishes
                    res = &mut join_task => {
                        match res {
                            Ok(Ok(_)) => {
                                println!("Exit..");
                            }
                            Ok(Err(_panic)) => {
                                eprintln!("Reth thread panicked while joining");
                            }
                            Err(e) => {
                                eprintln!("spawn_blocking join task errored: {:?}", e);
                            }
                        }
                    }
                    // Or wait for an external kill signal
                    msg = &mut kill_receiver => {
                        match msg {
                            Ok(NodeCommand::Kill) => {
                                println!("Received kill signal for node");
                                panic!("Killing node as per request");
                            }
                            Ok(NodeCommand::Stop) => {
                                if let Some(ctx) = node_ctx_opt.take() {
                                    println!("Shutting down node...");
                                    // Graceful shutdown of the node; this will also attempt to join the reth thread internally
                                    ctx.stop().await;
                                }
                                // Ensure the blocking join finishes as well
                                let _ = join_task.await;
                            }
                            Err(_e) => {
                                eprintln!("Kill signal channel closed unexpectedly");
                            }
                        }
                    }
                }
            });
        });

        // Save handles
        let handle = self.nodes.get_mut(node_id).unwrap();
        handle.thread_handle = Some(thread_handle);
        handle.kill_signal = Some(kill_sender);

        readiness_receiver.await.unwrap();
    }

    /// Kills the node with the given ID. Returns true if the node panicked during shutdown.
    pub(crate) fn crash_node(&mut self, node_id: usize) -> bool {
        let handle = self.nodes.get_mut(node_id).unwrap();
        if let Some(kill_signal) = handle.kill_signal.take() {
            let _ = kill_signal.send(NodeCommand::Kill);
        }
        match handle.thread_handle.take().unwrap().join() {
            Ok(_) => {
                println!("Node {} shut down successfully", node_id);
                false
            }
            Err(e) => {
                eprintln!("Node {} thread panicked: {:?}", node_id, e);
                true
            }
        }
    }

    pub async fn stop_node(&mut self, node_id: usize) -> Result<(), Box<dyn std::any::Any + Send>> {
        let handle = self.nodes.get_mut(node_id).unwrap();
        if let Some(kill_signal) = handle.kill_signal.take() {
            let _ = kill_signal.send(NodeCommand::Stop);
        }
        match handle.thread_handle.take().unwrap().join() {
            Ok(_) => {
                println!("Node {} shut down successfully", node_id);
                Ok(())
            }
            Err(e) => {
                eprintln!("Node {} thread panicked: {:?}", node_id, e);
                Err(e)
            }
        }
    }

    pub async fn stop_all(&mut self) {
        for node_id in 0..self.nodes.len() {
            let _ = self.stop_node(node_id).await;
        }
    }

    pub fn send_funds(&self, from: &IrysSigner, to: &Address, amount: u128) {

    }

    pub fn send_funds_from_random_genesis_account(&self, to: &Address, amount: u128) {
        let idx = rand::random_range((0..self.accounts.len()));
        let from = &self.accounts[idx];
        self.send_funds(from, to, amount);
    }
}

#[tokio::test]
async fn the_node_should_panic_on_kill_signal() {
    // TODO: make every node set up its own temp dir for db storage
    // By default, ClusterBuilder creates a network with one genesis node.
    let mut cluster = ClusterBuilder::new().build(false);
    cluster.start_node(0).await;
    let panicked = cluster.crash_node(0);
    assert!(panicked);
}

#[tokio::test]
async fn multiple_peers_should_start_and_connect() {
    let mut cluster = ClusterBuilder::new()
        .add_genesis_account(1_000_000_000_000_000_000_u128)
        .add_peers(3, SyncMode::Full)
        .build(true);
    cluster.start_all().await;
}

// TODO: create a test where your score drops below the active threshold, then connect a new peers
//  and see what happens