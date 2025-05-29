use alloy_eips::BlockNumberOrTag;
use alloy_rpc_types_engine::PayloadAttributes;
use irys_database::db::RethDbWrapper;
use irys_reth::{
    evm::IrysEvmConfig, IrysEthereumNode, IrysSystemTxValidator, SystemTxPriorityOrdering,
};
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
use reth_db::init_db;
use reth_node_builder::{
    FullNode, FullNodeTypesAdapter, NodeAdapter, NodeBuilder, NodeConfig, NodeHandle,
    NodeTypesWithDBAdapter,
};
use reth_provider::providers::BlockchainProvider;
use reth_rpc_eth_api::EthApiServer as _;
use std::{collections::HashSet, fmt::Formatter, sync::Arc};
use std::{fmt::Debug, ops::Deref};
use tracing::error;

use crate::{new_reth_context, unwind::unwind_to};
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
                IrysSystemTxValidator<
                    BlockchainProvider<NodeTypesWithDBAdapter<IrysEthereumNode, RethDbWrapper>>,
                    EthPooledTransaction,
                >,
            >,
            SystemTxPriorityOrdering<EthPooledTransaction>,
            DiskFileBlobStore,
        >,
        IrysEvmConfig,
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
    _provider: IrysRethProvider,
    latest_block: u64,
    random_ports: bool,
) -> eyre::Result<RethNodeHandle> {
    let mut reth_config = NodeConfig::new(chainspec.clone());

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
    reth_config.engine.persistence_threshold = 0;
    reth_config.engine.memory_block_buffer_target = 0;

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
        .with_database(database.clone())
        .with_launch_context(task_executor.clone());

    let handle = builder
        .node(IrysEthereumNode {
            allowed_system_tx_origin: node_config.miner_address(),
        })
        .launch_with_debug_capabilities()
        .await?;

    let context = new_reth_context(handle.node.clone()).await?;

    // check that the latest height lines up with the expected latest height from irys

    let latest = context
        .rpc
        .inner
        .eth_api()
        .block_by_number(BlockNumberOrTag::Latest, false)
        .await?
        .expect("latest block should be Some");

    if latest.header.number > latest_block {
        error!("\x1b[1;31m!!! REMOVING OUT OF SYNC BLOCK(s) !!! Reth head is {}, Irys head is {}\x1b[0m", &latest.header.number, &latest_block);
        database.close(); // important! otherwise we get MDBX error 11
        drop(context);
        drop(handle);

        unwind_to(node_config, chainspec.clone(), latest_block).await?;
        return Err(eyre::eyre!("Unwound blocks"));
    };

    Ok(handle)
}
