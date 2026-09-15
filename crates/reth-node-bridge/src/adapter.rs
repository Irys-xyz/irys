use std::{
    collections::HashMap,
    ops::Deref,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::node::{RethNode, eth_payload_attributes};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, B256, BlockNumber, Bytes};
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes, PayloadStatusEnum};
use irys_reth::{IrysEthereumNode, IrysPayloadAttributes, IrysPayloadBuilderAttributes};
use irys_types::IrysAddress;
use reth::transaction_pool::EthPooledTransaction;
use reth_ethereum_primitives::Block;
use reth_node_api::{
    EngineApiMessageVersion, NodeTypes, PayloadBuilderAttributes as _, PayloadTypes,
};
use reth_payload_builder::PayloadKind;
use reth_provider::{
    BlockReader as _, BlockReaderIdExt as _, BlockSource, StateProviderFactory as _,
};
use reth_rpc_eth_api::EthApiServer as _;
use reth_rpc_eth_api::helpers::EthTransactions;
use reth_storage_api::StateProvider as _;
use tracing::warn;

/// Production handle to a launched Reth node.
///
/// Wraps [`RethNode`] (`FullNode`) directly. The previous implementation
/// wrapped `reth_e2e_test_utils::NodeTestContext`, which pulled the entire
/// e2e harness into every node, actor, and domain build.
#[derive(Clone)]
pub struct IrysRethNodeAdapter {
    pub reth_node: Arc<RethNode>,
}

impl std::fmt::Debug for IrysRethNodeAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IrysRethNodeAdapter")
    }
}

impl IrysRethNodeAdapter {
    pub fn new(node: RethNode) -> Self {
        Self {
            reth_node: Arc::new(node),
        }
    }
}

impl Deref for IrysRethNodeAdapter {
    type Target = RethNode;
    fn deref(&self) -> &Self::Target {
        &self.reth_node
    }
}

impl IrysRethNodeAdapter {
    pub fn evm_block(&self, evm_block_hash: B256) -> Option<Block> {
        self.provider
            .find_block_by_hash(evm_block_hash, BlockSource::Any)
            .inspect_err(|err| tracing::error!(custom.error = ?err))
            .ok()
            .flatten()
    }

    pub async fn get_balance(
        &self,
        address: IrysAddress,
        block_id: Option<BlockId>,
    ) -> eyre::Result<alloy_primitives::U256> {
        Ok(self.eth_api().balance(address.into(), block_id).await?)
    }

    /// Returns Irys `U256`, or zero if the RPC call fails.
    pub async fn get_balance_irys(
        &self,
        address: IrysAddress,
        block_id: Option<BlockId>,
    ) -> irys_types::U256 {
        self.eth_api()
            .balance(address.into(), block_id)
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

    pub async fn get_balances_irys(
        &self,
        addresses: &[IrysAddress],
        block_id: Option<BlockId>,
    ) -> HashMap<IrysAddress, irys_types::U256> {
        let mut results = HashMap::new();
        for address in addresses {
            results.insert(*address, self.get_balance_irys(*address, block_id).await);
        }
        results
    }

    /// Balance from pending & canonical state. `block_id` of `None` / `Latest`
    /// reads the canonical tip; a hash reads that block's state.
    ///
    /// A missing account is balance zero (`StateProvider::account_balance` returns
    /// `None` when there is no account record). Provider errors still fail.
    pub fn get_balance_irys_canonical_and_pending(
        &self,
        address: IrysAddress,
        block_id: Option<BlockId>,
    ) -> eyre::Result<irys_types::U256> {
        let state_provider = {
            let block_id = block_id.unwrap_or(BlockId::Number(BlockNumberOrTag::Latest));
            match block_id {
                BlockId::Hash(rpc_block_hash) => self
                    .provider
                    .state_by_block_hash(rpc_block_hash.block_hash)?,
                BlockId::Number(block_number_or_tag) => match block_number_or_tag {
                    BlockNumberOrTag::Latest => self.provider.latest()?,
                    other => eyre::bail!("unsupported BlockNumberOrTag variant: {other:?}"),
                },
            }
        };
        Ok(state_provider
            .account_balance(&address.into())?
            .unwrap_or_default()
            .into())
    }

    pub async fn inject_tx(&self, raw_tx: Bytes) -> eyre::Result<B256> {
        EthTransactions::send_raw_transaction(self.eth_api(), raw_tx)
            .await
            .map_err(|e| eyre::eyre!("{e:?}"))
    }

    /// Asserts that a new block has been added to the blockchain
    /// and the tx has been included in the block.
    ///
    /// Does NOT work for pipeline since there's no stream notification!
    pub async fn assert_new_block_irys(
        &self,
        block_hash: B256,
        block_number: BlockNumber,
    ) -> eyre::Result<()> {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if let Some(latest_block) = self
                .provider
                .block_by_number_or_tag(BlockNumberOrTag::Latest)?
                && latest_block.header.number == block_number
            {
                assert_eq!(latest_block.hash_slow(), block_hash);
                break;
            }
        }
        Ok(())
    }

