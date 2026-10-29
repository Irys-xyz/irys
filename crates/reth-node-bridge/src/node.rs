use alloy_eips::BlockNumberOrTag;
use alloy_primitives::Address;
use alloy_rpc_types_engine::PayloadAttributes;
use clap::{CommandFactory as _, FromArgMatches as _, parser::ValueSource};
use irys_database::db::RethDbWrapper;
use irys_reth::{IrysEngineValidatorBuilder, IrysEthereumNode, IrysPayloadBuilderAttributes};
use irys_types::NetworkConfigWithDefaults as _;
use reth::{
    args::{DatabaseArgs, MetricArgs, RpcServerArgs},
    revm::primitives::B256,
    rpc::builder::{RethRpcModule, RpcModuleSelection, TransportRpcModules},
    tasks::TaskExecutor,
};
use reth_chainspec::ChainSpec;
use reth_db::{init_db, mdbx::MEGABYTE};
use reth_node_builder::{
    FullNode, FullNodeTypesAdapter, Node, NodeAdapter, NodeBuilder, NodeComponentsBuilder,
    NodeConfig, NodeHandle, NodeTypesWithDBAdapter, rpc::RpcAddOns,
};
use reth_provider::providers::BlockchainProvider;
use reth_rpc_eth_api::EthApiServer as _;
use std::future::IntoFuture as _;
use std::net::SocketAddr;
use std::{collections::HashSet, fmt::Formatter, sync::Arc};
use std::{fmt::Debug, ops::Deref};
use tracing::{Instrument as _, warn};

use crate::{IrysRethNodeAdapter, unwind::unwind_to};

type NodeTypesAdapter = FullNodeTypesAdapter<IrysEthereumNode, RethDbWrapper, NodeProvider>;

/// Type alias for a `NodeAdapter`
pub type RethNodeAdapter = NodeAdapter<
    NodeTypesAdapter,
    <<IrysEthereumNode as Node<NodeTypesAdapter>>::ComponentsBuilder as NodeComponentsBuilder<
        NodeTypesAdapter,
    >>::Components,
>;

pub type NodeProvider = BlockchainProvider<NodeTypesWithDBAdapter<IrysEthereumNode, RethDbWrapper>>;

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

/// Compute the `SocketAddr` for Reth's internal Prometheus endpoint.
///
/// Reads `node_config.metrics.bind_ip` (separate from
/// `network_defaults.bind_ip`, which still applies to HTTP / gossip / P2P
/// sockets that legitimately need to listen externally). Defaults to
/// `127.0.0.1` so an upgraded node stops publishing `:9001` to the public
/// interface; production deployments expose the endpoint through an
/// mTLS-terminating reverse proxy on the same host.
///
/// When `random_ports` is true, port `0` is used so the OS assigns a free
/// port — only used by integration tests.
fn compute_metrics_addr(
    node_config: &irys_types::NodeConfig,
    random_ports: bool,
) -> eyre::Result<SocketAddr> {
    let port = if random_ports { "0" } else { "9001" };
    let addr = format!("{}:{}", node_config.metrics.bind_ip.as_str(), port).parse()?;
    Ok(addr)
}

/// Wraps Reth's RPC server flags so `reth.rpc_args` parses like `reth node` RPC flags.
#[derive(clap::Parser)]
struct RethRpcFlags {
    #[command(flatten)]
    rpc: RpcServerArgs,
}

/// Clap argument ids of the [`RpcServerArgs`] fields that `run_node` sets.
/// `reth.rpc_args` must not set them.
const IRYS_OWNED_RPC_ARGS: [&str; 3] = ["http", "http_addr", "http_corsdomain"];

