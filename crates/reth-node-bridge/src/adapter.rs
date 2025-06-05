use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
    time::Instant,
};

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

use crate::node::{eth_payload_attributes, RethNode};

#[derive(Clone)]
pub struct IrysRethNodeAdapter {
    pub reth_node: Arc<NodeTestContext<RethNodeAdapter, RethNodeAddOns>>,
    pub system_tx_store: SystemTxStore,
}

impl std::fmt::Debug for IrysRethNodeAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IrysRethNodeAdapter")
    }
}

impl IrysRethNodeAdapter {
    pub async fn new(node: RethNode, system_tx_store: SystemTxStore) -> eyre::Result<Self> {
        let reth_node = NodeTestContext::new(node, eth_payload_attributes).await?;
        Ok(Self {
            reth_node: Arc::new(reth_node),
            system_tx_store,
        })
    }
}

impl Deref for IrysRethNodeAdapter {
    type Target = NodeTestContext<RethNodeAdapter, RethNodeAddOns>;
    fn deref(&self) -> &Self::Target {
        &self.reth_node
    }
}

impl IrysRethNodeAdapter {
    /// Asserts that a new block has been added to the blockchain
    /// and the tx has been included in the block.
    ///
    /// Does NOT work for pipeline since there's no stream notification!
    pub async fn assert_new_block_irys(
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
                .reth_node
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

    pub async fn advance_block_irys(
        &self,
        parent_block_hash: B256,
        payload_attrs: <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadAttributes,
        system_txs: Vec<EthPooledTransaction>,
    ) -> eyre::Result<<<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::BuiltPayload>
    {
        let payload = self
            .build_submit_payload_irys(parent_block_hash, payload_attrs, system_txs)
            .await?;

        // trigger forkchoice update via engine api to commit the block to the blockchain
        self.update_forkchoice_full(
            payload.block().hash(),
            Some(payload.block().hash()),
            Some(payload.block().hash()),
        )
        .await?;

        Ok(payload)
    }

    pub async fn new_payload_irys(
        &self,
        parent: B256,
        attributes: <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadAttributes,
        system_txs: Vec<EthPooledTransaction>,
    ) -> eyre::Result<<<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::BuiltPayload>
    {
        let attributes = EthPayloadBuilderAttributes::new(parent, attributes);
        let key = DeterministicSystemTxKey::new(attributes.payload_id());
        self.system_tx_store.set_system_txs(key, system_txs);
        let payload_id = self
            .reth_node
            .payload
            .payload_builder
            .send_new_payload(attributes.clone())
            .await??;

        let payload = self
            .reth_node
            .payload
            .payload_builder
            .best_payload(payload_id)
            .await
            .unwrap()?;
        Ok(payload)
    }

    /// Sends forkchoice update to the engine api
    // we can set safe or finalized to ZERO to skip updating them, but head is mandatory.
    // safe (confirmed) we update in the block confirmed handler
    // finalized we update in the block finalized handler
    pub async fn update_forkchoice_full(
        &self,
        head_block_hash: B256,
        confirmed_block_hash: Option<B256>,
        finalized_block_hash: Option<B256>,
    ) -> eyre::Result<()> {
        self.reth_node
            .inner
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

    pub async fn build_submit_payload_irys(
        &self,
        parent: B256,
        attributes: <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadAttributes,
        system_txs: Vec<EthPooledTransaction>,
    ) -> eyre::Result<<<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::BuiltPayload>
    {
        let payload = self
            .new_payload_irys(parent, attributes, system_txs)
            .await?;
        let _block_hash = self.reth_node.submit_payload(payload.clone()).await?;
        Ok(payload)
    }
}