    /// this should be used for testing only, as it doesn't use the payload builder
    /// and instead uses the attributes generator directly.
    /// Also, it doesn't use the shadow txs.
    /// Also, it doesn't set a proper parent beacon block root.
    pub async fn advance_block_testing(
        &mut self,
    ) -> eyre::Result<<<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::BuiltPayload>
    {
        let current_timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let attributes = eth_payload_attributes(current_timestamp.as_secs());
        let attributes = IrysPayloadAttributes {
            inner: PayloadAttributes {
                timestamp: attributes.inner.timestamp,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: Address::ZERO,
                withdrawals: None,
                parent_beacon_block_root: Some(B256::ZERO),
            },
            shadow_txs: vec![],
        };
        let payload = self
            .build_submit_payload_irys(B256::ZERO, attributes, vec![])
            .await?;

        self.update_forkchoice_full(
            payload.block().hash(),
            Some(payload.block().hash()),
            Some(payload.block().hash()),
        )
        .await?;

        Ok(payload)
    }

    pub async fn advance_block_custom(
        &self,
        parent_block_hash: B256,
        payload_attrs: <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadAttributes,
        shadow_txs: Vec<EthPooledTransaction>,
    ) -> eyre::Result<<<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::BuiltPayload>
    {
        let payload = self
            .build_submit_payload_irys(parent_block_hash, payload_attrs, shadow_txs)
            .await?;

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
        shadow_txs: Vec<EthPooledTransaction>,
    ) -> eyre::Result<<<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::BuiltPayload>
    {
        let rpc_attributes = IrysPayloadAttributes {
            inner: attributes.inner,
            shadow_txs,
        };

        let builder_attributes = IrysPayloadBuilderAttributes::try_new(
            parent,
            rpc_attributes,
            0, // version
        )
        .expect("IrysPayloadBuilderAttributes::try_new is infallible");

        let payload_id = self
            .payload_builder_handle
            .send_new_payload(builder_attributes)
            .await??;

        let payload = self
            .payload_builder_handle
            .resolve_kind(payload_id, PayloadKind::WaitForPending)
            .await
            .unwrap()?;
        Ok(payload)
    }

    /// Test-harness helper: `current_head` is written as both safe and finalized.
    ///
    /// `SYNCING` / `ACCEPTED` are success here. Gossip tests FCU a peer to a
    /// block it may not have imported yet; the engine then fetches it. Callers
    /// that need the block canonical wait afterwards (`assert_new_block_irys`,
    /// `wait_for_reth_marker`). Production CL updates use
    /// [`Self::update_forkchoice_full`], which still requires `VALID`.
    pub async fn update_forkchoice(&self, current_head: B256, new_head: B256) -> eyre::Result<()> {
        let res = self
            .add_ons_handle
            .beacon_engine_handle
            .fork_choice_updated(
                ForkchoiceState {
                    head_block_hash: new_head,
                    safe_block_hash: current_head,
                    finalized_block_hash: current_head,
                },
                None,
                EngineApiMessageVersion::default(),
            )
            .await?;

        match res.payload_status.status {
            PayloadStatusEnum::Valid | PayloadStatusEnum::Syncing | PayloadStatusEnum::Accepted => {
                Ok(())
            }
            other => eyre::bail!("Reth has gone out of sync: {other:?}"),
        }
    }

    /// Sends forkchoice update to the engine api
    // we can set safe or finalized to ZERO to skip updating them, but head is mandatory.
    // safe (confirmed) we update in the block confirmed handler
    // finalized we update in the block finalized handler
    #[tracing::instrument(level = "trace", skip_all, err)]
    pub async fn update_forkchoice_full(
        &self,
        head_block_hash: B256,
        confirmed_block_hash: Option<B256>,
        finalized_block_hash: Option<B256>,
    ) -> eyre::Result<()> {
        let res = self
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

        eyre::ensure!(
            res.payload_status.status == PayloadStatusEnum::Valid,
            "Reth has gone out of sync: {:?}",
            res.payload_status.status
        );

        Ok(())
    }

    pub async fn build_submit_payload_irys(
        &self,
        parent: B256,
        attributes: <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadAttributes,
        shadow_txs: Vec<EthPooledTransaction>,
    ) -> eyre::Result<<<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::BuiltPayload>
    {
        let payload = self
            .new_payload_irys(parent, attributes, shadow_txs)
            .await?;
        let _block_hash = self.submit_payload(payload.clone()).await?;
        Ok(payload)
    }

    pub async fn submit_payload(
        &self,
        payload: <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::BuiltPayload,
    ) -> eyre::Result<B256> {
        let block_hash = payload.block().hash();
        self.add_ons_handle
            .beacon_engine_handle
            .new_payload(<IrysEthereumNode as NodeTypes>::Payload::block_to_payload(
                payload.block().clone(),
            ))
            .await?;
        Ok(block_hash)
    }
}