/// Parses `reth.rpc_args` into Reth's RPC server settings.
///
/// Fails if a flag is not valid or sets a field in [`IRYS_OWNED_RPC_ARGS`].
/// Without `--http.api`, the HTTP RPC serves only the `eth` namespace.
/// An explicit `--http.api` applies as given. See `RethConfig::rpc_args` for
/// the namespaces that are unsafe on a public port.
fn parse_rpc_args(rpc_args: &[String]) -> eyre::Result<RpcServerArgs> {
    let command = RethRpcFlags::command();
    let argv = std::iter::once("reth").chain(rpc_args.iter().map(String::as_str));
    let matches = command
        .clone()
        .try_get_matches_from(argv)
        .map_err(|e| eyre::eyre!("invalid reth.rpc_args: {e}"))?;

    for id in IRYS_OWNED_RPC_ARGS {
        if matches.value_source(id) == Some(ValueSource::CommandLine) {
            let flag = command
                .get_arguments()
                .find(|arg| arg.get_id() == id)
                .and_then(|arg| arg.get_long())
                .unwrap_or(id);
            eyre::bail!("invalid reth.rpc_args: --{flag} is set by Irys and cannot be changed");
        }
    }

    let mut rpc = RethRpcFlags::from_arg_matches(&matches)
        .map_err(|e| eyre::eyre!("invalid reth.rpc_args: {e}"))?
        .rpc;
    if matches.value_source("http_api") != Some(ValueSource::CommandLine) {
        rpc.http_api = Some(RpcModuleSelection::Selection(HashSet::from([
            RethRpcModule::Eth,
        ])));
    }
    Ok(rpc)
}

/// The `debug` namespace methods the Reth RPC serves: reads and traces only.
/// Every other `debug_*` method is removed, including methods a later Reth adds.
/// Methods that the pinned Reth does not implement are not listed, so an
/// implementation in a later Reth is not served without review.
const DEBUG_RPC_ALLOWED_METHODS: [&str; 17] = [
    "debug_getRawHeader",
    "debug_getRawBlock",
    "debug_getRawTransaction",
    "debug_getRawTransactions",
    "debug_getRawReceipts",
    "debug_getBadBlocks",
    "debug_traceBlock",
    "debug_traceBlockByHash",
    "debug_traceBlockByNumber",
    "debug_traceTransaction",
    "debug_traceCall",
    "debug_traceCallMany",
    "debug_executionWitness",
    "debug_executionWitnessByBlockHash",
    "debug_chainConfig",
    "debug_codeByHash",
    "debug_stateRootWithUpdates",
];

