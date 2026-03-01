use alloy_eips::BlockNumberOrTag;
use alloy_primitives::Address;
use alloy_rpc_types_engine::PayloadAttributes;
use irys_database::db::RethDbWrapper;
use irys_reth::{IrysEngineValidatorBuilder, IrysEthereumNode, IrysPayloadBuilderAttributes};
use irys_types::NetworkConfigWithDefaults as _;
use reth::{
    args::{DatabaseArgs, MetricArgs},
    revm::primitives::B256,
    rpc::builder::{RethRpcModule, RpcModuleSelection},
    tasks::TaskExecutor,
};
use reth_chainspec::ChainSpec;
use reth_db::{
    init_db,
    mdbx::{SyncMode, MEGABYTE},
};
use reth_node_builder::{
    rpc::RpcAddOns, FullNode, FullNodeTypesAdapter, Node, NodeAdapter, NodeBuilder,
    NodeComponentsBuilder, NodeConfig, NodeHandle, NodeTypesWithDBAdapter,
};
use reth_provider::providers::BlockchainProvider;
use reth_rpc_eth_api::EthApiServer as _;
use std::future::IntoFuture as _;
use std::net::SocketAddr;
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

pub type RethNodeAddOns = RpcAddOns<
    RethNodeAdapter,
    reth_node_ethereum::node::EthereumEthApiBuilder,
    IrysEngineValidatorBuilder,
>;

pub type RethNode = FullNode<RethNodeAdapter, RethNodeAddOns>;

pub fn eth_payload_attributes(timestamp: u64) -> IrysPayloadBuilderAttributes {
    use irys_reth::IrysPayloadAttributes;
    use reth_node_api::PayloadBuilderAttributes as _;

    let rpc_attributes = IrysPayloadAttributes {
        inner: PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
        },
        shadow_txs: vec![],
    };

    IrysPayloadBuilderAttributes::try_new(B256::ZERO, rpc_attributes, 0)
        .expect("IrysPayloadBuilderAttributes::try_new is infallible")
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
) -> eyre::Result<(RethNodeHandle, IrysRethNodeAdapter)> {
    let mut reth_config = NodeConfig::new(chainspec.clone());

    let unwind_runtime =
        reth::tasks::RuntimeBuilder::new(reth::tasks::RuntimeConfig::default().with_tokio(
            reth::tasks::TokioConfig::existing_handle(tokio::runtime::Handle::current()),
        ))
        .build()?;
    if let Err(e) = unwind_to(
        &node_config,
        chainspec.clone(),
        latest_block,
        unwind_runtime,
    )
    .await
    {
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

    reth_config.rpc.http_addr = node_config
        .reth
        .network
        .bind_ip(&node_config.network_defaults)
        .parse()?;
    reth_config.network.port = node_config.reth.network.bind_port;
    reth_config.network.addr = node_config
        .reth
        .network
        .bind_ip(&node_config.network_defaults)
        .parse()?;

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
    // important: keep blobs disabled in our mempool
    reth_config.txpool.disable_blobs_support = true;

    if cfg!(test) {
        reth_config.engine.cross_block_cache_size = 10 * MEGABYTE;
    } else {
        reth_config.txpool.additional_validation_tasks = 2;
    }

    // Enable Prometheus metrics endpoint for local scraping by OTEL collector sidecar.
    let metrics_port = if random_ports { "0" } else { "9001" };
    let metrics_addr: SocketAddr = format!(
        "{}:{}",
        node_config
            .reth
            .network
            .bind_ip(&node_config.network_defaults),
        metrics_port
    )
    .parse()?;
    reth_config.metrics = MetricArgs {
        prometheus: Some(metrics_addr),
        ..Default::default()
    };

    let db_args = DatabaseArgs::default();
    // TODO: figure out if we shouldn't use smaller growth steps in production
    let db_arguments = db_args
        .database_args()
        .with_growth_step((10 * MEGABYTE).into())
        .with_shrink_threshold((20 * MEGABYTE).try_into()?)
        .with_sync_mode(if cfg!(test) {
            Some(SyncMode::UtterlyNoSync)
        } else {
            Some(SyncMode::Durable)
        });

    let data_dir = reth_config.datadir();
    let db_path = data_dir.db();

    tracing::info!(
        target = "reth::cli",
        custom.path = ?db_path,
        "Opening database"
    );
    let database = RethDbWrapper::new(init_db(db_path.clone(), db_arguments)?.with_metrics());

    if random_ports {
        reth_config = reth_config.with_unused_ports();
    }

    let builder = NodeBuilder::new(reth_config)
        .with_database(database.clone())
        .with_launch_context(task_executor.clone());

    let handle = builder
        .node(IrysEthereumNode)
        .launch_with_debug_capabilities()
        .into_future()
        .in_current_span()
        .await?;

    let context = IrysRethNodeAdapter::new(handle.node.clone()).await?;
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
