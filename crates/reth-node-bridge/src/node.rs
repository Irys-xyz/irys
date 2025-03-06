use core::fmt;
use std::{fs::canonicalize, future::Future, ops::Deref, sync::Arc, sync::RwLock};

use clap::{command, Args, Parser};
use irys_config::IrysNodeConfig;
use irys_storage::reth_provider::IrysRethProvider;
use reth::{
    chainspec::EthereumChainSpecParser,
    cli::{Cli, Commands},
    core::irys_ext::NodeExitReason,
    prometheus_exporter::install_prometheus_recorder,
    version, CliContext,
};
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::NodeCommand;
use reth_consensus::Consensus;
use reth_db::{init_db, DatabaseEnv, HasName, HasTableType};
use reth_engine_tree::tree::TreeConfig;
use reth_ethereum_engine_primitives::EthereumEngineValidator;
use reth_node_api::{FullNodeTypesAdapter, NodeTypesWithDBAdapter};
use reth_node_builder::{
    components::Components,
    engine_tree_config::{DEFAULT_MEMORY_BLOCK_BUFFER_TARGET, DEFAULT_PERSISTENCE_THRESHOLD},
    FullNode, NodeBuilder, NodeConfig, WithLaunchContext,
};
use reth_node_builder::{NodeAdapter, NodeHandle};
use reth_node_ethereum::{node::EthereumAddOns, EthEvmConfig, EthExecutorProvider, EthereumNode};
use reth_provider::providers::BlockchainProvider2;
use reth_tasks::TaskExecutor;
use reth_transaction_pool::{
    blobstore::DiskFileBlobStore, CoinbaseTipOrdering, EthPooledTransaction,
    EthTransactionValidator, Pool, TransactionValidationTaskExecutor,
};
use tracing::info;

use crate::{
    launcher::CustomEngineNodeLauncher,
    precompile::irys_executor::{
        IrysEvmConfig, IrysExecutorBuilder, IrysPayloadBuilder, PrecompileStateProvider,
    },
};

// use crate::node_launcher::CustomNodeLauncher;

#[macro_export]
macro_rules! vec_of_strings {
    ($($x:expr),*) => (vec![$($x.to_string()),*]);
}

// #[global_allocator]
// static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

// reth node with custom IrysExecutor
pub type RethNode = NodeAdapter<
    FullNodeTypesAdapter<
        NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>,
        BlockchainProvider2<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>,
    >,
    Components<
        FullNodeTypesAdapter<
            NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>,
            BlockchainProvider2<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>,
        >,
        Pool<
            TransactionValidationTaskExecutor<
                EthTransactionValidator<
                    BlockchainProvider2<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>,
                    EthPooledTransaction,
                >,
            >,
            CoinbaseTipOrdering<EthPooledTransaction>,
            DiskFileBlobStore,
        >,
        IrysEvmConfig,
        EthExecutorProvider<IrysEvmConfig>,
        Arc<dyn Consensus>,
        EthereumEngineValidator,
    >,
>;

// reth node with the standard EVM
pub type RethNodeStandard = NodeAdapter<
    FullNodeTypesAdapter<
        NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>,
        BlockchainProvider2<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>,
    >,
    Components<
        FullNodeTypesAdapter<
            NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>,
            BlockchainProvider2<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>,
        >,
        Pool<
            TransactionValidationTaskExecutor<
                EthTransactionValidator<
                    BlockchainProvider2<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>,
                    EthPooledTransaction,
                >,
            >,
            CoinbaseTipOrdering<EthPooledTransaction>,
            DiskFileBlobStore,
        >,
        EthEvmConfig,
        EthExecutorProvider,
        Arc<dyn Consensus>,
        EthereumEngineValidator,
    >,
>;

pub type RethNodeAddOns = EthereumAddOns;
pub type RethNodeExitHandle = NodeHandle<RethNode, RethNodeAddOns>;

pub type RethNodeHandle = FullNode<RethNode, RethNodeAddOns>;

#[derive(Debug, Clone)]
pub struct RethNodeProvider(pub Arc<RethNodeHandle>);