/// Removes every `debug_*` method not in [`DEBUG_RPC_ALLOWED_METHODS`] from the
/// http, ws and ipc transports.
fn remove_unlisted_debug_methods(modules: &mut TransportRpcModules) {
    let unlisted: Vec<&'static str> = modules
        .methods_by(|name| name.starts_with("debug_") && !DEBUG_RPC_ALLOWED_METHODS.contains(&name))
        .method_names()
        .collect();
    for name in unlisted {
        modules.remove_method_from_configured(name);
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
    reth_config.rpc = parse_rpc_args(&node_config.reth.rpc_args)?;

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

    if let Some(cache_size) = node_config.reth.cross_block_cache_size_megabytes {
        reth_config.engine.cross_block_cache_size = cache_size;
    }
    reth_config.txpool.additional_validation_tasks = node_config.reth.additional_validation_tasks;

    // Reth's internal Prometheus endpoint. Bound to
    // node_config.metrics.bind_ip (default 127.0.0.1) so external scrapers
    // must go through an mTLS-terminating reverse proxy configured by the
    // deployment layer.
    let metrics_addr = compute_metrics_addr(&node_config, random_ports)?;
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
        .with_sync_mode(Some(node_config.reth.db_sync_mode.into()));

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
        .extend_rpc_modules(|ctx| {
            remove_unlisted_debug_methods(ctx.modules);
            Ok(())
        })
        .launch_with_debug_capabilities()
        .into_future()
        .in_current_span()
        .await?;

    let context = IrysRethNodeAdapter::new(handle.node.clone());
    // check that the latest height lines up with the expected latest height from irys

    let latest = context
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard: the metrics socket must follow
    /// `node_config.metrics.bind_ip`, not the (intentionally external-by-
    /// default) `network_defaults.bind_ip`. Bind isolation here is the
    /// whole point of the dedicated `MetricsConfig` section.
    #[test]
    fn metrics_socket_uses_metrics_bind_ip_not_network_defaults() {
        let mut cfg = irys_types::NodeConfig::testing();
        cfg.network_defaults.bind_ip = "0.0.0.0".into();
        cfg.metrics.bind_ip = "127.0.0.1".into();

        let addr = compute_metrics_addr(&cfg, false).expect("valid addr");
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_eq!(addr.port(), 9001);
    }

    #[test]
    fn metrics_socket_random_ports_uses_port_zero() {
        let cfg = irys_types::NodeConfig::testing();
        let addr = compute_metrics_addr(&cfg, true).expect("valid addr");
        assert_eq!(addr.port(), 0);
    }

    /// Every transport keeps the listed `debug_*` methods and the other
    /// namespaces, and loses every other `debug_*` method.
    #[test]
    fn unlisted_debug_methods_are_removed_from_every_transport() {
        let mut module = jsonrpsee::RpcModule::new(());
        for name in [
            "debug_traceTransaction",
            "debug_traceChain",
            "debug_getBlockAccessList",
            "debug_storageRangeAt",
            "debug_setHead",
            "eth_chainId",
        ] {
            module
                .register_method(name, |_, _, _| "")
                .expect("unique method name");
        }
        let mut modules = TransportRpcModules::default()
            .with_http(module.clone())
            .with_ws(module.clone())
            .with_ipc(module);

        remove_unlisted_debug_methods(&mut modules);

        let expected = HashSet::from(["debug_traceTransaction", "eth_chainId"]);
        for methods in [
            modules.http_methods(|_| true),
            modules.ws_methods(|_| true),
            modules.ipc_methods(|_| true),
        ] {
            let served: HashSet<_> = methods
                .expect("transport configured")
                .method_names()
                .collect();
            assert_eq!(served, expected);
        }
    }

    fn to_args(flags: &[&str]) -> Vec<String> {
        flags.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn rpc_args_empty_serves_only_eth() {
        let rpc = parse_rpc_args(&[]).expect("empty rpc_args parse");
        assert_eq!(
            rpc,
            RpcServerArgs {
                http_api: Some(RpcModuleSelection::Selection(HashSet::from([
                    RethRpcModule::Eth
                ]))),
                ..Default::default()
            }
        );
    }

    #[test]
    fn rpc_args_valid_flags_are_applied() {
        let rpc = parse_rpc_args(&to_args(&[
            "--http.api=eth,debug",
            "--rpc.max-tracing-requests=4",
            "--rpc.gascap",
            "1000000",
        ]))
        .expect("valid rpc_args parse");
        assert_eq!(
            rpc.http_api,
            Some(RpcModuleSelection::Selection(HashSet::from([
                RethRpcModule::Eth,
                RethRpcModule::Debug
            ])))
        );
        assert_eq!(rpc.rpc_max_tracing_requests, 4);
        assert_eq!(rpc.rpc_gas_cap, 1_000_000);
    }

    #[test]
    fn rpc_args_unknown_flag_fails() {
        let err = parse_rpc_args(&to_args(&["--http.no-such-flag"])).unwrap_err();
        assert!(err.to_string().contains("reth.rpc_args"), "{err}");
    }

    #[rstest::rstest]
    #[case("--http", "--http")]
    #[case("--http.addr=0.0.0.0", "--http.addr")]
    #[case("--http.corsdomain=example.com", "--http.corsdomain")]
    fn rpc_args_irys_owned_flag_fails(#[case] arg: &str, #[case] flag: &str) {
        let err = parse_rpc_args(&to_args(&[arg])).unwrap_err().to_string();
        assert!(err.contains("reth.rpc_args"), "{err}");
        assert!(err.contains(&format!("{flag} is set by Irys")), "{err}");
    }

    /// Each owned id must name a Reth RPC argument, or the check above
    /// would not see the flag.
    #[test]
    fn irys_owned_rpc_args_exist() {
        let command = RethRpcFlags::command();
        for id in IRYS_OWNED_RPC_ARGS {
            assert!(
                command.get_arguments().any(|arg| arg.get_id() == id),
                "{id} is not an RpcServerArgs argument"
            );
        }
    }
}
