use alloy_eips::BlockNumberOrTag;
use alloy_primitives::Address;
use alloy_rpc_types_engine::PayloadAttributes;
use irys_database::db::RethDbWrapper;
use irys_reth::{IrysEngineValidatorBuilder, IrysEthereumNode, IrysPayloadBuilderAttributes};
use irys_types::{
    config::node::{RethConfig, RethEngineConfig, RethRpcConfig, RethTxPoolConfig},
    NetworkConfigWithDefaults as _,
};
use reth::{
    args::{DatabaseArgs, EngineArgs, MetricArgs, RpcServerArgs, TxPoolArgs},
    revm::primitives::B256,
    tasks::TaskExecutor,
};
use reth_chainspec::ChainSpec;
use reth_db::{init_db, mdbx::MEGABYTE};
use reth_node_builder::{
    rpc::RpcAddOns, FullNode, FullNodeTypesAdapter, Node, NodeAdapter, NodeBuilder,
    NodeComponentsBuilder, NodeConfig, NodeHandle, NodeTypesWithDBAdapter,
};
use reth_provider::providers::BlockchainProvider;
use reth_rpc_eth_api::EthApiServer as _;
use std::future::IntoFuture as _;
use std::net::SocketAddr;
use std::{fmt::Debug, ops::Deref};
use std::{fmt::Formatter, sync::Arc};
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

/// Build Reth's `RpcServerArgs` from our TOML-serializable config.
///
/// Starts from Reth's defaults (preserving fields we don't expose in our config)
/// and overrides only the fields we manage. The `bind_ip` is used for both HTTP
/// and WS bind addresses.
fn build_rpc_args(rpc: &RethRpcConfig, bind_ip: &str) -> eyre::Result<RpcServerArgs> {
    let mut args = RpcServerArgs::default();

    // HTTP
    args.http = rpc.http;
    args.http_addr = bind_ip.parse()?;
    args.http_port = rpc.http_port;
    args.http_api =
        Some(rpc.http_api.parse().map_err(|e| {
            eyre::eyre!("invalid reth.rpc.http_api modules {:?}: {e}", rpc.http_api)
        })?);
    args.http_corsdomain = Some(rpc.http_corsdomain.clone());

    // WebSocket
    args.ws = rpc.ws;
    args.ws_addr = bind_ip.parse()?;
    args.ws_port = rpc.ws_port;
    if rpc.ws {
        args.ws_api =
            Some(rpc.ws_api.parse().map_err(|e| {
                eyre::eyre!("invalid reth.rpc.ws_api modules {:?}: {e}", rpc.ws_api)
            })?);
    }

    // Limits
    args.rpc_max_request_size.0 = rpc.max_request_size_mb;
    args.rpc_max_response_size.0 = rpc.max_response_size_mb;
    args.rpc_max_connections.0 = rpc.max_connections;
    args.rpc_gas_cap = rpc.gas_cap;
    args.rpc_tx_fee_cap = rpc.tx_fee_cap as u128;

    Ok(args)
}

/// Build Reth's `TxPoolArgs` from our TOML-serializable config.
///
/// Starts from Reth's defaults and overrides the subset we expose.
/// Always disables blob support — Irys doesn't use EIP-4844 blobs.
fn build_txpool_args(txpool: &RethTxPoolConfig) -> TxPoolArgs {
    TxPoolArgs {
        pending_max_count: txpool.pending_max_count,
        pending_max_size: txpool.pending_max_size_mb,
        basefee_max_count: txpool.basefee_max_count,
        basefee_max_size: txpool.basefee_max_size_mb,
        queued_max_count: txpool.queued_max_count,
        queued_max_size: txpool.queued_max_size_mb,
        additional_validation_tasks: txpool.additional_validation_tasks,
        max_account_slots: txpool.max_account_slots,
        price_bump: txpool.price_bump as u128,
        // Important: Irys doesn't use EIP-4844 blobs
        disable_blobs_support: true,
        ..Default::default()
    }
}

/// Build Reth's `EngineArgs` from our TOML-serializable config.
///
/// Only overrides persistence threshold and memory buffer target;
/// all other engine settings use Reth's defaults.
fn build_engine_args(engine: &RethEngineConfig) -> EngineArgs {
    EngineArgs {
        persistence_threshold: engine.persistence_threshold,
        memory_block_buffer_target: engine.memory_block_buffer_target,
        ..Default::default()
    }
}

/// Build Reth's `MetricArgs` from our config.
///
/// When `random_ports` is true (tests), uses port 0 so the OS picks a free port.
fn build_metric_args(
    reth: &RethConfig,
    bind_ip: &str,
    random_ports: bool,
) -> eyre::Result<MetricArgs> {
    let port = if random_ports { 0 } else { reth.metrics.port };
    let addr: SocketAddr = format!("{bind_ip}:{port}").parse()?;
    Ok(MetricArgs {
        prometheus: Some(addr),
        ..Default::default()
    })
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

    let irys_reth = &node_config.reth;
    let bind_ip = irys_reth.network.bind_ip(&node_config.network_defaults);

    // -- Network (no Reth NetworkArgs equivalent we'd want to wholesale-assign;
    //    we only set port/addr and always disable discovery) --
    reth_config.network.discovery.disable_discovery = true;
    reth_config.network.port = irys_reth.network.bind_port;
    reth_config.network.addr = bind_ip.parse()?;

    // -- Datadir --
    reth_config.datadir.datadir = node_config.reth_data_dir().into();

    // Build Reth arg structs from our TOML-serializable config.
    // This reuses Reth's own types (RpcServerArgs, TxPoolArgs, EngineArgs),
    // replacing the field-by-field mapping that was here before.
    reth_config.rpc = build_rpc_args(&irys_reth.rpc, bind_ip)?;
    reth_config.txpool = build_txpool_args(&irys_reth.txpool);
    reth_config.engine = build_engine_args(&irys_reth.engine);
    reth_config.metrics = build_metric_args(irys_reth, bind_ip, random_ports)?;

    let db_args = DatabaseArgs::default();
    // TODO: figure out if we shouldn't use smaller growth steps in production
    let db_arguments = db_args
        .database_args()
        .with_growth_step((10 * MEGABYTE).into())
        .with_shrink_threshold((20 * MEGABYTE).try_into()?);

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
