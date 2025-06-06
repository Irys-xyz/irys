use alloy_eips::BlockId;
use alloy_primitives::U256;
use irys_types::Address;
use reth_chainspec::EthereumHardforks;
use reth_e2e_test_utils::rpc::RpcTestContext;
use reth_node_api::{BlockTy, FullNodeComponents, NodeTypes};
use reth_provider::BlockReader;
use reth_rpc_eth_api::helpers::{EthApiSpec, EthTransactions, LoadState, TraceExt};
use tracing::warn;

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

pub trait IrysRethRpcTestContextExt<Node, EthApi>
where
    Node: FullNodeComponents<Types: NodeTypes<ChainSpec: EthereumHardforks>>,
    EthApi: EthApiSpec<Provider: BlockReader<Block = BlockTy<Node::Types>>>
        + EthTransactions
        + TraceExt,
{
    fn get_balance(&self, address: Address, block_id: Option<BlockId>) -> eyre::Result<U256>;

    fn get_balance_irys(&self, address: Address, block_id: Option<BlockId>) -> irys_types::U256;
}

impl<Node, EthApi> IrysRethRpcTestContextExt<Node, EthApi> for RpcTestContext<Node, EthApi>
where
    Node: FullNodeComponents<Types: NodeTypes<ChainSpec: EthereumHardforks>>,
    EthApi: EthApiSpec<Provider: BlockReader<Block = BlockTy<Node::Types>>>
        + EthTransactions
        + TraceExt,
{
    fn get_balance(&self, address: Address, block_id: Option<BlockId>) -> eyre::Result<U256> {
        let eth_api = self.inner.eth_api();
        Ok(eth_api.balance(address, block_id)?)
    }

    /// Modified version of the above `get_balance` impl,
    /// which will return an Irys U256, and will return a value of `0` if getting the balance fails
    fn get_balance_irys(&self, address: Address, block_id: Option<BlockId>) -> irys_types::U256 {
        let eth_api = self.inner.eth_api();
        eth_api
            .balance(address, block_id)
            .map(|v| v.into())
            .inspect_err(|e| {
                warn!(
                    "Error getting balance for {}@{:?} - {:?}",
                    &address.0.to_base58(),
                    &block_id,
                    &e
                )
            })
            .unwrap_or(irys_types::U256::zero())
    }
}
