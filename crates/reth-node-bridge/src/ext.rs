use crate::node::{RethNodeAdapter, RethNodeAddOns};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{BlockNumber, B256, U256};
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes};
use base58::ToBase58;
use irys_reth::IrysEthereumNode;
use irys_types::Address;
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
use tracing::warn;

#[async_trait::async_trait(?Send)]
pub trait IrysRethTestContextExt {
    async fn assert_new_block_irys(
        &self,
        block_hash: B256,
        block_number: BlockNumber,
    ) -> eyre::Result<()>;

    async fn advance_block_irys(
        &mut self,
    ) -> eyre::Result<<<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::BuiltPayload>;

    async fn new_payload_irys(
        &mut self,
        parent: B256,
        attributes: <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadAttributes,
    ) -> eyre::Result<<<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::BuiltPayload>;

    async fn update_forkchoice_full(
        &self,
        head_block_hash: B256,
        confirmed_block_hash: Option<B256>,
        finalized_block_hash: Option<B256>,
    ) -> eyre::Result<()>;

    async fn build_submit_payload_irys(
        &mut self,
        parent: B256,
        attributes: <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadAttributes,
    ) -> eyre::Result<<<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::BuiltPayload>;
}

#[async_trait::async_trait(?Send)]
impl IrysRethTestContextExt for NodeTestContext<RethNodeAdapter, RethNodeAddOns> {
    /// Asserts that a new block has been added to the blockchain
    /// and the tx has been included in the block.
    ///
    /// Does NOT work for pipeline since there's no stream notification!
    async fn assert_new_block_irys(
        &self,
        block_hash: B256,
        block_number: BlockNumber,
    ) -> eyre::Result<()> {
        // get head block from notifications stream and verify the tx has been pushed to the
        // pool is actually present in the canonical block
        // let head = self.engine_api.canonical_stream.next().await.unwrap();
        // let tx = head.tip().transactions().next();
        // assert_eq!(tx.unwrap().hash().as_slice(), tip_tx_hash.as_slice());

        loop {
            // wait for the block to commit
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if let Some(latest_block) = self
                .inner
                .provider
                .block_by_number_or_tag(BlockNumberOrTag::Latest)?
            {
                if latest_block.header.number == block_number {
                    // make sure the block hash we submitted via FCU engine api is the new latest
                    // block using an RPC call
                    assert_eq!(latest_block.hash_slow(), block_hash);
                    break;
                }
            }
        }
        Ok(())
    }

    async fn advance_block_irys(
        &mut self,
    ) -> eyre::Result<<<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::BuiltPayload>
    {
        let attrs = self.payload.new_attributes()?;
        let payload = self
            .build_submit_payload_irys(
                attrs.parent,
                PayloadAttributes {
                    timestamp: attrs.timestamp,
                    prev_randao: attrs.prev_randao,
                    suggested_fee_recipient: attrs.suggested_fee_recipient,
                    withdrawals: Some(attrs.withdrawals.to_vec()),
                    parent_beacon_block_root: attrs.parent_beacon_block_root,
                },
            )
            .await?;

        // trigger forkchoice update via engine api to commit the block to the blockchain
        self.update_forkchoice(payload.block().hash(), payload.block().hash())
            .await?;

        Ok(payload)
    }

    async fn new_payload_irys(
        &mut self,
        parent: B256,
        attributes: <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadAttributes,
    ) -> eyre::Result<<<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::BuiltPayload>
    {
        let attributes = EthPayloadBuilderAttributes::new(parent, attributes);
        self.payload
            .build_new_payload_irys(attributes.clone())
            .await?;
        // first event is the payload attributes

        self.payload.expect_attr_event(attributes.clone()).await?;
        // wait for the payload builder to have finished building

        // self.payload
        //     .wait_for_built_payload(attributes.payload_id())
        //     .await;
        self.payload
            .wait_for_built_payload_irys(attributes.payload_id())
            .await?;

        Ok(self.payload.expect_built_payload().await?)
    }

    /// Sends forkchoice update to the engine api
    // we can set safe or finalized to ZERO to skip updating them, but head is mandatory.
    // safe (confirmed) we update in the block confirmed handler
    // finalized we update in the block finalized handler
    async fn update_forkchoice_full(
        &self,
        head_block_hash: B256,
        confirmed_block_hash: Option<B256>,
        finalized_block_hash: Option<B256>,
    ) -> eyre::Result<()> {
        self.inner
            .add_ons_handle
            .beacon_engine_handle
            .fork_choice_updated(
                ForkchoiceState {
                    head_block_hash,
                    safe_block_hash: confirmed_block_hash.unwrap_or(B256::ZERO),
                    finalized_block_hash: finalized_block_hash.unwrap_or(B256::ZERO),
                },
                None,
                EngineApiMessageVersion::default(),
            )
            .await?;

        Ok(())
    }

    async fn build_submit_payload_irys(
        &mut self,
        parent: B256,
        attributes: <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadAttributes,
    ) -> eyre::Result<<<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::BuiltPayload>
    {
        let payload = self.new_payload_irys(parent, attributes).await?;

        self.submit_payload(payload.clone()).await?;

        Ok(payload)
    }
}

#[async_trait::async_trait(?Send)]
pub trait IrysRethPayloadTestContextExt {
    async fn build_new_payload_irys(
        &mut self,
        attributes: <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadBuilderAttributes,
    ) -> eyre::Result<()>;

    async fn wait_for_built_payload_irys(
        &self,
        payload_id: PayloadId,
    ) -> eyre::Result<<<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::BuiltPayload>;

    fn new_attributes(
        &mut self,
    ) -> eyre::Result<
        <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadBuilderAttributes,
    >;
}

#[async_trait::async_trait(?Send)]
impl IrysRethPayloadTestContextExt
    for PayloadTestContext<<IrysEthereumNode as NodeTypes>::Payload>
{
    async fn build_new_payload_irys(
        &mut self,
        attributes: <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadBuilderAttributes,
    ) -> eyre::Result<()> {
        self.payload_builder
            .send_new_payload(attributes.clone())
            .await
            .unwrap()?;
        Ok(())
    }

    async fn wait_for_built_payload_irys(
        &self,
        payload_id: PayloadId,
    ) -> eyre::Result<<<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::BuiltPayload>
    {
        Ok(self
            .payload_builder
            .resolve_kind(payload_id, PayloadKind::WaitForPending)
            .await
            .unwrap()?)
    }

    fn new_attributes(
        &mut self,
    ) -> eyre::Result<
        <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadBuilderAttributes,
    > {
        self.timestamp += 1;
        let attributes = (self.attributes_generator)(self.timestamp);
        Ok(attributes)
    }
}

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
