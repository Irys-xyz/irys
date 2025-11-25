use alloy_eips::BlockId;
use alloy_primitives::U256;
use std::collections::HashMap;
use async_trait::async_trait;

use irys_types::Address;
use rayon::iter::{IntoParallelIterator as _, ParallelIterator as _};
use reth_chainspec::EthereumHardforks;
use reth_e2e_test_utils::rpc::RpcTestContext;
use reth_node_api::{BlockTy, FullNodeComponents, NodeTypes};
use reth_provider::{BlockReader, StateProviderBox};
use reth_rpc_eth_api::helpers::{EthApiSpec, EthTransactions, LoadState, SpawnBlocking, TraceExt};
use tracing::warn;

pub trait IrysRethLoadStateExt: LoadState {
    /// Get the account balance.
    async fn balance(&self, address: Address, block_id: Option<BlockId>) -> Result<U256, Self::Error>;
}

impl<T> IrysRethLoadStateExt for T
where
    T: LoadState + SpawnBlocking,
{
    async fn balance(&self, address: Address, block_id: Option<BlockId>) -> Result<U256, Self::Error> {
        Ok(self
            .state_at_block_id_or_latest(block_id).await?
            .account_balance(&address)
            .unwrap_or_default()
            .unwrap_or(U256::ZERO))
    }
}

#[async_trait]
pub trait IrysRethRpcTestContextExt<Node, EthApi>
where
    Node: FullNodeComponents<Types: NodeTypes<ChainSpec: EthereumHardforks>>,
    EthApi: EthApiSpec<Provider: BlockReader<Block = BlockTy<Node::Types>>>
        + EthTransactions
        + TraceExt,
{
    async fn get_balance(&self, address: Address, block_id: Option<BlockId>) -> eyre::Result<U256>;

    async fn get_balance_irys(&self, address: Address, block_id: Option<BlockId>) -> irys_types::U256;

    async fn get_balances_irys(
        &self,
        addresses: &[Address],
        block_id: Option<BlockId>,
    ) -> HashMap<Address, irys_types::U256>;

    async fn get_balance_irys_canonical_and_pending(
        &self,
        address: Address,
        block_id: Option<BlockId>,
    ) -> eyre::Result<irys_types::U256>;
}

#[async_trait]
impl<Node, EthApi> IrysRethRpcTestContextExt<Node, EthApi> for RpcTestContext<Node, EthApi>
where
    Node: FullNodeComponents<Types: NodeTypes<ChainSpec: EthereumHardforks>>,
    EthApi: EthApiSpec<Provider: BlockReader<Block = BlockTy<Node::Types>>>
        + EthTransactions
        + TraceExt,
{
    async fn get_balance(&self, address: Address, block_id: Option<BlockId>) -> eyre::Result<U256> {
        let eth_api = self.inner.eth_api();
        Ok(eth_api.balance(address, block_id).await?)
    }

    /// Modified version of the above `get_balance` impl,
    /// which will return an Irys U256, and will return a value of `0` if getting the balance fails
    async fn get_balance_irys(&self, address: Address, block_id: Option<BlockId>) -> irys_types::U256 {
        let eth_api = self.inner.eth_api();
        eth_api
            .balance(address, block_id)
            .await
            .map(std::convert::Into::into)
            .inspect_err(|e| {
                warn!(
                    "Error getting balance for {}@{:?} - {:?}",
                    &address, &block_id, &e
                )
            })
            .unwrap_or(irys_types::U256::zero())
    }

    async fn get_balances_irys(
        &self,
        addresses: &[Address],
        block_id: Option<BlockId>,
    ) -> HashMap<Address, irys_types::U256> {
        let mut results = HashMap::new();
        for address in addresses {
            results.insert(*address, self.get_balance_irys(*address, block_id).await);
        }
        results
    }

    /// checks all known blocks (pending & canonical) for the provided hash and returns the account's balance using an Irys U256.
    async fn get_balance_irys_canonical_and_pending(
        &self,
        address: Address,
        block_id: Option<BlockId>,
    ) -> eyre::Result<irys_types::U256> {
        use eyre::OptionExt as _;
        use reth_provider::StateProviderFactory as _;
        let provider = self.inner.provider();
        let state_provider: StateProviderBox = {
            let block_id =
                block_id.unwrap_or(BlockId::Number(alloy_eips::BlockNumberOrTag::Latest));
            match block_id {
                BlockId::Hash(rpc_block_hash) => {
                    provider.state_by_block_hash(rpc_block_hash.block_hash)?
                }
                BlockId::Number(block_number_or_tag) => match block_number_or_tag {
                    alloy_eips::BlockNumberOrTag::Latest => provider.latest()?,

                    _ => unimplemented!(),
                },
            }
        };
        Ok(state_provider
            .account_balance(&address)?
            .ok_or_eyre("Unable to get account balance from state")?
            .into())
    }
}
