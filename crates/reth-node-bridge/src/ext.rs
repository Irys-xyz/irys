use std::time::Instant;

use crate::node::{RethNodeAdapter, RethNodeAddOns};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{BlockNumber, B256, U256};
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes};
use irys_reth::{
    payload::{DeterministicSystemTxKey, SystemTxRequest, SystemTxStore},
    IrysEthereumNode,
};
use irys_types::Address;
use reth::transaction_pool::EthPooledTransaction;
use reth_chainspec::EthereumHardforks;
use reth_e2e_test_utils::{
    node::NodeTestContext, payload::PayloadTestContext, rpc::RpcTestContext,
};
use reth_node_api::{
    BlockTy, EngineApiMessageVersion, FullNodeComponents, NodeTypes, PayloadKind, PayloadTypes,
};
use reth_payload_builder::{EthPayloadBuilderAttributes, PayloadId};
use reth_provider::{BlockReader, BlockReaderIdExt as _};
use reth_rpc_eth_api::helpers::{EthApiSpec, EthTransactions, LoadState, TraceExt};

pub trait IrysRethLoadStateExt: LoadState {
    /// Get the account balance.
    fn balance(&self, address: Address, block_id: Option<BlockId>) -> Result<U256, Self::Error>;
}

impl<T> IrysRethLoadStateExt for T
where
    T: LoadState,
{
    fn balance(&self, address: Address, block_id: Option<BlockId>) -> Result<U256, Self::Error> {
        Ok(self
            .state_at_block_id_or_latest(block_id)?
            .account_balance(&address)
            .unwrap_or_default()
            .unwrap_or(U256::ZERO))
    }
}

#[async_trait::async_trait(?Send)]
pub trait IrysRethRpcTestContextExt<Node, EthApi>
where
    Node: FullNodeComponents<Types: NodeTypes<ChainSpec: EthereumHardforks>>,
    EthApi: EthApiSpec<Provider: BlockReader<Block = BlockTy<Node::Types>>>
        + EthTransactions
        + TraceExt,
{
    async fn get_balance(&self, address: Address, block_id: Option<BlockId>) -> eyre::Result<U256>;
}

#[async_trait::async_trait(?Send)]
impl<Node, EthApi> IrysRethRpcTestContextExt<Node, EthApi> for RpcTestContext<Node, EthApi>
where
    Node: FullNodeComponents<Types: NodeTypes<ChainSpec: EthereumHardforks>>,
    EthApi: EthApiSpec<Provider: BlockReader<Block = BlockTy<Node::Types>>>
        + EthTransactions
        + TraceExt,
{
    async fn get_balance(&self, address: Address, block_id: Option<BlockId>) -> eyre::Result<U256> {
        let eth_api = self.inner.eth_api();
        Ok(eth_api.balance(address, block_id)?)
    }
}
