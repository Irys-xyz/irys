use alloy_rpc_types_engine::PayloadAttributes;
use irys_database::db::RethDbWrapper;
use irys_reth::{IrysEthTransactionValidator, IrysEthereumNode, SystemTxsCoinbaseTipOrdering};
use irys_storage::reth_provider::IrysRethProvider;
use irys_types::Address;
use reth::{
    args::DatabaseArgs,
    consensus::{ConsensusError, FullConsensus},
    network::NetworkHandle,
    payload::EthPayloadBuilderAttributes,
    primitives::EthPrimitives,
    prometheus_exporter::install_prometheus_recorder,
    revm::primitives::B256,
    rpc::builder::{RethRpcModule, RpcModuleSelection},
    tasks::TaskExecutor,
    transaction_pool::{
        blobstore::DiskFileBlobStore, EthPooledTransaction, TransactionValidationTaskExecutor,
    },
};
use reth_chainspec::ChainSpec;
use reth_db::{init_db, DatabaseEnv};
use reth_node_builder::{
    FullNode, FullNodeTypesAdapter, NodeAdapter, NodeBuilder, NodeConfig, NodeHandle,
    NodeTypesWithDBAdapter,
};
use reth_provider::providers::BlockchainProvider;
use std::{collections::HashSet, fmt::Formatter, sync::Arc};
use std::{fmt::Debug, ops::Deref};

pub use reth_e2e_test_utils::node::NodeTestContext;

pub type RethNodeHandle = NodeHandle<RethNodeAdapter, RethNodeAddOns>;

pub type RethNodeAdapter = NodeAdapter<
    FullNodeTypesAdapter<
        IrysEthereumNode,
        RethDbWrapper,
        BlockchainProvider<NodeTypesWithDBAdapter<IrysEthereumNode, RethDbWrapper>>,
    >,
    reth_node_builder::components::Components<
        FullNodeTypesAdapter<
            IrysEthereumNode,
            RethDbWrapper,
            BlockchainProvider<NodeTypesWithDBAdapter<IrysEthereumNode, RethDbWrapper>>,
        >,
        NetworkHandle,
        reth::transaction_pool::Pool<
            TransactionValidationTaskExecutor<
                IrysEthTransactionValidator<
                    BlockchainProvider<NodeTypesWithDBAdapter<IrysEthereumNode, RethDbWrapper>>,
                    EthPooledTransaction,
                >,
            >,
            SystemTxsCoinbaseTipOrdering<EthPooledTransaction>,
            DiskFileBlobStore,
        >,
        irys_reth::evm::CustomEvmConfig,
        Arc<(dyn FullConsensus<EthPrimitives, Error = ConsensusError> + 'static)>,
    >,
>;

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
    // reth_config: NodeConfig<<IrysEthereumNode as NodeTypes>::ChainSpec>,
    provider: IrysRethProvider,
    latest_block: u64,
    random_ports: bool,
) -> eyre::Result<RethNodeHandle> {
    // let logs = LogArgs::default()
    let mut reth_config = NodeConfig::new(chainspec);

    reth_config.network.discovery.disable_discovery = true;
    reth_config.rpc.http = true;
    reth_config.rpc.http_api = Some(RpcModuleSelection::Selection(HashSet::from([
        RethRpcModule::Eth,
    ])));
    reth_config.rpc.http_addr = node_config.reth_peer_info.peering_tcp_addr.ip();
    reth_config.network.discovery.addr = node_config.reth_peer_info.peering_tcp_addr.ip();
    reth_config.network.discovery.port = node_config.reth_peer_info.peering_tcp_addr.port();
    reth_config.datadir.datadir = node_config.reth_data_dir().into();
    reth_config.rpc.http_corsdomain = Some("*".to_string());

    // if let Some(chain_spec) = self.command.chain_spec() {
    //     self.logs.log_file_directory =
    //         self.logs.log_file_directory.join(chain_spec.chain.to_string());
    // }
    // let _guard = logs.init_tracing()?;
    // info!(target: "reth::cli", "Initialized tracing, debug log directory: {}", logs.log_file_directory);
    let db_args = DatabaseArgs::default();
    // Install the prometheus recorder to be sure to record all metrics
    let _ = install_prometheus_recorder();

    let data_dir = reth_config.datadir();
    let db_path = data_dir.db();

    tracing::info!(target: "reth::cli", path = ?db_path, "Opening database");
    let database =
        RethDbWrapper::new(init_db(db_path.clone(), db_args.database_args())?.with_metrics());

    if random_ports {
        reth_config = reth_config.with_unused_ports();
    }

    let builder = NodeBuilder::new(reth_config)
        .with_database(database)
        .with_launch_context(task_executor);

    // LAUNCHER

    //let handle =

    let handle = builder
        .node(IrysEthereumNode {
            allowed_system_tx_origin: Address::random(),
        })
        .launch_with_debug_capabilities()
        .await?;

    Ok(handle)

    // let NodeHandle {
    //     node,
    //     node_exit_future,
    // } = handle;

    // Ok(node)
}
