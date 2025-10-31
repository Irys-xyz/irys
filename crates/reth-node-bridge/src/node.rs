use alloy_eips::BlockNumberOrTag;
use alloy_rpc_types_engine::PayloadAttributes;
use irys_database::db::RethDbWrapper;
use irys_reth::{payload::ShadowTxStore, IrysEthereumNode};
use irys_types::Address;
use reth::{
    args::DatabaseArgs,
    payload::EthPayloadBuilderAttributes,
    prometheus_exporter::install_prometheus_recorder,
    revm::primitives::B256,
    rpc::builder::{RethRpcModule, RpcModuleSelection},
    tasks::TaskExecutor,
};
use reth_chainspec::ChainSpec;
use reth_db::init_db;
use reth_node_builder::{
    FullNode, FullNodeTypesAdapter, Node, NodeAdapter, NodeBuilder, NodeComponentsBuilder,
    NodeConfig, NodeHandle, NodeTypesWithDBAdapter,
};
use reth_provider::providers::BlockchainProvider;
use reth_rpc_eth_api::EthApiServer as _;
use std::{collections::HashSet, fmt::Formatter, sync::Arc};
use std::{fmt::Debug, ops::Deref};
use tracing::{warn, Instrument as _};

use crate::{unwind::unwind_to, IrysRethNodeAdapter};
pub use reth_e2e_test_utils::node::NodeTestContext;

type NodeTypesAdapter = FullNodeTypesAdapter<IrysEthereumNode, RethDbWrapper, NodeProvider>;

/// Type alias for a `NodeAdapter`
pub type RethNodeAdapter = NodeAdapter<
    NodeTypesAdapter,
    <<IrysEthereumNode as Node<NodeTypesAdapter>>::ComponentsBuilder as NodeComponentsBuilder<
        NodeTypesAdapter,
    >>::Components,
>;

pub type NodeProvider = BlockchainProvider<NodeTypesWithDBAdapter<IrysEthereumNode, RethDbWrapper>>;

pub type NodeHelperType =
    NodeTestContext<RethNodeAdapter, <IrysEthereumNode as Node<NodeTypesAdapter>>::AddOns>;

pub type RethNodeHandle = NodeHandle<RethNodeAdapter, RethNodeAddOns>;

pub type RethNodeAddOns = reth_node_ethereum::node::EthereumAddOns<RethNodeAdapter>;

pub type RethNode = FullNode<RethNodeAdapter, RethNodeAddOns>;

pub fn eth_payload_attributes(timestamp: u64) -> EthPayloadBuilderAttributes {
    let attributes = PayloadAttributes {
        timestamp,
        prev_randao: B256::ZERO,
        suggested_fee_recipient: Address::ZERO,
        withdrawals: Some(vec![]),
        parent_beacon_block_root: Some(B256::ZERO),
    };
    EthPayloadBuilderAttributes::new(B256::ZERO, attributes)
}

#[derive(Clone)]
pub struct RethNodeProvider(pub Arc<RethNode>);

impl Debug for RethNodeProvider {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "RethNodeProvider")
    }
}

impl Deref for RethNodeProvider {
    type Target = Arc<RethNode>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<RethNodeProvider> for RethNode {
    fn from(val: RethNodeProvider) -> Self {
        val.0.as_ref().clone()
    }
}

pub async fn run_node(
    chainspec: Arc<ChainSpec>,
    task_executor: TaskExecutor,
    node_config: irys_types::NodeConfig,
    latest_block: u64,
    random_ports: bool,
    shadow_tx_store: ShadowTxStore,
) -> eyre::Result<(RethNodeHandle, IrysRethNodeAdapter)> {
    let mut reth_config = NodeConfig::new(chainspec.clone());

    if let Err(e) = unwind_to(&node_config, chainspec.clone(), latest_block).await {
        // hack to ignore trying to unwind future blocks
        // (this can happen sometimes, but should be resolved by the payload repair process - erroring here won't help.)
        if e.to_string().starts_with("Target block number") {
            warn!("Error unwinding - Reth/Irys head block mismatch {}", &e)
        } else {
            return Err(e);
        }
    }

    reth_config.network.discovery.disable_discovery = true;
    reth_config.rpc.http = true;
    reth_config.rpc.http_api = Some(RpcModuleSelection::Selection(HashSet::from([
        RethRpcModule::Eth,
        RethRpcModule::Debug,
    ])));

    reth_config.rpc.http_addr = node_config.reth.network.bind_ip.parse()?;
    reth_config.network.port = node_config.reth.network.bind_port;
    reth_config.network.addr = node_config.reth.network.bind_ip.parse()?;

    reth_config.datadir.datadir = node_config.reth_data_dir().into();
    reth_config.rpc.http_corsdomain = Some("*".to_string());
    reth_config.engine.persistence_threshold = 0;
    reth_config.engine.memory_block_buffer_target = 0;

    let subpool_max_tx_count = 1_000_000;
    let subpool_max_size_mb = 1000;

    reth_config.txpool.pending_max_count = subpool_max_tx_count;
    reth_config.txpool.pending_max_size = subpool_max_size_mb;

    reth_config.txpool.basefee_max_count = subpool_max_tx_count;
    reth_config.txpool.basefee_max_size = subpool_max_size_mb;

    reth_config.txpool.queued_max_count = subpool_max_tx_count;
    reth_config.txpool.queued_max_size = subpool_max_size_mb;

    reth_config.txpool.additional_validation_tasks = 2;

    let db_args = DatabaseArgs::default();
    // Install the prometheus recorder to be sure to record all metrics
    let _ = install_prometheus_recorder();

    let data_dir = reth_config.datadir();
    let db_path = data_dir.db();

    tracing::info!(
        target = "reth::cli",
        custom.path = ?db_path,
        "Opening database"
    );
    let database =
        RethDbWrapper::new(init_db(db_path.clone(), db_args.database_args())?.with_metrics());

    if random_ports {
        reth_config = reth_config.with_unused_ports();
    }

    let builder = NodeBuilder::new(reth_config)
        .with_database(database.clone())
        .with_launch_context(task_executor.clone());

    let max_pd_chunks_per_block = node_config
        .consensus_config()
        .mempool
        .max_pd_chunks_per_block;

    let handle = builder
        .node(IrysEthereumNode {
            shadow_tx_store: shadow_tx_store.clone(),
            max_pd_chunks_per_block,
        })
        .launch_with_debug_capabilities()
        .in_current_span()
        .await?;

    let context = IrysRethNodeAdapter::new(handle.node.clone(), shadow_tx_store).await?;
    // check that the latest height lines up with the expected latest height from irys

    let latest = context
        .rpc
        .inner
        .eth_api()
        .block_by_number(BlockNumberOrTag::Latest, false)
        .await?
        .expect("latest block should be Some");

    if latest.header.number > latest_block {
        //Note: if this happens, let Jesse know ASAP
        eyre::bail!(
            "Error: Reth is ahead of Irys (Reth: {}, Irys: {})",
            &latest.header.number,
            &latest_block
        )
    }

    Ok((handle, context))
}
