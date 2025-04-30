//! Utilities for end-to-end tests.
//! Taken from ext/reth/crates/e2e-test-utils

use std::sync::{Arc, RwLock};

use irys_database::db::RethDbWrapper;
use irys_storage::reth_provider::IrysRethProviderInner;
use node::RethNodeContext;
use reth::{
    args::{DiscoveryArgs, NetworkArgs, RpcServerArgs},
    builder::{NodeBuilder, NodeConfig, NodeHandle},
    network::PeersHandleProvider,
    rpc::api::eth::{helpers::AddDevSigners, FullEthApiServer},
    tasks::TaskManager,
};
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_db::{test_utils::TempDatabase, DatabaseEnv};
use reth_node_builder::{
    components::NodeComponentsBuilder, rpc::EthApiBuilderProvider, FullNodeTypesAdapter, Node,
    NodeAdapter, NodeAddOns, NodeComponents, NodeTypesWithDBAdapter, NodeTypesWithEngine,
    RethFullAdapter,
};
use reth_provider::providers::{BlockchainProvider, BlockchainProvider2};
use tracing::{span, Level};
use wallet::Wallet;

use crate::launcher::CustomEngineNodeLauncher;

/// Wrapper type to create test nodes
pub mod node;

/// Helper for transaction operations
pub mod transaction;

/// Helper type to yield accounts from mnemonic
pub mod wallet;

/// Helper for payload operations
mod payload;

/// Helper for network operations
mod network;

/// Helper for engine api operations
mod engine_api;
/// Helper for rpc operations
mod rpc;

/// Helper traits
mod traits;

/// Creates the initial setup with `num_nodes` started and interconnected.
pub async fn setup<N>(
    num_nodes: usize,
    chain_spec: Arc<N::ChainSpec>,
    is_dev: bool,
) -> eyre::Result<(Vec<NodeHelperType<N, N::AddOns>>, TaskManager, Wallet)>
where
    N: Default + Node<TmpNodeAdapter<N>> + NodeTypesWithEngine<ChainSpec: EthereumHardforks>,
    N::ComponentsBuilder: NodeComponentsBuilder<
        TmpNodeAdapter<N>,
        Components: NodeComponents<TmpNodeAdapter<N>, Network: PeersHandleProvider>,
    >,
    N::AddOns: NodeAddOns<
        Adapter<N>,
        EthApi: FullEthApiServer + AddDevSigners + EthApiBuilderProvider<Adapter<N>>,
    >,
{
    let tasks = TaskManager::current();
    let exec = tasks.executor();

    let network_config = NetworkArgs {
        discovery: DiscoveryArgs {
            disable_discovery: true,
            ..DiscoveryArgs::default()
        },
        ..NetworkArgs::default()
    };

    // Create nodes and peer them
    let mut nodes: Vec<RethNodeContext<_, _>> = Vec::with_capacity(num_nodes);

    for idx in 0..num_nodes {
        let node_config = NodeConfig::new(chain_spec.clone())
            .with_network(network_config.clone())
            .with_unused_ports()
            .with_rpc(RpcServerArgs::default().with_unused_ports().with_http())
            .set_dev(is_dev);

        let span = span!(Level::INFO, "node", idx);
        let _enter = span.enter();
        // let irys_provider: Arc<RwLock<Option<IrysRethProviderInner>>> = Arc::new(RwLock::new(None));

        // use reth_engine_tree::tree::TreeConfig;
        // let engine_tree_config = TreeConfig::default();

        // let db = reth_db::test_utils::create_test_rw_db();

        // use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};
        let NodeHandle {
            node,
            node_exit_future: _,
        } = NodeBuilder::new(node_config.clone())
            // .with_database(db)
            // .with_types_and_provider::<EthereumNode, BlockchainProvider2<
            //     NodeTypesWithDBAdapter<EthereumNode, Arc<TempDatabase<DatabaseEnv>>>,
            // >>()
            // .testing_node(exec.clone())
            // //     .with_types_and_provider::<EthereumNode, BlockchainProvider2<
            // //     NodeTypesWithDBAdapter<EthereumNode, RethDbWrapper>,
            // // >>()
            // // .with_types::<EthereumNode>();
            // .with_components(
            //     // EthereumNode::components()
            //     //     .executor(IrysExecutorBuilder {
            //     //         precompile_state_provider: PrecompileStateProvider {
            //     //             provider: irys_provider.clone(),
            //     //         },
            //     //     })
            //     //     .payload(IrysPayloadBuilder::default()),
            //     Default::default(),
            // )
            // // .with_components(EthereumNode::components())
            // .with_add_ons(EthereumAddOns::default())
            // // .with_types_and_provider::<EthereumNode, BlockchainProvider2<
            // //     NodeTypesWithDBAdapter<EthereumNode, RethDbWrapper>,
            // // >>()
            // .launch_with_fn(|builder| {
            //     let launcher = CustomEngineNodeLauncher::new(
            //         builder.task_executor().clone(),
            //         builder.config().datadir(),
            //         engine_tree_config,
            //         irys_provider,
            //         latest_block,
            //     );
            //     builder.launch_with(launcher)
            // })
            .testing_node(exec.clone())
            .node(Default::default())
            .launch()
            // .with_components(EthereumNode::components())
            // .with_add_ons(EthereumAddOns::default())
            // .launch_with_fn(|builder| {
            //     let launcher = reth_node_builder::EngineNodeLauncher::new(
            //         tasks.executor(),
            //         builder.config.datadir(),
            //         Default::default(),
            //     );
            //     builder.launch_with(launcher)
            // })
            .await?;

        let mut node = RethNodeContext::new(node).await?;

        // Connect each node in a chain.
        if let Some(previous_node) = nodes.last_mut() {
            previous_node.connect(&mut node).await;
        }

        // Connect last node with the first if there are more than two
        if idx + 1 == num_nodes && num_nodes > 2 {
            if let Some(first_node) = nodes.first_mut() {
                node.connect(first_node).await;
            }
        }

        nodes.push(node);
    }

    Ok((
        nodes,
        tasks,
        Wallet::default().with_chain_id(chain_spec.chain().into()),
    ))
}

// Type aliases

type TmpDB = Arc<TempDatabase<DatabaseEnv>>;
type TmpNodeAdapter<N> = FullNodeTypesAdapter<
    NodeTypesWithDBAdapter<N, TmpDB>,
    BlockchainProvider<NodeTypesWithDBAdapter<N, TmpDB>>,
>;

type Adapter<N> = NodeAdapter<
    RethFullAdapter<TmpDB, N>,
    <<N as Node<TmpNodeAdapter<N>>>::ComponentsBuilder as NodeComponentsBuilder<
        RethFullAdapter<TmpDB, N>,
    >>::Components,
>;

/// Type alias for a type of `NodeHelper`
pub type NodeHelperType<N, AO> = RethNodeContext<Adapter<N>, AO>;