impl Deref for RethNodeProvider {
    type Target = Arc<RethNodeHandle>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<RethNodeProvider> for RethNodeHandle {
    fn from(val: RethNodeProvider) -> Self {
        val.0.as_ref().clone()
    }
}

pub async fn run_node<T: HasName + HasTableType>(
    chainspec: Arc<ChainSpec>,
    task_executor: TaskExecutor,
    irys_config: Arc<IrysNodeConfig>,
    tables: &[T],
    provider: IrysRethProvider,
    latest_block: u64,
    consensus_engine_shutdown: tokio::sync::mpsc::Receiver<()>,
) -> eyre::Result<RethNodeExitHandle> {
    let mut os_args: Vec<String> = std::env::args().collect();
    let bp = os_args.remove(0);

    let mut args = vec_of_strings![
        "node",
        "-vvvvv",
        "--instance",
        format!("{}", irys_config.instance_number.unwrap_or(1)),
        "--disable-discovery",
        "--http",
        "--http.api",
        // "debug,rpc,reth,eth,trace",
        "eth",
        "--http.addr",
        "0.0.0.0",
        "--datadir",
        format!("{}", irys_config.reth_data_dir().to_str().unwrap()),
        "--log.file.directory",
        format!("{}", irys_config.reth_log_dir().to_str().unwrap()),
        "--log.file.format",
        "json",
        "--log.stdout.format",
        "terminal",
        "--log.stdout.filter",
        "debug",
        "--log.file.filter",
        "trace",
        "--http.corsdomain",
        "*" // TODO @JesseTheRobot - make sure this lines up with the path dev_genesis.json is written to
            // "--chain",
            // ".reth/dev_genesis.json"
    ];

    args.insert(0, bp.to_string());
    info!("discarding os args: {:?}", os_args);
    // args.append(&mut os_args);
    // // dbg!(&args);
    info!("Running with args: {:#?}", &args);

    let cli = Cli::<EthereumChainSpecParser, EngineArgs>::parse_from(args.clone());
    let _guard = cli.logs.init_tracing()?;

    // loop {

    // this loop allows us to 'reload' the reth node with a new config very quickly, without having to actually restart the entire process
    // mainly used to provide and then restart with a new genesis block.
    // TODO: extract run_command_until_exit to re-use async context to prevent global thread pool error on reload

    // let exit_reason = runner.run_command_until_exit(|ctx| {
    let ctx = CliContext { task_executor };

    let matched_cmd = match cli.command {
        Commands::Node(command) => Some(command),
        _ => None,
    };

    let node_command = *matched_cmd.expect("unable to get cmd_cfg");

    tracing::info!(target: "reth::cli", version = ?version::SHORT_VERSION, "Starting reth");

    let NodeCommand {
        datadir,
        config,
        // chain,
        metrics,
        instance,
        with_unused_ports,
        network,
        rpc,
        txpool,
        builder,
        debug,
        db,
        dev,
        pruning,
        ext: _engine_args,
        ..
    } = node_command;

    // let chain_spec =
    // get_chain_spec_with_path(vec![], &datadir.datadir.unwrap_or_default().as_ref(), dev.dev);

    // set up node config
    let mut node_config = NodeConfig {
        datadir,
        config,
        // chain,
        chain: chainspec,
        // chain: Arc::new(chain_spec),
        metrics,
        instance,
        network,
        rpc,
        txpool,
        builder,
        debug,
        db,
        dev,
        pruning,
    };

    // Register the prometheus recorder before creating the database,
    // because irys_database init needs it to register metrics.
    let _ = install_prometheus_recorder();

    let data_dir = node_config.datadir();
    let abs_data_dir = canonicalize(data_dir.data_dir())?;
    tracing::info!(target: "reth::cli", path = ?abs_data_dir, "Absolute data dir:");
    let db_path = data_dir.db();

    tracing::info!(target: "reth::cli", path = ?db_path, "Opening database");
    let database =
        Arc::new(init_db(db_path.clone(), db.database_args())?.with_metrics_and_tables(tables));

    let irys_provider = provider;

    if with_unused_ports {
        node_config = node_config.with_unused_ports();
    }

    let builder = NodeBuilder::new(node_config)
        .with_database(database)
        .with_launch_context(ctx.task_executor);

    // launcher(builder, ext).await?;

    // run_custom_node(ctx, cli.clone(), |builder, engine_args| async move {
    // from ext/reth/bin/reth/src/main.rs

    let engine_tree_config = TreeConfig::default()
        .with_persistence_threshold(0 /* engine_args.persistence_threshold */) // always persist to disk
        .with_memory_block_buffer_target(0 /* engine_args.memory_block_buffer_target */);

    let handle =
        builder
            .with_types_and_provider::<EthereumNode, BlockchainProvider2<
                NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>,
            >>()
            .with_components(
                EthereumNode::components()
                .executor(IrysExecutorBuilder{ precompile_state_provider: PrecompileStateProvider { provider: irys_provider.clone()}})
                .payload(IrysPayloadBuilder::default())
            )
            // .with_components(EthereumNode::components())
            .with_add_ons(EthereumAddOns::default())
            // .extend_rpc_modules(move |ctx| {
            //     let provider = ctx.provider().clone();
            //     let irys_ext = ctx.node().components.irys_ext.clone();
            //     let network = ctx.network().clone();
            //     let ext = AccountStateExt { provider, irys_ext, network };
            //     let rpc = ext.into_rpc();
            //     ctx.modules.merge_configured(rpc)?;
            //     Ok(())
            // })
            .launch_with_fn(|builder| {
                let launcher = CustomEngineNodeLauncher::new(
                    builder.task_executor().clone(),
                    builder.config().datadir(),
                    engine_tree_config,
                    irys_provider,
                    latest_block,
                    consensus_engine_shutdown
                );
                builder.launch_with(launcher)
            })
            .await?;

    Ok(handle)
    // let exit_reason = handle.node_exit_future.await?;

    // if true
    // /* launched.node.config.dev.dev */
    // {
    //     match exit_reason.clone() {
    //         NodeExitReason::Normal => (),
    //         NodeExitReason::Reload(payload) => match payload {
    //             ReloadPayload::ReloadConfig(chain_spec) => {
    //                 // delay here so the genesis submission RPC response is able to make it back before the server dies
    //                 let ser = serde_json::to_string_pretty(&chain_spec.genesis)?;
    //                 let pb =
    //                     PathBuf::from(handle.node.data_dir.data_dir().join("dev_genesis.json"));
    //                 // remove_file(&pb)?;
    //                 let mut f = File::create(&pb)?;
    //                 f.write_all(ser.as_bytes())?;
    //                 info!("Written dev_genesis.json");
    //                 sleep(Duration::from_millis(500)).await;
    //             }
    //         },
    //     }
    // }

    // Ok(NodeExitReason::Normal)

    // })
    //     })?;
    // }
    // Ok(())
}

pub async fn run_custom_node<Ext, C, L, Fut>(
    ctx: CliContext,
    cli: Cli<C, Ext>,
    launcher: L,
) -> eyre::Result<NodeExitReason>
where
    C: ChainSpecParser<ChainSpec: EthChainSpec + EthereumHardforks + EthChainSpec>,
    Ext: clap::Args + Clone + fmt::Debug,
    L: Fn(WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, C::ChainSpec>>, Ext) -> Fut,
    Fut: Future<Output = eyre::Result<NodeExitReason>>,
{
    // from reth/bin/reth/src/commands/node/mod.rs:137
    let matched_cmd = match cli.command {
        Commands::Node(command) => Some(command),
        _ => None,
    };

    let node_command = *matched_cmd.expect("unable to get cmd_cfg");

    tracing::info!(target: "reth::cli", version = ?version::SHORT_VERSION, "Starting reth");

    let NodeCommand {
        datadir,
        config,
        chain,
        metrics,
        instance,
        with_unused_ports,
        network,
        rpc,
        txpool,
        builder,
        debug,
        db,
        dev,
        pruning,
        ext,
    } = node_command;

    // let chain_spec =
    // get_chain_spec_with_path(vec![], &datadir.datadir.unwrap_or_default().as_ref(), dev.dev);

    // set up node config
    let mut node_config = NodeConfig {
        datadir,
        config,
        chain,
        // chain: Arc::new(chain_spec),
        metrics,
        instance,
        network,
        rpc,
        txpool,
        builder,
        debug,
        db,
        dev,
        pruning,
    };

    // Register the prometheus recorder before creating the database,
    // because irys_database init needs it to register metrics.
    let _ = install_prometheus_recorder();

    let data_dir = node_config.datadir();
    let db_path = data_dir.db();

    tracing::info!(target: "reth::cli", path = ?db_path, "Opening database");
    let database = Arc::new(init_db(db_path.clone(), db.database_args())?.with_metrics());

    if with_unused_ports {
        node_config = node_config.with_unused_ports();
    }

    let builder = NodeBuilder::new(node_config)
        .with_database(database)
        .with_launch_context(ctx.task_executor);

    launcher(builder, ext).await?;
    Ok(NodeExitReason::Normal)
}

/// Parameters for configuring the engine
#[derive(Debug, Clone, Args, PartialEq, Eq)]
#[command(next_help_heading = "Engine")]
pub struct EngineArgs {
    /// Enable the experimental engine features on reth binary
    ///
    /// DEPRECATED: experimental engine is default now, use --engine.legacy to enable the legacy
    /// functionality
    #[arg(long = "engine.experimental", default_value = "false")]
    pub experimental: bool,

    /// Enable the legacy engine on reth binary
    #[arg(long = "engine.legacy", default_value = "false")]
    pub legacy: bool,

    /// Configure persistence threshold for engine experimental.
    #[arg(long = "engine.persistence-threshold", requires = "experimental", default_value_t = DEFAULT_PERSISTENCE_THRESHOLD)]
    pub persistence_threshold: u64,

    /// Configure the target number of blocks to keep in memory.
    #[arg(long = "engine.memory-block-buffer-target", requires = "experimental", default_value_t = DEFAULT_MEMORY_BLOCK_BUFFER_TARGET)]
    pub memory_block_buffer_target: u64,
}

impl Default for EngineArgs {
    fn default() -> Self {
        Self {
            experimental: false,
            legacy: false,
            persistence_threshold: DEFAULT_PERSISTENCE_THRESHOLD,
            memory_block_buffer_target: DEFAULT_MEMORY_BLOCK_BUFFER_TARGET,
        }
    }
}
