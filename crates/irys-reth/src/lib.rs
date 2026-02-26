//! # Irys Reth Node
//!
//! ## Shadow Transactions
//! Shadow transactions are special EVM transactions used to encode protocol-level actions such as:
//! - Block rewards (must go to the Irys block producer)
//! - Storage fee collection (balance decrements)
//! - Stake management (release, stake)
//!
//! The CL must validate that:
//! - Block rewards are paid to the correct block producer
//! - Balance increments correspond to rewards
//! - Balance decrements correspond to storage transaction fees

use std::{sync::Arc, time::SystemTime};

use alloy_consensus::TxEip1559;
use alloy_eips::{eip2930::AccessList, eip7840::BlobParams, merge::EPOCH_SLOTS};
use alloy_primitives::{TxKind, U256};
use evm::{IrysBlockAssembler, IrysEvmFactory};
pub use reth::primitives::EthPrimitives;
use reth::{
    api::{FullNodeComponents, FullNodeTypes, NodeTypes, PayloadTypes},
    builder::{
        BuilderContext, DebugNode, Node, NodeAdapter, NodeComponentsBuilder,
        components::{ComponentsBuilder, ExecutorBuilder, PoolBuilder},
    },
    payload::EthBuiltPayload,
    primitives::SealedBlock,
    providers::{EthStorage, StateProviderFactory, providers::ProviderFactoryBuilder},
    rpc::builder::constants::DEFAULT_TX_FEE_CAP_WEI,
    transaction_pool::TransactionValidationTaskExecutor,
};
use reth_chainspec::{ChainSpec, ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_engine_local::LocalPayloadAttributesBuilder;
pub use reth_ethereum_engine_primitives;
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::ConfigureEvm;
use reth_evm_ethereum::RethReceiptBuilder;
use reth_node_builder::rpc::RpcAddOns;
pub use reth_node_ethereum;
use reth_node_ethereum::{
    EthEngineTypes, EthEvmConfig,
    node::{EthereumConsensusBuilder, EthereumEthApiBuilder, EthereumNetworkBuilder},
};
use reth_primitives_traits::{
    BlockTy, constants::MINIMUM_GAS_LIMIT, transaction::error::InvalidTransactionError,
};
pub use reth_provider::{BlockReaderIdExt, providers::BlockchainProvider};
use reth_tracing::tracing;
use reth_transaction_pool::{CoinbaseTipOrdering, TransactionValidationOutcome};
use reth_transaction_pool::{
    EthPoolTransaction, EthPooledTransaction, EthTransactionValidator, Pool, TransactionOrigin,
    TransactionValidator, blobstore::DiskFileBlobStore,
};
use shadow_tx::ShadowTransaction;
use tracing::{debug, info};

use crate::{
    payload_builder_builder::IrysPayloadBuilderBuilder,
    payload_service_builder::IyrsPayloadServiceBuilder,
};

pub mod engine;
pub mod evm;
pub mod payload;
pub mod payload_builder_builder;
pub mod payload_service_builder;
pub mod shadow_tx;
pub mod validator;

pub use engine::{IrysPayloadAttributes, IrysPayloadBuilderAttributes, IrysPayloadTypes};
pub use irys_types::chainspec::{IrysChainHardforks, IrysHardfork};
pub use shadow_tx::{IRYS_SHADOW_EXEC, SHADOW_TX_DESTINATION_ADDR};
pub use validator::{IrysEngineValidator, IrysEngineValidatorBuilder};

#[must_use]
pub fn compose_shadow_tx(
    chain_id: u64,
    shadow_tx: &ShadowTransaction,
    max_priority_fee_per_gas: u128,
) -> TxEip1559 {
    let shadow_tx_buf = crate::shadow_tx::encode_prefixed_input(shadow_tx);
    TxEip1559 {
        access_list: AccessList::default(),
        chain_id,
        // TODO: now that we control the EVM (muhahaha), we can _probably_ bypass these gas validations
        // large enough to not be rejected by the payload builder
        gas_limit: MINIMUM_GAS_LIMIT,
        input: shadow_tx_buf,
        // large enough to not be rejected by the payload builder
        max_fee_per_gas: DEFAULT_TX_FEE_CAP_WEI,
        // Use the provided priority fee
        max_priority_fee_per_gas,
        // nonce is always 0 for shadow txs
        nonce: 0_u64,
        to: TxKind::Call(*SHADOW_TX_DESTINATION_ADDR),
        value: U256::ZERO,
    }
}

/// Type configuration for an Irys-Ethereum node.
#[derive(Debug, Clone, Default)]
pub struct IrysEthereumNode;

impl NodeTypes for IrysEthereumNode {
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;
    type Storage = EthStorage;
    type Payload = EthEngineTypes<IrysPayloadTypes>;
}

impl IrysEthereumNode {
    /// Returns a [`ComponentsBuilder`] configured for a regular Ethereum node.
    #[must_use]
    pub fn components<Node>(
        &self,
    ) -> ComponentsBuilder<
        Node,
        IrysPoolBuilder,
        IyrsPayloadServiceBuilder<IrysPayloadBuilderBuilder>,
        EthereumNetworkBuilder,
        IrysExecutorBuilder,
        EthereumConsensusBuilder,
    >
    where
        Node: FullNodeTypes<Types = Self>,
        <Node::Types as NodeTypes>::Payload: PayloadTypes<
                BuiltPayload = EthBuiltPayload,
                PayloadAttributes = IrysPayloadAttributes,
                PayloadBuilderAttributes = IrysPayloadBuilderAttributes,
            >,
    {
        ComponentsBuilder::default()
            .node_types::<Node>()
            .pool(IrysPoolBuilder::default())
            .executor(IrysExecutorBuilder)
            .payload(IyrsPayloadServiceBuilder::new(IrysPayloadBuilderBuilder))
            .network(EthereumNetworkBuilder::default())
            .consensus(EthereumConsensusBuilder::default())
    }

    #[must_use]
    pub fn provider_factory_builder() -> ProviderFactoryBuilder<Self> {
        ProviderFactoryBuilder::default()
    }
}

impl<N> Node<N> for IrysEthereumNode
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        IrysPoolBuilder,
        IyrsPayloadServiceBuilder<IrysPayloadBuilderBuilder>,
        EthereumNetworkBuilder,
        IrysExecutorBuilder,
        EthereumConsensusBuilder,
    >;

    type AddOns = RpcAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
        EthereumEthApiBuilder,
        IrysEngineValidatorBuilder,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        self.components()
    }

    fn add_ons(&self) -> Self::AddOns {
        RpcAddOns::default()
    }
}

impl<N: FullNodeComponents<Types = Self>> DebugNode<N> for IrysEthereumNode {
    type RpcBlock = alloy_rpc_types_eth::Block;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> reth_ethereum_primitives::Block {
        let alloy_rpc_types_eth::Block {
            header,
            transactions,
            withdrawals,
            ..
        } = rpc_block;
        reth_ethereum_primitives::Block {
            header: header.inner,
            body: reth_ethereum_primitives::BlockBody {
                transactions: transactions
                    .into_transactions()
                    .map(|tx| tx.inner.into_inner().into())
                    .collect(),
                ommers: Vec::default(),
                withdrawals,
            },
        }
    }

    fn local_payload_attributes_builder(
        chain_spec: &Self::ChainSpec,
    ) -> impl reth::api::PayloadAttributesBuilder<
        <Self::Payload as reth::api::PayloadTypes>::PayloadAttributes,
    > {
        LocalPayloadAttributesBuilder::new(Arc::new(chain_spec.clone()))
    }
}

/// A custom pool builder for Irys shadow transaction validation and pool configuration.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct IrysPoolBuilder;

/// Implement the [`PoolBuilder`] trait for the Irys pool builder
///
/// This will be used to build the transaction pool and its maintenance tasks during launch.
///
/// Original code from:
/// <https://github.com/Irys-xyz/reth-irys/blob/6c892d38bfcd6689c618386bd7ccc6fde7fbb64e/crates/ethereum/node/src/node.rs?plain=1#L453>
///
/// Notable changes from the original: we reject all shadow txs, as they are not allowed to land in a the pool.
impl<Node, Evm> PoolBuilder<Node, Evm> for IrysPoolBuilder
where
    Node: FullNodeTypes<Types = IrysEthereumNode>,
    Evm: ConfigureEvm<Primitives = EthPrimitives> + 'static,
{
    type Pool = Pool<
        TransactionValidationTaskExecutor<
            IrysShadowTxValidator<Node::Provider, EthPooledTransaction, Evm>,
        >,
        CoinbaseTipOrdering<EthPooledTransaction>,
        DiskFileBlobStore,
    >;

    async fn build_pool(
        self,
        ctx: &BuilderContext<Node>,
        evm_config: Evm,
    ) -> eyre::Result<Self::Pool> {
        let pool_config = ctx.pool_config();

        let blobs_disabled = ctx.config().txpool.disable_blobs_support
            || ctx.config().txpool.blobpool_max_count == 0;

        let blob_cache_size = if let Some(blob_cache_size) = pool_config.blob_cache_size {
            Some(blob_cache_size)
        } else {
            // get the current blob params for the current timestamp, fallback to default Cancun
            // params
            let current_timestamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_secs();
            let blob_params = ctx
                .chain_spec()
                .blob_params_at_timestamp(current_timestamp)
                .unwrap_or_else(BlobParams::cancun);

            // Derive the blob cache size from the target blob count, to auto scale it by
            // multiplying it with the slot count for 2 epochs: 384 for pectra
            Some((blob_params.target_blob_count * EPOCH_SLOTS * 2) as u32)
        };

        let blob_store =
            reth_node_builder::components::create_blob_store_with_cache(ctx, blob_cache_size)?;

        let validator =
            TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone(), evm_config)
                .set_eip4844(!blobs_disabled)
                .kzg_settings(ctx.kzg_settings()?)
                .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
                .with_local_transactions_config(pool_config.local_transactions_config.clone())
                .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
                .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
                .with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
                .with_additional_tasks(ctx.config().txpool.additional_validation_tasks)
                .build_with_tasks(ctx.task_executor().clone(), blob_store.clone());
        let validator = TransactionValidationTaskExecutor {
            validator: Arc::new(IrysShadowTxValidator {
                eth_tx_validator: validator.validator,
            }),
            to_validation_task: validator.to_validation_task,
        };

        if validator.validator().eth_tx_validator.eip4844() {
            // initializing the KZG settings can be expensive, this should be done upfront so that
            // it doesn't impact the first block or the first gossiped blob transaction, so we
            // initialize this in the background
            let kzg_settings = validator
                .validator()
                .eth_tx_validator
                .kzg_settings()
                .clone();
            ctx.task_executor().spawn_blocking_task(async move {
                let _ = kzg_settings.get();
                tracing::debug!(target: "reth::cli", "Initialized KZG settings");
            });
        }

        let transaction_pool = reth_node_builder::components::TxPoolBuilder::new(ctx)
            .with_validator(validator)
            .build_and_spawn_maintenance_task(blob_store, pool_config)?;

        info!(target: "reth::cli", "Transaction pool initialized");
        debug!(target: "reth::cli", "Spawned txpool maintenance task");

        Ok(transaction_pool)
    }
}

#[derive(Debug)]
pub struct IrysShadowTxValidator<Client, T, Evm> {
    eth_tx_validator: Arc<EthTransactionValidator<Client, T, Evm>>,
}

impl<Client, Tx, Evm> IrysShadowTxValidator<Client, Tx, Evm>
where
    Tx: EthPoolTransaction,
{
    /// Irys-specific prefilter to reject transactions we never accept in the mempool.
    ///
    /// Returns `Ok(tx)` if the tx should continue to normal eth validation,
    /// or `Err(outcome)` if the tx is invalid for Irys-specific reasons.
    #[expect(clippy::result_large_err, reason = "to comply with reth api")]
    fn prefilter_tx(&self, tx: Tx) -> Result<Tx, TransactionValidationOutcome<Tx>> {
        let input = tx.input();
        let to = tx.to();

        match crate::shadow_tx::detect_and_decode_from_parts(to, input) {
            Ok(Some(_)) | Err(_) => {
                tracing::trace!(
                    shadow_tx.sender = ?tx.sender(),
                    shadow_tx.hash = ?tx.hash(),
                    "shadow tx submitted to the pool. Not supported. Likely via gossip post-block"
                );
                return Err(TransactionValidationOutcome::Invalid(
                    tx,
                    reth_transaction_pool::error::InvalidPoolTransactionError::Consensus(
                        InvalidTransactionError::SignerAccountHasBytecode,
                    ),
                ));
            }
            Ok(None) => {}
        }

        // once we support blobs, we can start accepting eip4844 txs
        if tx.is_eip4844() {
            return Err(TransactionValidationOutcome::Invalid(
                tx,
                reth_transaction_pool::error::InvalidPoolTransactionError::Consensus(
                    InvalidTransactionError::Eip4844Disabled,
                ),
            ));
        }

        Ok(tx)
    }
}

impl<Client, Tx, Ev> TransactionValidator for IrysShadowTxValidator<Client, Tx, Ev>
where
    Client: ChainSpecProvider<ChainSpec: EthChainSpec + EthereumHardforks> + StateProviderFactory,
    Tx: EthPoolTransaction,
    Ev: ConfigureEvm,
{
    type Transaction = Tx;
    type Block = BlockTy<Ev::Primitives>;

    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        let transaction = match self.prefilter_tx(transaction) {
            Ok(tx) => tx,
            Err(outcome) => return outcome,
        };

        tracing::trace!(shadow_tx.hash = ?transaction.hash(), "non shadow tx, passing to eth validator");
        self.eth_tx_validator.validate_one(origin, transaction)
    }

    fn on_new_head_block(&self, new_tip_block: &SealedBlock<Self::Block>) {
        self.eth_tx_validator.on_new_head_block(new_tip_block);
    }
}

/// A regular ethereum evm and executor builder.
#[derive(Debug, Default, Clone, Copy)]
pub struct IrysExecutorBuilder;

impl<Types, Node> ExecutorBuilder<Node> for IrysExecutorBuilder
where
    Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>,
    Node: FullNodeTypes<Types = Types>,
{
    type EVM = evm::IrysEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        let evm_config = EthEvmConfig::new(ctx.chain_spec());

        let spec = ctx.chain_spec();
        let evm_factory = IrysEvmFactory::new();
        let evm_config = evm::IrysEvmConfig {
            inner: evm_config,
            assembler: IrysBlockAssembler::new(ctx.chain_spec()),
            executor_factory: evm::IrysBlockExecutorFactory::new(
                RethReceiptBuilder::default(),
                spec,
                evm_factory,
            ),
        };
        Ok(evm_config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use crate::shadow_tx::{
        BLOCK_REWARD_ID, BalanceDecrement, BlockRewardIncrement, ShadowTransaction,
        TransactionPacket, UNSTAKE_ID,
    };
    use crate::test_utils::*;
    use crate::test_utils::{
        DEFAULT_PRIORITY_FEE, advance_blocks, block_reward, eth_payload_attributes_with_parent,
        get_balance, pledge, sign_tx, stake, storage_fees, unpledge, unstake,
    };
    use alloy_consensus::{EthereumTxEnvelope, SignableTransaction as _, TxEip4844};
    use alloy_eips::Encodable2718 as _;
    use alloy_network::{EthereumWallet, TxSigner};
    use alloy_primitives::Bytes;
    use alloy_primitives::Signature;
    use alloy_primitives::{Address, B256};
    use alloy_rpc_types_engine::ForkchoiceState;
    use alloy_signer_local::PrivateKeySigner;
    use reth::api::EngineApiMessageVersion;
    use reth::{
        providers::{AccountReader as _, BlockHashReader as _, BlockNumReader as _},
        rpc::server_types::eth::EthApiError,
    };
    use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
    use reth_payload_primitives::PayloadBuilderAttributes as _;

    use reth_transaction_pool::{PoolTransaction as _, TransactionPool as _};
    use std::sync::Mutex;
    use std::time::Duration;

    /// Ensures that only the allowed shadow tx origin can submit shadow transactions.
    ///
    /// Steps:
    /// - Setup: Use `setup_irys_reth` to launch node, `block_reward` to compose shadow tx, `sign_tx` to sign.
    /// - Action: Inject shadow tx with invalid origin.
    /// - Assertion: Tx is rejected with pool error.
    #[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
    async fn external_users_cannot_submit_shadow_txs() -> eyre::Result<()> {
        // setup
        let ctx = TestContext::new().await?;
        let (node, ctx) = ctx.get_single_node()?;

        let shadow_tx = block_reward();
        let mut shadow_tx_raw = compose_shadow_tx(1, &shadow_tx, DEFAULT_PRIORITY_FEE);
        let signed_tx = ctx
            .target_account
            .sign_transaction(&mut shadow_tx_raw)
            .await
            .unwrap();
        let tx = EthereumTxEnvelope::<TxEip4844>::Eip1559(shadow_tx_raw.into_signed(signed_tx))
            .encoded_2718()
            .into();

        let tx_res = node.rpc.inject_tx(tx).await;
        assert!(matches!(tx_res, Err(EthApiError::PoolError(_))));
        Ok(())
    }

    /// Ensures the mempool rejects EIP-4844 (blob) transactions outright.
    ///
    /// We keep Cancun active but disable blobs. Any EIP-4844 envelope should be rejected
    /// via the mempool validator before deeper validation occurs.
    #[test_log::test(tokio::test)]
    async fn eip4844_txs_are_rejected_by_mempool() -> eyre::Result<()> {
        // setup
        let ctx = TestContext::new().await?;
        let (node, _ctx) = ctx.get_single_node()?;
        let local_signer = PrivateKeySigner::random();
        let envelope: Bytes = TransactionTestContext::tx_with_blobs_bytes(1, local_signer)
            .await
            .expect("constructing a valid EIP-4844 tx should succeed");

        // inject the tx via RPC
        let res = node.rpc.inject_tx(envelope).await;
        dbg!(&res);

        // mempool rejects EIP-4844
        assert!(
            matches!(res, Err(EthApiError::PoolError(_))),
            "expected pool error for EIP-4844"
        );

        Ok(())
    }

    /// Ensures that stale shadow transactions are dropped from the pool after a commit or reorg event.
    ///
    /// Setup:
    /// - Create two nodes with the same block producer.
    /// - Submit a normal tx to node a, and a shadow tx to node b.
    ///
    /// Action:
    /// - Advance the block on node a.
    /// - Update forkchoice on node b to point to the new block that was created on node a.
    ///
    /// Assertion:
    /// - The shadow tx from node b is not included in the block.
    /// - The normal tx from node a is included in the block.
    #[test_log::test(tokio::test)]
    async fn stale_shadow_txs_dont_get_included_in_fcus() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let ((mut node_a, mut node_b), ctx) = ctx.get_two_nodes()?;

        // Submit normal transaction to node a
        let normal_tx_hash = create_and_submit_normal_tx(
            &mut node_a,
            0,
            U256::from(1000),
            1_000_000_000_u128,
            Address::random(),
            &ctx.normal_signer,
        )
        .await?;
        let payload_node_a = advance_block(&mut node_a, vec![]).await?;

        // Submit shadow transaction to node b
        let shadow_tx = create_shadow_tx(BLOCK_REWARD_ID, ctx.block_producer_b.address());
        let shadow_tx = sign_shadow_tx(shadow_tx, &ctx.block_producer_b, 0).await?;
        let shadow_tx_hash = *shadow_tx.hash();
        let _payload_node_b = advance_block(&mut node_b, vec![shadow_tx]).await?;

        // Update forkchoice on node b
        node_b.sync_to(payload_node_a.block().hash()).await?;

        // Assert shadow tx never touched the pool
        let pool_txs: Vec<_> = node_b
            .inner
            .pool
            .all_transactions()
            .all()
            .map(|tx| *tx.hash())
            .collect();
        assert!(
            !pool_txs.contains(&shadow_tx_hash),
            "Shadow tx should never enter the second node pool"
        );

        // Assert shadow tx never touched the pool
        let pool_txs: Vec<_> = node_a
            .inner
            .pool
            .all_transactions()
            .all()
            .map(|tx| *tx.hash())
            .collect();
        assert!(
            !pool_txs.contains(&shadow_tx_hash),
            "Shadow tx should never enter the first node pool"
        );

        // Assert shadow tx never touched the block
        assert_txs_not_in_block(
            &payload_node_a,
            &[shadow_tx_hash],
            "Shadow tx should not be in block",
        );
        // Assert normal tx is in the block
        assert_txs_in_block(
            &payload_node_a,
            &[normal_tx_hash],
            "Normal tx should be in block",
        );
        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn block_with_shadow_txs_gets_broadcasted_between_peers() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let ((mut node_a, mut node_b), ctx) = ctx.get_two_nodes()?;

        let initial_balance = get_balance(&node_a.inner, ctx.block_producer_a.address());

        let amount = U256::from(7000000000000000000_u64);
        let shadow_tx = compose_shadow_tx(
            1,
            &ShadowTransaction::new_v1(
                TransactionPacket::BlockReward(BlockRewardIncrement { amount }),
                alloy_primitives::FixedBytes::ZERO,
            ),
            0, // Block rewards must have 0 priority fee
        );
        let shadow_tx = sign_tx(shadow_tx, &ctx.block_producer_a).await;
        let shadow_tx_hash = *shadow_tx.hash();

        // make the node advance
        let payload = advance_block(&mut node_a, vec![shadow_tx]).await?;

        let block_hash = payload.block().hash();
        let block_number = payload.block().number;
        assert_eq!(block_number, 1);

        // assert the block has been committed to the blockchain
        node_a
            .assert_new_block(shadow_tx_hash, block_hash, block_number)
            .await?;

        // only send forkchoice update to second node
        node_b.update_forkchoice(block_hash, block_hash).await?;

        // expect second node advanced via p2p gossip
        node_b
            .assert_new_block(shadow_tx_hash, block_hash, block_number)
            .await?;

        // The producer A is the beneficiary and receives the block reward (no priority fee since BlockReward has no target)
        assert_balance_change(
            &node_b,
            ctx.block_producer_a.address(),
            initial_balance,
            amount, // block reward only (no priority fee)
            true,
            "Producer A balance should increase by block reward only",
        );

        Ok(())
    }

    // assert that "incrementing" shadow txs update account state
    #[test_log::test(tokio::test)]
    #[rstest::rstest]
    #[case::unstake(unstake, signer_b())]
    #[case::unstake_init_no_balance(unstake, signer_c())]
    async fn incr_shadow_txs(
        #[case] shadow_tx: impl Fn(Address) -> ShadowTransaction,
        #[case] target_signer: Arc<dyn TxSigner<Signature> + Send + Sync>,
    ) -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        let initial_balance = get_balance(&node.inner, target_signer.address());
        let initial_producer_balance = get_balance(&node.inner, ctx.block_producer_a.address());

        let tx_count = 5;
        let shadow_tx = shadow_tx(target_signer.address());
        let shadow_tx_topic = shadow_tx.topic().into();
        let shadow_tx =
            sign_shadow_tx(shadow_tx, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;
        let shadow_txs = vec![shadow_tx.clone(); tx_count];

        let _block_payload = mine_block_and_validate(&mut node, shadow_txs, &[]).await?;

        let block_execution = node.inner.provider.get_state(0..=1).unwrap().unwrap();
        assert_topic_present_in_logs(block_execution, shadow_tx_topic, tx_count as u64);

        // Target should gain from unstake but pay priority fees
        // Each transaction has 1 Gwei priority fee
        let total_priority_fees = U256::from(tx_count) * U256::from(1_000_000_000_u64);
        let unstake_amount = U256::from(tx_count); // 1 wei per unstake tx

        // Calculate expected balance change
        let final_balance = get_balance(&node.inner, target_signer.address());
        let _final_producer_balance = get_balance(&node.inner, ctx.block_producer_a.address());

        // All accounts now have sufficient initial balance to pay fees
        let expected_balance = initial_balance + unstake_amount - total_priority_fees;
        assert_eq!(
            final_balance, expected_balance,
            "Target balance should be initial + unstake - fees"
        );
        // Producer is now the beneficiary, so they receive priority fees
        let expected_producer_gain = total_priority_fees;
        assert_balance_change(
            &node,
            ctx.block_producer_a.address(),
            initial_producer_balance,
            expected_producer_gain,
            true,
            "Producer balance should increase by priority fees (as beneficiary)",
        );
        assert_nonce(
            &node,
            ctx.block_producer_a.address(),
            0,
            "Producer nonce should not change",
        );

        Ok(())
    }

    // check if the "decrementing" shadow txs update account state
    #[test_log::test(tokio::test)]
    #[rstest::rstest]
    #[case::stake(stake, signer_b())]
    #[case::storage_fees(storage_fees, signer_b())]
    async fn decr_shadow_txs(
        #[case] shadow_tx: impl Fn(Address) -> ShadowTransaction,
        #[case] target_signer: Arc<dyn TxSigner<Signature> + Send + Sync>,
    ) -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        let initial_balance = get_balance(&node.inner, target_signer.address());
        let initial_producer_balance = get_balance(&node.inner, ctx.block_producer_a.address());

        let tx_count = 2;
        let shadow_tx = shadow_tx(target_signer.address());
        let shadow_tx_topic = shadow_tx.topic().into();
        let shadow_tx =
            sign_shadow_tx(shadow_tx, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;
        let shadow_txs = vec![shadow_tx.clone(); tx_count];
        let tx_hashes = shadow_txs.iter().map(|tx| *tx.hash()).collect::<Vec<_>>();
        let block_payload = mine_block_and_validate(&mut node, shadow_txs, &[]).await?;

        // Assertions
        let block_execution = node.inner.provider.get_state(0..=1).unwrap().unwrap();

        // Ensure all txs are included in the block
        assert_txs_in_block(&block_payload, &tx_hashes, "Shadow transactions");

        // Assert all txs have a corresponding log
        assert_topic_present_in_logs(block_execution, shadow_tx_topic, tx_count as u64);

        // Assert balance for target is decremented by both the shadow tx amount and priority fees
        // Each transaction costs 1 wei (for stake/storage_fees) + 1 Gwei priority fee
        let total_priority_fees = U256::from(tx_count) * U256::from(1_000_000_000_u64);
        let shadow_tx_cost = U256::from(tx_count); // 1 wei per decrement tx
        let total_cost = shadow_tx_cost + total_priority_fees;

        assert_balance_change(
            &node,
            target_signer.address(),
            initial_balance,
            total_cost,
            false, // is_decrement
            "Target balance should be reduced by shadow tx amount plus priority fees",
        );

        // Producer is now the beneficiary, so they receive priority fees
        let expected_producer_gain = total_priority_fees;
        assert_balance_change(
            &node,
            ctx.block_producer_a.address(),
            initial_producer_balance,
            expected_producer_gain,
            true,
            "Producer balance should increase by priority fees (as beneficiary)",
        );

        assert_nonce(
            &node,
            ctx.block_producer_a.address(),
            0,
            "Producer nonce should not change",
        );

        Ok(())
    }

    // expect that shadow txs get executed first, no matter what. Normal txs get executed only afterwards
    #[test_log::test(tokio::test)]
    async fn test_shadow_tx_ordering() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Create normal transactions with high gas price
        let normal_tx_hashes = create_and_submit_multiple_normal_txs(
            &mut node,
            3,
            0,
            10_000_000_000_u128, // High gas price
            Address::random(),
            &ctx.normal_signer,
        )
        .await?;

        // Create shadow transactions with lower effective priority
        let shadow_tx = create_shadow_tx(UNSTAKE_ID, ctx.target_account.address());
        let shadow_tx =
            sign_shadow_tx(shadow_tx, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;
        let shadow_txs = vec![shadow_tx.clone(); 2];
        let shadow_tx_hashes = shadow_txs.iter().map(|tx| *tx.hash()).collect::<Vec<_>>();

        let block_payload =
            mine_block_and_validate(&mut node, shadow_txs, &normal_tx_hashes).await?;

        assert_txs_in_block(&block_payload, &shadow_tx_hashes, "Shadow transactions");
        assert_txs_in_block(&block_payload, &normal_tx_hashes, "Normal transactions");
        assert_shadow_txs_before_normal_txs(&block_payload, &shadow_tx_hashes, &normal_tx_hashes);

        Ok(())
    }

    // test decrementing when account does not exist (expect that even receipt not created)
    #[test_log::test(tokio::test)]
    async fn test_decrement_nonexistent_account() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Create a random address that has never existed on chain
        let nonexistent_address = Address::random();

        // Verify the account doesn't exist
        let account = node
            .inner
            .provider
            .basic_account(&nonexistent_address)
            .unwrap();
        assert!(account.is_none(), "Test account should not exist");

        // Create and submit a shadow transaction trying to decrement balance of non-existent account
        let shadow_tx = ShadowTransaction::new_v1(
            TransactionPacket::Stake(BalanceDecrement {
                amount: U256::ONE,
                target: nonexistent_address,
                irys_ref: alloy_primitives::FixedBytes::ZERO,
            }),
            alloy_primitives::FixedBytes::ZERO,
        );
        let shadow_tx =
            sign_shadow_tx(shadow_tx, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;

        // Submit a normal transaction to ensure there's something in the mempool
        let _normal_tx_hash = create_and_submit_normal_tx(
            &mut node,
            0,
            U256::from(1000),
            1_000_000_000_u128,
            Address::random(),
            &ctx.normal_signer,
        )
        .await?;

        // Attempt to produce a new block - this should fail because the shadow transaction
        // tries to deduct priority fees from a non-existent account

        // Set up the payload attributes with shadow txs
        node.payload.timestamp += 1;
        let base_attributes = (node.payload.attributes_generator)(node.payload.timestamp);
        let attributes = IrysPayloadBuilderAttributes {
            inner: base_attributes.inner,
            id: base_attributes.id,
            shadow_txs: vec![shadow_tx],
        };

        // Send the payload - this will trigger block building
        node.payload
            .payload_builder
            .send_new_payload(attributes.clone())
            .await
            .unwrap()?;

        // Wait briefly for the payload builder to process
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Try to get the payload - this should return an error
        let result = node
            .payload
            .payload_builder
            .best_payload(attributes.payload_id())
            .await;

        // Verify the payload build failed with an error
        let err = result
            .expect("Expected Some result from best_payload")
            .expect_err("Block production should have failed due to non-existent account");

        let error_msg = format!("{err:?}");
        assert!(
            error_msg.contains("Shadow transaction priority fee failed"),
            "Expected shadow transaction priority fee failure for non-existent account, got: {error_msg}"
        );

        Ok(())
    }

    // test decrementing when account exists but not enough balance (expect block production failure)
    #[test_log::test(tokio::test)]
    async fn test_decrement_insufficient_balance() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        let funded_balance = get_balance(&node.inner, ctx.normal_signer.address());

        assert!(
            funded_balance > U256::ZERO,
            "Funded account should have nonzero balance"
        );

        // Create a shadow tx that tries to decrement more than the balance
        // This should cause block production to fail entirely
        let decrement_amount = funded_balance + U256::from(DEFAULT_PRIORITY_FEE) + U256::ONE;
        let shadow_tx = ShadowTransaction::new_v1(
            TransactionPacket::Stake(BalanceDecrement {
                amount: decrement_amount,
                target: ctx.normal_signer.address(),
                irys_ref: alloy_primitives::FixedBytes::ZERO,
            }),
            alloy_primitives::FixedBytes::ZERO,
        );
        let shadow_tx =
            sign_shadow_tx(shadow_tx, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;

        // Attempt to produce a new block - this should fail because the shadow transaction
        // tries to decrement more than the available balance

        // Set up the payload attributes with shadow txs
        node.payload.timestamp += 1;
        let base_attributes = (node.payload.attributes_generator)(node.payload.timestamp);
        let attributes = IrysPayloadBuilderAttributes {
            inner: base_attributes.inner,
            id: base_attributes.id,
            shadow_txs: vec![shadow_tx],
        };

        // Send the payload - this will trigger block building
        node.payload
            .payload_builder
            .send_new_payload(attributes.clone())
            .await
            .unwrap()?;

        // Wait briefly for the payload builder to process
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Try to get the payload - this should return an error
        let result = node
            .payload
            .payload_builder
            .best_payload(attributes.payload_id())
            .await;

        // Verify the payload build failed with an error
        let err = result
            .expect("Expected Some result from best_payload")
            .expect_err("Block production should have failed due to insufficient balance");

        let error_msg = format!("{err:?}");
        assert!(
            error_msg.contains("insufficient balance")
                || error_msg.contains("Shadow transaction failed")
                || error_msg.contains("EVM"),
            "Expected shadow transaction insufficient balance failure, got: {error_msg}"
        );

        Ok(())
    }

    #[rstest::fixture]
    fn signer_b() -> Arc<dyn TxSigner<Signature> + Send + Sync> {
        let wallets = Wallet::new(2).wallet_gen();
        let signer_b = EthereumWallet::from(wallets[1].clone());

        (signer_b.default_signer()) as _
    }

    #[rstest::fixture]
    fn signer_c() -> Arc<dyn TxSigner<Signature> + Send + Sync> {
        let wallets = Wallet::new(3).wallet_gen();
        let signer_c = EthereumWallet::from(wallets[2].clone());
        (signer_c.default_signer()) as _
    }

    #[rstest::fixture]
    fn signer_random() -> Arc<dyn TxSigner<Signature> + Send + Sync> {
        Arc::new(PrivateKeySigner::random())
    }

    /// Mines 5 blocks, each with a shadow (block reward) and a normal tx.
    /// Asserts both txs are present in every block.
    /// Verifies sequential block production and tx inclusion.
    /// Expects latest block number to be 5 at the end.
    #[test_log::test(tokio::test)]
    async fn mine_5_blocks_with_shadow_and_normal_tx() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        let recipient = ctx.target_account.address();

        for block_number in 1..=5 {
            // Block reward shadow tx
            let shadow_tx = block_reward();
            let shadow_tx = sign_shadow_tx(shadow_tx, &ctx.block_producer_a, 0).await?; // Block rewards must have 0 priority fee

            // Normal tx
            let normal_tx_hash = create_and_submit_normal_tx(
                &mut node,
                block_number - 1,
                U256::from(1234_u64),
                2_000_000_000_u128, // 2 Gwei
                recipient,
                &ctx.normal_signer,
            )
            .await?;

            // Mine block
            let _block_payload =
                mine_block_and_validate(&mut node, vec![shadow_tx], &[normal_tx_hash]).await?;
        }

        // Assert that the current block is the latest block
        let latest_block = node
            .inner
            .provider
            .consistent_provider()
            .unwrap()
            .best_block_number()
            .unwrap();
        assert_eq!(latest_block, 5, "Latest block is not 5");

        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn rollback_state_revert_on_fork_switch() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let ((mut node_a, mut node_b), ctx) = ctx.get_two_nodes()?;
        // Use the producer B address for checking rewards (it's the beneficiary for node B's blocks)
        let reward_address = ctx.block_producer_b.address();

        // Node A: advance 3 blocks, 2 shadow txs per block
        let shadow_tx = block_reward();
        let shadow_txs = vec![vec![shadow_tx; 2]; 3];

        let _block_hashes_a =
            advance_blocks(&mut node_a, shadow_txs, &ctx.block_producer_a).await?;
        let consistent_provider_a = node_a.inner.provider.consistent_provider().unwrap();
        let account_a_on_fork = consistent_provider_a
            .basic_account(&ctx.block_producer_a.address())
            .unwrap()
            .unwrap();
        let node_a_reward_balance = get_balance(&node_a.inner, reward_address);

        // Node B: advance 4 blocks, 1 shadow tx per block
        let shadow_tx = block_reward();
        let shadow_txs = vec![vec![shadow_tx; 1]; 4];
        let _block_hashes_b =
            advance_blocks(&mut node_b, shadow_txs, &ctx.block_producer_b).await?;

        // Record Node B's state after 4 blocks
        let consistent_provider_b = node_b.inner.provider.consistent_provider().unwrap();
        let block_producer_node_b_account_b = consistent_provider_b
            .basic_account(&ctx.block_producer_b.address())
            .unwrap()
            .unwrap();
        let best_block_b = consistent_provider_b.best_block_number().unwrap();
        let block_hash_b = consistent_provider_b
            .block_hash(best_block_b)
            .unwrap()
            .unwrap();
        let reward_balance_node_b = get_balance(&node_b.inner, reward_address);

        // Node A switches forkchoice to Node B's latest block
        node_a.sync_to(block_hash_b).await?;

        // Node A's state should match Node B's
        let consistent_provider_a = node_a.inner.provider.consistent_provider().unwrap();
        let account_a = consistent_provider_a
            .basic_account(&ctx.block_producer_a.address())
            .unwrap()
            .unwrap();
        let best_block_a = consistent_provider_a.best_block_number().unwrap();
        let block_hash_a = consistent_provider_a.block_hash(4).unwrap().unwrap();
        let reward_balance_post_switch_node_a = get_balance(&node_a.inner, reward_address);
        assert_ne!(
            ctx.block_producer_a.address(),
            ctx.block_producer_b.address()
        );
        assert_eq!(best_block_b, 4);
        assert_eq!(best_block_a, 4);
        assert_eq!(
            block_hash_a, block_hash_b,
            "block hashes after sync must be equal"
        );
        assert_eq!(reward_balance_post_switch_node_a, reward_balance_node_b);
        // Producer B (beneficiary for node B) starts with initial balance and gets:
        // - 4 block rewards (1 wei each) = 4 wei
        // - NO priority fees since block rewards have no target
        let expected_balance =
            U256::from_str_radix("1000000000000000000000000", 10).unwrap() + U256::from(4); // block rewards only
        assert_eq!(reward_balance_post_switch_node_a, expected_balance);
        assert_ne!(
            node_a_reward_balance, reward_balance_node_b,
            "initial rewards differ on forks produced by each chain"
        );
        assert_eq!(
            account_a_on_fork.nonce, account_a.nonce,
            "nonce should be the same for each block producer account"
        );
        assert_eq!(
            block_producer_node_b_account_b.nonce, 0,
            "nonce should be 0 for each block producer account"
        );
        assert_eq!(
            account_a.nonce, 0,
            "nonce should be 0 for each block producer account"
        );
        assert_eq!(
            best_block_a, best_block_b,
            "Canonical block height should match after forkchoice switch"
        );

        Ok(())
    }

    /// Tests state rollback functionality on safe block reorgs.
    ///
    /// This test verifies that when a forkchoice update rolls back to an earlier safe block,
    /// the state is correctly reverted and subsequent blocks can be built on the rolled-back state.
    ///
    /// Test scenario:
    /// 1. Build 4 blocks with block rewards (balance +4)
    /// 2. Verify state after 4 blocks: balance = initial + 4
    /// 3. Roll back to block 1 via forkchoice update (safe/finalized = block 1)
    /// 4. Build a new fork block (block 2) on top of the rolled-back state
    /// 5. Verify final state: balance = initial + 2, nonce = 1 (reflecting the rollback and new block)
    #[test_log::test(tokio::test)]
    async fn rollback_state_on_safe_blocks() -> eyre::Result<()> {
        // Setup custom parent tracker for forkchoice updates
        let parent_tracker = Arc::new(Mutex::new(B256::ZERO));

        // Create context with custom attributes that can track parent block
        let payload_attributes = {
            let parent_tracker = parent_tracker.clone();
            move |timestamp: u64, beneficiary: Address| {
                let parent = *parent_tracker.lock().unwrap();
                let mut attrs = eth_payload_attributes_with_parent(timestamp, parent);
                attrs.inner.suggested_fee_recipient = beneficiary;
                attrs
            }
        };

        let ctx = TestContext::new_with_custom_attributes(payload_attributes).await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Initial setup and baseline measurements
        let initial_balance = get_balance(&node.inner, ctx.block_producer_a.address());
        let mut parent_blockhash = ctx.genesis_blockhash;
        let mut block_hashes = vec![parent_blockhash];

        // Phase 1: Build 4 blocks with shadow transactions
        tracing::info!("Phase 1: Building 4 blocks with block rewards");
        for block_number in 1..=4 {
            // Create block reward transaction
            let block_reward_tx = block_reward();
            let block_reward_tx = sign_shadow_tx(block_reward_tx, &ctx.block_producer_a, 0) // Block rewards must have 0 priority fee
                .await?;

            // Mine the block
            let payload = mine_block_and_validate(&mut node, vec![block_reward_tx], &[]).await?;
            parent_blockhash = payload.block().hash();
            block_hashes.push(parent_blockhash);

            tracing::info!("Built block {}: {}", block_number, parent_blockhash);
        }

        // Phase 2: Verify state after 4 blocks
        tracing::info!("Phase 2: Verifying state after building 4 blocks");
        let best_block = node
            .inner
            .provider
            .consistent_provider()
            .unwrap()
            .best_block_number()
            .unwrap();
        assert_eq!(best_block, 4, "Should be at block 4");
        // Each block reward transaction gives 1 wei to producer A (beneficiary for node 0)
        assert_balance_change(
            &node,
            ctx.block_producer_a.address(),
            initial_balance,
            U256::from(4), // 4 block rewards only
            true,
            "Balance should reflect 4 block rewards",
        );
        assert_nonce(
            &node,
            ctx.block_producer_a.address(),
            0,
            "Nonce should be 0",
        );

        // Phase 3: Roll back to block 1 (safe/finalized)
        tracing::info!("Phase 3: Rolling back to block 1 via forkchoice update");
        let rollback_target = block_hashes[1]; // Block 1
        node.inner
            .add_ons_handle
            .beacon_engine_handle
            .fork_choice_updated(
                ForkchoiceState {
                    head_block_hash: rollback_target,
                    safe_block_hash: rollback_target,
                    finalized_block_hash: rollback_target,
                },
                None,
                EngineApiMessageVersion::default(),
            )
            .await?;

        // Allow time for rollback to process
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Phase 4: Build new fork block on rolled-back state
        tracing::info!("Phase 4: Building new fork block on rolled-back state");
        let fork_block_number = 2; // Building block 2 on top of block 1
        // Update parent tracker for payload attributes
        *parent_tracker.lock().unwrap() = rollback_target;
        let fork_reward_tx = block_reward();
        let fork_reward_tx = sign_shadow_tx(fork_reward_tx, &ctx.block_producer_a, 0).await?; // Block rewards must have 0 priority fee
        let fork_payload = prepare_block(&mut node, vec![fork_reward_tx]).await?;
        let fork_block_hash = fork_payload.block().hash();

        tracing::info!(
            "Built fork block {}: {}",
            fork_block_number,
            fork_block_hash
        );

        // Phase 5: Finalize the new fork
        tracing::info!("Phase 5: Finalizing the new fork");
        node.inner
            .add_ons_handle
            .beacon_engine_handle
            .fork_choice_updated(
                ForkchoiceState {
                    head_block_hash: fork_block_hash,
                    safe_block_hash: fork_block_hash,
                    finalized_block_hash: fork_block_hash,
                },
                None,
                EngineApiMessageVersion::default(),
            )
            .await?;

        // Allow time for finalization to process
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Phase 6: Verify final state after rollback and fork
        tracing::info!("Phase 6: Verifying final state after rollback and fork");
        let final_best_block = node
            .inner
            .provider
            .consistent_provider()
            .unwrap()
            .best_block_number()
            .unwrap();
        let final_best_hash = node
            .inner
            .provider
            .consistent_provider()
            .unwrap()
            .block_hash(final_best_block)
            .unwrap()
            .unwrap();

        // Assertions for final state
        assert_eq!(
            fork_block_hash, final_best_hash,
            "Fork block should be the canonical head"
        );
        assert_eq!(
            final_best_block, fork_block_number,
            "Should be at fork block number"
        );
        // Each block reward transaction gives 1 wei to producer A (beneficiary for node 0)
        assert_balance_change(
            &node,
            ctx.block_producer_a.address(),
            initial_balance,
            U256::from(2), // 2 block rewards only
            true,
            "Balance should reflect rollback to block 1 + new fork block reward",
        );
        assert_nonce(
            &node,
            ctx.block_producer_a.address(),
            0,
            "Nonce always be 0",
        );

        tracing::info!("Rollback test completed successfully");
        Ok(())
    }

    /// Tests state rollback functionality on safe block reorgs.
    ///
    /// This test verifies that when a forkchoice update rolls back to an earlier safe block,
    /// the state is correctly reverted and subsequent blocks can be built on the rolled-back state.
    ///
    /// Test scenario:
    /// 1. Build 4 blocks with block rewards (balance +4)
    /// 2. Verify state after 4 blocks: balance = initial + 4
    /// 3. Roll back to block 1 via forkchoice update (safe/finalized = block 1)
    /// 4. Build a new fork block (block 2) on top of the rolled-back state
    /// 5. Verify final state: balance = initial + 2, nonce = 1 (reflecting the rollback and new block)
    #[test_log::test(tokio::test)]
    async fn rollback_state_on_confirmed_blocks() -> eyre::Result<()> {
        // Setup custom parent tracker for forkchoice updates
        let parent_tracker = Arc::new(Mutex::new(B256::ZERO));

        // Create context with custom attributes that can track parent block
        let payload_attributes = {
            let parent_tracker = parent_tracker.clone();
            move |timestamp: u64, beneficiary: Address| {
                let parent = *parent_tracker.lock().unwrap();
                let mut attrs = eth_payload_attributes_with_parent(timestamp, parent);
                attrs.inner.suggested_fee_recipient = beneficiary;
                attrs
            }
        };

        let ctx = TestContext::new_with_custom_attributes(payload_attributes).await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Initial setup and baseline measurements
        let initial_balance = get_balance(&node.inner, ctx.block_producer_a.address());
        let mut parent_blockhash = ctx.genesis_blockhash;
        let mut block_hashes = vec![parent_blockhash];

        // Phase 1: Build 4 blocks with shadow transactions
        tracing::info!("Phase 1: Building 4 blocks with block rewards");
        for block_number in 1..=4 {
            // Create block reward transaction
            let block_reward_tx = block_reward();
            let block_reward_tx = sign_shadow_tx(block_reward_tx, &ctx.block_producer_a, 0) // Block rewards must have 0 priority fee
                .await?;

            // Mine the block
            let payload = mine_block_and_validate(&mut node, vec![block_reward_tx], &[]).await?;
            parent_blockhash = payload.block().hash();
            block_hashes.push(parent_blockhash);

            tracing::info!("Built block {}: {}", block_number, parent_blockhash);
        }

        // Phase 2: Verify state after 4 blocks
        tracing::info!("Phase 2: Verifying state after building 4 blocks");
        let best_block = node
            .inner
            .provider
            .consistent_provider()
            .unwrap()
            .best_block_number()
            .unwrap();
        assert_eq!(best_block, 4, "Should be at block 4");
        // Each block reward transaction gives 1 wei to producer A (beneficiary for node 0)
        assert_balance_change(
            &node,
            ctx.block_producer_a.address(),
            initial_balance,
            U256::from(4), // 4 block rewards only
            true,
            "Balance should reflect 4 block rewards",
        );
        assert_nonce(
            &node,
            ctx.block_producer_a.address(),
            0,
            "Nonce should be 0",
        );

        // Phase 3: Roll back to block 1 (safe/finalized)
        tracing::info!("Phase 3: Rolling back to block 1 via forkchoice update");
        let rollback_target = block_hashes[1]; // Block 1
        node.inner
            .add_ons_handle
            .beacon_engine_handle
            .fork_choice_updated(
                ForkchoiceState {
                    head_block_hash: rollback_target,
                    safe_block_hash: block_hashes[0],
                    finalized_block_hash: block_hashes[0],
                },
                None,
                EngineApiMessageVersion::default(),
            )
            .await?;

        // Allow time for rollback message to propagate. Reth does not immediately rewind the
        // canonical head when supplied an older forkchoice head; the actual tip moves once a new
        // payload becomes canonical. We only assert on the final forkchoice state after building
        // the replacement block below.
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Phase 4: Build new fork block on rolled-back state
        tracing::info!("Phase 4: Building new fork block on rolled-back state");
        let fork_block_number = 2; // Building block 2 on top of block 1
        // Update parent tracker for payload attributes
        *parent_tracker.lock().unwrap() = rollback_target;
        let fork_reward_tx = block_reward();
        let fork_reward_tx = sign_shadow_tx(fork_reward_tx, &ctx.block_producer_a, 0).await?; // Block rewards must have 0 priority fee
        let fork_payload = prepare_block(&mut node, vec![fork_reward_tx]).await?;
        let fork_block_hash = fork_payload.block().hash();

        tracing::info!(
            "Built fork block {}: {}",
            fork_block_number,
            fork_block_hash
        );

        // Phase 5: Finalize the new fork
        tracing::info!("Phase 5: Finalizing the new fork");
        node.inner
            .add_ons_handle
            .beacon_engine_handle
            .fork_choice_updated(
                ForkchoiceState {
                    head_block_hash: fork_block_hash,
                    safe_block_hash: fork_block_hash,
                    finalized_block_hash: fork_block_hash,
                },
                None,
                EngineApiMessageVersion::default(),
            )
            .await?;

        // Allow time for finalization to process
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Phase 6: Verify final state after rollback and fork
        tracing::info!("Phase 6: Verifying final state after rollback and fork");
        let final_best_block = node
            .inner
            .provider
            .consistent_provider()
            .unwrap()
            .best_block_number()
            .unwrap();
        let final_best_hash = node
            .inner
            .provider
            .consistent_provider()
            .unwrap()
            .block_hash(final_best_block)
            .unwrap()
            .unwrap();

        // Assertions for final state
        assert_eq!(
            fork_block_hash, final_best_hash,
            "Fork block should be the canonical head"
        );
        assert_eq!(
            final_best_block, fork_block_number,
            "Should be at fork block number"
        );
        // Each block reward transaction gives 1 wei to producer A (beneficiary for node 0)
        assert_balance_change(
            &node,
            ctx.block_producer_a.address(),
            initial_balance,
            U256::from(2), // 2 block rewards only
            true,
            "Balance should reflect rollback to block 1 + new fork block reward",
        );
        assert_nonce(
            &node,
            ctx.block_producer_a.address(),
            0,
            "Nonce always be 0",
        );

        tracing::info!("Rollback test completed successfully");
        Ok(())
    }

    /// Tests that shadow transactions never enter the transaction pool when rolling back state to a past block.
    ///
    /// Test scenario:
    /// 1. Setup a node and create shadow transactions
    /// 2. Mine blocks with shadow transactions to establish state
    /// 3. Rollback the state to a past block
    /// 4. Verify that shadow transactions are never in the transaction pool past
    #[test_log::test(tokio::test)]
    async fn shadow_txs_never_in_pool_during_rollback() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Phase 1: Build initial blocks with shadow transactions
        let _initial_balance = get_balance(&node.inner, ctx.block_producer_a.address());
        let mut parent_blockhash = ctx.genesis_blockhash;
        let mut block_hashes = vec![parent_blockhash];
        let mut shadow_txs = vec![];

        // Build 3 blocks with shadow transactions
        for block_number in 1..=3 {
            let block_reward_tx = block_reward();
            let block_reward_tx = sign_shadow_tx(block_reward_tx, &ctx.block_producer_a, 0) // Block rewards must have 0 priority fee
                .await?;
            shadow_txs.push(block_reward_tx.clone());

            let normal_tx = create_and_submit_normal_tx(
                &mut node,
                block_number - 1,
                U256::from(1234_u64),
                2_000_000_000_u128, // 2 Gwei
                ctx.target_account.address(),
                &ctx.normal_signer,
            )
            .await?;

            let payload =
                mine_block_and_validate(&mut node, vec![block_reward_tx], &[normal_tx]).await?;
            parent_blockhash = payload.block().hash();
            block_hashes.push(parent_blockhash);
        }

        // Verify we're at block 3
        let best_block = node
            .inner
            .provider
            .consistent_provider()
            .unwrap()
            .best_block_number()
            .unwrap();
        assert_eq!(best_block, 3, "Should be at block 3");

        // Phase 3: Rollback to block 1
        let rollback_target = block_hashes[1]; // Block 1
        tracing::info!("Rolling back to block 1: {}", rollback_target);

        node.inner
            .add_ons_handle
            .beacon_engine_handle
            .fork_choice_updated(
                ForkchoiceState {
                    head_block_hash: rollback_target,
                    safe_block_hash: rollback_target,
                    finalized_block_hash: rollback_target,
                },
                None,
                EngineApiMessageVersion::default(),
            )
            .await?;

        // Allow time for rollback to process
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Phase 4: Verify shadow transactions never entered the pool during rollback
        // The key invariant is that shadow txs should NEVER be in the transaction pool regardless of rollback state
        let pool_txs: Vec<_> = node
            .inner
            .pool
            .all_transactions()
            .all()
            .map(|tx| *tx.hash())
            .collect();

        for tx in shadow_txs {
            assert!(
                !pool_txs.contains(tx.hash()),
                "Shadow tx should never be in the transaction pool during rollback"
            );
            assert!(
                pool_txs.is_empty(),
                "Transaction pool should be empty during rollback, but contains: {pool_txs:?}"
            );
        }

        // Phase 5: Try to submit the future shadow transactions directly to the pool
        // They should be rejected and never enter the pool
        let future_shadow_tx_1 = block_reward();
        let mut tx_1_raw = compose_shadow_tx(1, &future_shadow_tx_1, DEFAULT_PRIORITY_FEE);
        let signed_tx_1 = ctx
            .block_producer_a
            .sign_transaction(&mut tx_1_raw)
            .await
            .unwrap();
        let tx_1_envelope =
            EthereumTxEnvelope::<TxEip4844>::Eip1559(tx_1_raw.into_signed(signed_tx_1))
                .encoded_2718()
                .into();

        // These should fail since shadow txs are not allowed in the pool
        let tx_1_result = node.rpc.inject_tx(tx_1_envelope).await;

        assert!(
            tx_1_result.is_err(),
            "Shadow transaction should be rejected when submitted to pool"
        );

        // Phase 6: Final check - pool should still be empty
        let final_pool_txs: Vec<_> = node
            .inner
            .pool
            .all_transactions()
            .all()
            .map(|tx| *tx.hash())
            .collect();

        assert!(
            final_pool_txs.is_empty(),
            "Transaction pool should remain empty after attempted shadow tx submission, but contains: {final_pool_txs:?}"
        );

        Ok(())
    }

    /// Tests that shadow transactions are executed in the exact order they were submitted.
    /// This is verified by checking the transaction hashes in the receipts match the submission order.
    ///
    /// Test scenario:
    /// 1. Create 5 different types of shadow transactions in a specific order
    /// 2. Submit them to the shadow transaction store
    /// 3. Mine a block containing these transactions
    /// 4. Verify the receipts contain the transactions in the same order as submitted
    #[test_log::test(tokio::test)]
    async fn test_shadow_tx_execution_order_via_receipts() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Create different addresses for different transaction types
        let address_a = ctx.block_producer_a.address();
        let address_b = ctx.target_account.address();
        let address_c = ctx.normal_signer.address();

        // Create 5 different shadow transactions in a specific order
        let mut shadow_txs = Vec::new();
        let mut expected_tx_hashes = Vec::new();

        // 1. Block reward
        let block_reward_tx = block_reward();
        let block_reward_tx = sign_shadow_tx(block_reward_tx, &ctx.block_producer_a, 0).await?; // Block rewards must have 0 priority fee
        expected_tx_hashes.push(*block_reward_tx.hash());
        shadow_txs.push(block_reward_tx);

        // 2. Unstake
        let unstake_tx = unstake(address_b);
        let unstake_tx =
            sign_shadow_tx(unstake_tx, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;
        expected_tx_hashes.push(*unstake_tx.hash());
        shadow_txs.push(unstake_tx);

        // 3. Storage fees
        let storage_fees_tx = storage_fees(address_c);
        let storage_fees_tx =
            sign_shadow_tx(storage_fees_tx, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;
        expected_tx_hashes.push(*storage_fees_tx.hash());
        shadow_txs.push(storage_fees_tx);

        // 4. Another block reward
        let block_reward_2_tx = block_reward();
        let block_reward_2_tx = sign_shadow_tx(
            block_reward_2_tx,
            &ctx.block_producer_a,
            0, // Block rewards must have 0 priority fee
        )
        .await?;
        expected_tx_hashes.push(*block_reward_2_tx.hash());
        shadow_txs.push(block_reward_2_tx);

        // 5. Stake transaction
        let stake_tx = stake(address_a);
        let stake_tx =
            sign_shadow_tx(stake_tx, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;
        expected_tx_hashes.push(*stake_tx.hash());
        shadow_txs.push(stake_tx);

        tracing::info!(
            "Created {} shadow transactions in order: {:?}",
            shadow_txs.len(),
            expected_tx_hashes
        );

        // Mine a block with these shadow transactions
        let block_payload = mine_block(&mut node, shadow_txs).await?;

        // Get execution results to verify receipt ordering
        let block_execution = node.inner.provider.get_state(0..=1).unwrap().unwrap();
        let receipts = &block_execution.receipts;

        // Verify we have receipts for block 1
        assert!(
            receipts.len() > 1,
            "Should have receipts for at least block 1"
        );
        let block_1_receipts = &receipts[1];

        tracing::info!("Block 1 has {} receipts", block_1_receipts.len());
        assert_eq!(
            block_1_receipts.len(),
            expected_tx_hashes.len(),
            "Should have exactly {} receipts for the {} shadow transactions",
            expected_tx_hashes.len(),
            expected_tx_hashes.len()
        );

        // Get the transaction hashes from the block in order
        let block_tx_hashes: Vec<_> = block_payload
            .block()
            .body()
            .transactions
            .iter()
            .map(|tx| *tx.hash())
            .collect();

        tracing::info!("Block transaction order: {:?}", block_tx_hashes);

        // Verify the transactions appear in the block in the same order as submitted
        for (i, expected_hash) in expected_tx_hashes.iter().enumerate() {
            assert_eq!(
                block_tx_hashes[i], *expected_hash,
                "Transaction at position {} should match submitted order. Expected: {:?}, Got: {:?}",
                i, expected_hash, block_tx_hashes[i]
            );
        }

        // Verify all receipts are successful (shadow transactions should succeed)
        for (i, receipt) in block_1_receipts.iter().enumerate() {
            assert!(
                receipt.success,
                "Receipt at position {} should be successful for shadow transaction {:?}",
                i, expected_tx_hashes[i]
            );
        }

        Ok(())
    }

    /// Test pledge transaction (balance decrement)
    #[test_log::test(tokio::test)]
    async fn test_pledge_balance_decrement() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        let target_address = ctx.normal_signer.address();
        let initial_balance = get_balance(&node.inner, target_address);
        assert!(
            initial_balance > U256::ZERO,
            "Target account should have initial balance"
        );

        // Create pledge transaction with a smaller amount to ensure it's less than initial balance
        let pledge_amount = U256::from(1_000_000_000_000_000_u64); // 0.001 IRYS
        let pledge_tx = ShadowTransaction::new_v1(
            TransactionPacket::Pledge(BalanceDecrement {
                amount: pledge_amount,
                target: target_address,
                irys_ref: alloy_primitives::FixedBytes::ZERO,
            }),
            alloy_primitives::FixedBytes::ZERO,
        );
        let pledge_tx = sign_shadow_tx(pledge_tx, &ctx.normal_signer, DEFAULT_PRIORITY_FEE).await?;
        let pledge_tx_hash = *pledge_tx.hash();

        // Mine block with pledge transaction
        let block_payload = mine_block(&mut node, vec![pledge_tx]).await?;

        // Verify transaction is included in block
        assert_txs_in_block(&block_payload, &[pledge_tx_hash], "Pledge transaction");

        // Verify balance decreased
        assert_balance_change(
            &node,
            target_address,
            initial_balance,
            pledge_amount + U256::from(DEFAULT_PRIORITY_FEE),
            false,
            "Target balance should decrease after pledge",
        );

        Ok(())
    }

    /// Test unpledge transaction (priority-fee-only at inclusion)
    #[test_log::test(tokio::test)]
    async fn test_unpledge_fee_only() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Use a funded account
        let target_address = ctx.normal_signer.address();

        let balance_after_initial_funding = get_balance(&node.inner, target_address);
        assert!(
            balance_after_initial_funding > U256::ZERO,
            "Target account should have initial balance"
        );

        // Create unpledge transaction: fee-only via priority fee
        let unpledge_tx = unpledge(target_address);
        let unpledge_tx =
            sign_shadow_tx(unpledge_tx, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;
        let unpledge_tx_hash = *unpledge_tx.hash();

        // Mine block with unpledge transaction
        let block_payload = mine_block(&mut node, vec![unpledge_tx]).await?;

        // Verify transaction is included in block
        assert_txs_in_block(&block_payload, &[unpledge_tx_hash], "Unpledge transaction");

        // Verify balance decreased by exactly the priority fee
        assert_balance_change(
            &node,
            target_address,
            balance_after_initial_funding,
            U256::from(DEFAULT_PRIORITY_FEE),
            false,
            "Target balance should decrease by priority fee only on unpledge",
        );

        Ok(())
    }

    /// Test unstake-debit transaction (priority-fee-only at inclusion)
    #[test_log::test(tokio::test)]
    async fn test_unstake_debit_fee_only() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Use a funded account
        let target_address = ctx.normal_signer.address();

        let balance_after_initial_funding = get_balance(&node.inner, target_address);
        assert!(
            balance_after_initial_funding > U256::ZERO,
            "Target account should have initial balance"
        );

        // Create unstake-debit transaction: fee-only via priority fee
        let unstake_debit_tx = unstake_debit(target_address);
        let unstake_debit_tx = sign_shadow_tx(
            unstake_debit_tx,
            &ctx.block_producer_a,
            DEFAULT_PRIORITY_FEE,
        )
        .await?;
        let unstake_debit_tx_hash = *unstake_debit_tx.hash();

        // Mine block with unstake-debit transaction
        let block_payload = mine_block(&mut node, vec![unstake_debit_tx]).await?;

        // Verify transaction is included in block
        assert_txs_in_block(
            &block_payload,
            &[unstake_debit_tx_hash],
            "UnstakeDebit transaction",
        );

        // Verify balance decreased by exactly the priority fee
        assert_balance_change(
            &node,
            target_address,
            balance_after_initial_funding,
            U256::from(DEFAULT_PRIORITY_FEE),
            false,
            "Target balance should decrease by priority fee only on UnstakeDebit",
        );

        Ok(())
    }

    /// Test pledge and unpledge transaction ordering
    #[test_log::test(tokio::test)]
    async fn test_pledge_unpledge_ordering() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Use a funded account
        let target_address = ctx.normal_signer.address();
        let initial_balance = get_balance(&node.inner, target_address);

        // Create multiple pledge and unpledge transactions
        let mut shadow_txs = Vec::new();
        let mut expected_tx_hashes = Vec::new();

        // 1. Pledge transaction
        let pledge_tx = pledge(target_address);
        let pledge_tx =
            sign_shadow_tx(pledge_tx, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;
        expected_tx_hashes.push(*pledge_tx.hash());
        shadow_txs.push(pledge_tx);

        // 2. Another pledge transaction
        let pledge_tx2 = pledge(target_address);
        let pledge_tx2 =
            sign_shadow_tx(pledge_tx2, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;
        expected_tx_hashes.push(*pledge_tx2.hash());
        shadow_txs.push(pledge_tx2);

        // 3. Unpledge transaction
        let unpledge_tx = unpledge(target_address);
        let unpledge_tx =
            sign_shadow_tx(unpledge_tx, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;
        expected_tx_hashes.push(*unpledge_tx.hash());
        shadow_txs.push(unpledge_tx);

        // Mine block with all transactions
        let block_payload = mine_block(&mut node, shadow_txs).await?;

        // Verify all transactions are included in block in correct order
        assert_txs_in_block(
            &block_payload,
            &expected_tx_hashes,
            "Pledge/Unpledge transactions",
        );

        // Get the transaction hashes from the block in order
        let block_tx_hashes: Vec<_> = block_payload
            .block()
            .body()
            .transactions
            .iter()
            .map(|tx| *tx.hash())
            .collect();

        // Verify the transactions appear in the block in the same order as submitted
        for (i, expected_hash) in expected_tx_hashes.iter().enumerate() {
            assert_eq!(
                block_tx_hashes[i], *expected_hash,
                "Transaction at position {i} should match submitted order"
            );
        }

        // Calculate expected final balance accounting for priority fees
        // Each transaction costs DEFAULT_PRIORITY_FEE
        // Net operation: 2 pledge decrements (-2 wei) + 1 unpledge no-op (0 wei) = -2 wei
        // Total priority fees: 3 * DEFAULT_PRIORITY_FEE
        let total_priority_fees = U256::from(DEFAULT_PRIORITY_FEE) * U256::from(3);
        let net_operation_change = U256::from(2_u64); // Net decrease of 2 wei from operations
        let expected_final_balance = initial_balance - net_operation_change - total_priority_fees;

        let final_balance = get_balance(&node.inner, target_address);
        assert_eq!(
            final_balance, expected_final_balance,
            "Final balance should reflect net effect of pledge/unpledge operations plus priority fees"
        );

        Ok(())
    }

    /// Test pledge + unpledge_refund balances with zero-priority-fee refund
    #[test_log::test(tokio::test)]
    async fn test_pledge_unpledge_refund_balances() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Use a funded account
        let target_address = ctx.normal_signer.address();
        let initial_balance = get_balance(&node.inner, target_address);

        // Create transactions: pledge, pledge, unpledge_refund(1 wei)
        let pledge_tx1 = pledge(target_address);
        let pledge_tx1 =
            sign_shadow_tx(pledge_tx1, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;
        let pledge_tx2 = pledge(target_address);
        let pledge_tx2 =
            sign_shadow_tx(pledge_tx2, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;
        let refund_tx = unpledge_refund(target_address, U256::ONE);
        let refund_tx = sign_shadow_tx(refund_tx, &ctx.block_producer_a, 0).await?; // zero-priority fee on refunds

        let expected_tx_hashes = vec![*pledge_tx1.hash(), *pledge_tx2.hash(), *refund_tx.hash()];

        // Mine block with all transactions
        let block_payload = mine_block(&mut node, vec![pledge_tx1, pledge_tx2, refund_tx]).await?;

        // Verify all transactions are included in block in correct order
        assert_txs_in_block(
            &block_payload,
            &expected_tx_hashes,
            "Pledge/UnpledgeRefund transactions",
        );

        // Calculate expected final balance
        // Operation effect: -1 (pledge) -1 (pledge) +1 (refund) = -1 wei
        // Priority fees: 2 * DEFAULT_PRIORITY_FEE (refund has zero priority fee)
        let total_priority_fees = U256::from(DEFAULT_PRIORITY_FEE) * U256::from(2);
        let net_operation_change = U256::ONE; // net -1 wei
        let expected_final_balance = initial_balance - net_operation_change - total_priority_fees;

        let final_balance = get_balance(&node.inner, target_address);
        assert_eq!(
            final_balance, expected_final_balance,
            "Final balance should reflect pledge/refund operations plus priority fees"
        );

        Ok(())
    }

    /// Test unpledge on non-existent account fails block production
    #[test_log::test(tokio::test)]
    async fn test_unpledge_nonexistent_account() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Create a random address that has never existed on chain
        let nonexistent_address = Address::random();

        // Verify the account doesn't exist
        let account = node
            .inner
            .provider
            .basic_account(&nonexistent_address)
            .unwrap();
        assert!(account.is_none(), "Test account should not exist");

        // Create unpledge transaction for non-existent account (priority-fee debit should fail)
        let unpledge_tx = unpledge(nonexistent_address);
        let unpledge_tx =
            sign_shadow_tx(unpledge_tx, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;

        // Attempt to produce a new block - this should fail because the unpledge transaction
        // tries to deduct priority fees from a non-existent account

        // Set up the payload attributes with shadow txs
        node.payload.timestamp += 1;
        let base_attributes = (node.payload.attributes_generator)(node.payload.timestamp);
        let attributes = IrysPayloadBuilderAttributes {
            inner: base_attributes.inner,
            id: base_attributes.id,
            shadow_txs: vec![unpledge_tx],
        };

        // Send the payload - this will trigger block building
        node.payload
            .payload_builder
            .send_new_payload(attributes.clone())
            .await
            .unwrap()?;

        // Wait briefly for the payload builder to process
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Try to get the payload - this should return an error
        let result = node
            .payload
            .payload_builder
            .best_payload(attributes.payload_id())
            .await;

        // Verify the payload build failed with an error
        let err = result
            .expect("Expected Some result from best_payload")
            .expect_err(
                "Block production should have failed due to non-existent account for unpledge",
            );

        let error_msg = format!("{err:?}");
        assert!(
            error_msg.contains("Shadow transaction priority fee failed"),
            "Expected shadow transaction priority fee failure for non-existent account, got: {error_msg}"
        );

        Ok(())
    }

    /// Test pledge on non-existent account fails
    #[test_log::test(tokio::test)]
    async fn test_pledge_nonexistent_account() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Create a random address that has never existed on chain
        let nonexistent_address = Address::random();

        // Verify the account doesn't exist
        let account = node
            .inner
            .provider
            .basic_account(&nonexistent_address)
            .unwrap();
        assert!(account.is_none(), "Test account should not exist");

        // Create pledge transaction for non-existent account
        let pledge_tx = ShadowTransaction::new_v1(
            TransactionPacket::Pledge(BalanceDecrement {
                amount: U256::ONE,
                target: nonexistent_address,
                irys_ref: alloy_primitives::FixedBytes::ZERO,
            }),
            alloy_primitives::FixedBytes::ZERO,
        );
        let pledge_tx =
            sign_shadow_tx(pledge_tx, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;

        // Submit a normal transaction to ensure there's something in the mempool
        let _normal_tx_hash = create_and_submit_normal_tx(
            &mut node,
            0,
            U256::from(1000),
            1_000_000_000_u128,
            Address::random(),
            &ctx.normal_signer,
        )
        .await?;

        // Attempt to produce a new block - this should fail because the pledge transaction
        // tries to deduct priority fees from a non-existent account

        // Set up the payload attributes with shadow txs
        node.payload.timestamp += 1;
        let base_attributes = (node.payload.attributes_generator)(node.payload.timestamp);
        let attributes = IrysPayloadBuilderAttributes {
            inner: base_attributes.inner,
            id: base_attributes.id,
            shadow_txs: vec![pledge_tx],
        };

        // Send the payload - this will trigger block building
        node.payload
            .payload_builder
            .send_new_payload(attributes.clone())
            .await
            .unwrap()?;

        // Wait briefly for the payload builder to process
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Try to get the payload - this should return an error
        let result = node
            .payload
            .payload_builder
            .best_payload(attributes.payload_id())
            .await;

        // Verify the payload build failed with an error
        let err = result
            .expect("Expected Some result from best_payload")
            .expect_err(
                "Block production should have failed due to non-existent account for pledge",
            );

        let error_msg = format!("{err:?}");
        assert!(
            error_msg.contains("Shadow transaction priority fee failed"),
            "Expected shadow transaction priority fee failure for non-existent account, got: {error_msg}"
        );

        Ok(())
    }

    /// Test that shadow transactions with priority fees distribute fees to beneficiary
    #[test_log::test(tokio::test)]
    async fn test_shadow_tx_priority_fee_distribution() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Get initial balances
        let beneficiary = ctx.block_producer_a.address(); // Producer A is the beneficiary for node 0
        let target_address = ctx.target_account.address();

        // Fund the target account first to ensure it can pay priority fees
        let funding_amount = U256::from(100_000_000_000_u128); // 100 Gwei
        let fund_tx = ShadowTransaction::new_v1(
            TransactionPacket::BlockReward(BlockRewardIncrement {
                amount: funding_amount,
            }),
            alloy_primitives::FixedBytes::ZERO,
        );
        let fund_tx = sign_shadow_tx(fund_tx, &ctx.block_producer_a, 0).await?; // Block rewards must have 0 priority fee
        mine_block(&mut node, vec![fund_tx]).await?;

        let initial_beneficiary_balance = get_balance(&node.inner, beneficiary);
        let initial_target_balance = get_balance(&node.inner, target_address);

        // Create shadow transaction with significant priority fee
        let priority_fee_per_gas = 10_000_000_000_u128; // 10 Gwei
        // Use unstake instead of block_reward so there's a target for priority fee
        let shadow_tx = unstake(target_address);
        let shadow_tx_raw = compose_shadow_tx(1, &shadow_tx, priority_fee_per_gas);
        let expected_priority_fee = U256::from(priority_fee_per_gas);

        // Sign and prepare the transaction using the helper (signed by block producer)
        let shadow_tx_pooled = sign_tx(shadow_tx_raw, &ctx.block_producer_a).await;

        // Mine block with shadow transaction
        let _block_payload = mine_block(&mut node, vec![shadow_tx_pooled]).await?;

        // Verify beneficiary received the priority fee
        assert_balance_change(
            &node,
            beneficiary,
            initial_beneficiary_balance,
            expected_priority_fee,
            true,
            "Beneficiary should receive priority fee",
        );

        // Verify target balance changed: received unstake amount (1 wei) minus priority fee
        let unstake_amount = U256::from(1); // unstake gives 1 wei
        let net_decrease = expected_priority_fee - unstake_amount; // Fee is larger than unstake amount
        assert_balance_change(
            &node,
            target_address,
            initial_target_balance,
            net_decrease,
            false, // This is a decrease
            "Target balance should decrease by net of priority fee minus unstake amount",
        );

        Ok(())
    }

    /// Test multiple shadow transactions accumulate priority fees correctly
    #[test_log::test(tokio::test)]
    async fn test_multiple_shadow_tx_priority_fees() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        let beneficiary = ctx.block_producer_a.address(); // Producer A is the beneficiary for node 0
        let target_address = ctx.target_account.address();

        // Fund the target account first to ensure it can pay priority fees
        let funding_amount = U256::from(100_000_000_000_u128); // 100 Gwei
        let fund_tx = ShadowTransaction::new_v1(
            TransactionPacket::BlockReward(BlockRewardIncrement {
                amount: funding_amount,
            }),
            alloy_primitives::FixedBytes::ZERO,
        );
        let fund_tx = sign_shadow_tx(fund_tx, &ctx.block_producer_a, 0).await?; // Block rewards must have 0 priority fee
        mine_block(&mut node, vec![fund_tx]).await?;

        let initial_beneficiary_balance = get_balance(&node.inner, beneficiary);
        let initial_target_balance = get_balance(&node.inner, target_address);

        // Create multiple shadow transactions with different priority fees
        let mut shadow_txs = Vec::new();
        let mut total_expected_fee = U256::ZERO;

        // Mix transaction types - some with targets (unstake) and some without (block_reward)
        // Transaction 1: Unstake (has target, so priority fee is distributed)
        let priority_fee_1 = 1_000_000_000_u128; // 1 Gwei
        let shadow_tx_1 = unstake(target_address);
        let shadow_tx_raw_1 = compose_shadow_tx(1, &shadow_tx_1, priority_fee_1);
        total_expected_fee += U256::from(priority_fee_1);
        let shadow_tx_pooled_1 = sign_tx(shadow_tx_raw_1, &ctx.block_producer_a).await;
        shadow_txs.push(shadow_tx_pooled_1);

        // Transaction 2: Block reward (must have 0 priority fee)
        let priority_fee_2 = 0; // Block rewards must have 0 priority fee
        let shadow_tx_2 = block_reward();
        let shadow_tx_raw_2 = compose_shadow_tx(1, &shadow_tx_2, priority_fee_2);
        // Block rewards with non-zero priority fees are now rejected
        let shadow_tx_pooled_2 = sign_tx(shadow_tx_raw_2, &ctx.block_producer_a).await;
        shadow_txs.push(shadow_tx_pooled_2);

        // Transaction 3: Stake (has target, so priority fee is distributed)
        let priority_fee_3 = 3_000_000_000_u128; // 3 Gwei
        let shadow_tx_3 = stake(target_address);
        let shadow_tx_raw_3 = compose_shadow_tx(1, &shadow_tx_3, priority_fee_3);
        total_expected_fee += U256::from(priority_fee_3);
        let shadow_tx_pooled_3 = sign_tx(shadow_tx_raw_3, &ctx.block_producer_a).await;
        shadow_txs.push(shadow_tx_pooled_3);

        // Mine block with all shadow transactions
        let _block_payload = mine_block(&mut node, shadow_txs).await?;

        // Verify beneficiary received only fees from transactions with targets
        // Only unstake (1 Gwei) and stake (3 Gwei) have targets, so total = 4 Gwei
        let expected_beneficiary_fee = U256::from(priority_fee_1) + U256::from(priority_fee_3);
        assert_balance_change(
            &node,
            beneficiary,
            initial_beneficiary_balance,
            expected_beneficiary_fee + U256::from(1), // fees + block reward amount
            true,
            "Beneficiary should receive fees only from transactions with targets plus block reward",
        );

        // Verify target balance changed by net of operations and fees
        // Target receives: +1 wei (unstake) -1 wei (stake) -4 Gwei (fees) = -4 Gwei total
        let net_target_change = U256::from(4_000_000_000_u64); // 4 Gwei deducted
        assert_balance_change(
            &node,
            target_address,
            initial_target_balance,
            net_target_change,
            false, // This is a decrease
            "Target should pay priority fees for transactions where it's the target",
        );

        Ok(())
    }

    /// Test shadow tx priority fees go to different miner than tx signer
    /// (This is to enable mining pools in the future, where a beneficiary
    /// of a block may be a different entity that actually mined the block)
    #[test_log::test(tokio::test)]
    async fn test_shadow_tx_priority_fee_different_miner() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        // Get node 1 (second node) which has producer B as beneficiary
        let ((mut _node_a, mut node), ctx) = ctx.get_two_nodes()?;

        let miner_address = ctx.block_producer_b.address(); // Producer B is the beneficiary for node 1

        // Get initial balances
        let initial_miner_balance = get_balance(&node.inner, miner_address);
        let initial_signer_balance = get_balance(&node.inner, ctx.block_producer_a.address());

        // Create shadow transaction with priority fee
        let priority_fee_per_gas = 20_000_000_000_u128; // 20 Gwei
        // Use stake transaction which has a target
        let target_address = ctx.block_producer_a.address(); // Producer A is the target
        let shadow_tx = stake(target_address);
        let shadow_tx_raw = compose_shadow_tx(1, &shadow_tx, priority_fee_per_gas);
        let expected_priority_fee = U256::from(priority_fee_per_gas);

        // Sign with block producer A (the target, not the miner)
        let shadow_tx_pooled = sign_tx(shadow_tx_raw, &ctx.block_producer_a).await;

        // Mine block
        let _block_payload = mine_block(&mut node, vec![shadow_tx_pooled]).await?;

        // Verify miner (producer B) received the priority fee
        let expected_total = expected_priority_fee; // priority fee only
        assert_balance_change(
            &node,
            miner_address,
            initial_miner_balance,
            expected_total,
            true,
            "Miner (block producer B) should receive priority fee",
        );

        // Verify signer (producer A) balance decreased by stake amount + priority fee
        let stake_amount = U256::from(1); // stake decrements 1 wei
        let total_decrease = stake_amount + expected_priority_fee;
        assert_balance_change(
            &node,
            ctx.block_producer_a.address(),
            initial_signer_balance,
            total_decrease,
            false, // This is a decrease
            "Signer (block producer A) balance should decrease by stake amount plus priority fee",
        );

        Ok(())
    }

    /// Test that block reward transactions with non-zero priority fees are rejected
    #[test_log::test(tokio::test)]
    async fn test_block_reward_rejects_priority_fee() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Create a block reward transaction with a non-zero priority fee
        let invalid_shadow_tx = block_reward();
        let priority_fee_per_gas = 1_000_000_000_u128; // 1 Gwei (should be rejected)
        let invalid_shadow_tx_raw = compose_shadow_tx(1, &invalid_shadow_tx, priority_fee_per_gas);

        // Sign the invalid transaction
        let invalid_shadow_tx_pooled = sign_tx(invalid_shadow_tx_raw, &ctx.block_producer_a).await;

        // Also create a valid transaction so the block can be produced
        // Using unstake which increments balance (so we don't need to fund the account first)
        let valid_tx = unstake(ctx.target_account.address());
        let valid_tx_raw = compose_shadow_tx(1, &valid_tx, 0); // 0 priority fee
        let valid_tx_pooled = sign_tx(valid_tx_raw, &ctx.block_producer_a).await;

        // Record initial balances
        let beneficiary = ctx.block_producer_a.address();
        let target_address = ctx.target_account.address();
        let initial_beneficiary_balance = get_balance(&node.inner, beneficiary);
        let initial_target_balance = get_balance(&node.inner, target_address);

        // Try to mine block with both transactions (invalid block reward + valid stake)
        let block_payload = mine_block(
            &mut node,
            vec![invalid_shadow_tx_pooled.clone(), valid_tx_pooled.clone()],
        )
        .await?;

        // Verify the invalid transaction was not included in the block
        let invalid_tx_hash = invalid_shadow_tx_pooled.hash();
        let valid_tx_hash = valid_tx_pooled.hash();
        let block_tx_hashes: HashSet<_> = block_payload
            .block()
            .body()
            .transactions
            .iter()
            .map(|tx| *tx.hash())
            .collect();
        assert!(
            !block_tx_hashes.contains(invalid_tx_hash),
            "Block reward transaction with priority fee should not be included in block"
        );
        assert!(
            block_tx_hashes.contains(valid_tx_hash),
            "Valid unstake transaction should be included in block"
        );

        // Verify balances after the block
        let final_beneficiary_balance = get_balance(&node.inner, beneficiary);
        let final_target_balance = get_balance(&node.inner, target_address);

        // Beneficiary balance should not change (no block reward from rejected tx, no fee on unstake with 0 priority fee)
        assert_eq!(
            final_beneficiary_balance, initial_beneficiary_balance,
            "Beneficiary balance should not change"
        );

        // Target balance should increase by 1 (unstake increments balance)
        assert_eq!(
            final_target_balance - initial_target_balance,
            U256::from(1),
            "Target balance should increase by unstake amount"
        );

        // Now test that a block reward with 0 priority fee works correctly
        let valid_shadow_tx = block_reward();
        let valid_shadow_tx_raw = compose_shadow_tx(1, &valid_shadow_tx, 0); // 0 priority fee, chain ID 1
        let valid_shadow_tx_pooled = sign_tx(valid_shadow_tx_raw, &ctx.block_producer_a).await;

        // Mine block with valid block reward transaction
        let _block_payload = mine_block(&mut node, vec![valid_shadow_tx_pooled]).await?;

        // Verify the block reward was applied
        let final_balance_after_valid = get_balance(&node.inner, beneficiary);
        assert_eq!(
            final_balance_after_valid - final_beneficiary_balance,
            U256::ONE, // Block reward amount is 1 wei
            "Beneficiary should receive block reward from valid shadow tx with 0 priority fee"
        );

        Ok(())
    }

    /// Test that all shadow transaction types (except block rewards) properly distribute priority fees
    #[test_log::test(tokio::test)]
    async fn test_all_shadow_tx_types_priority_fees() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        let beneficiary = ctx.block_producer_a.address();
        let target_address = ctx.target_account.address();

        // Fund the target account first
        let funding_amount = U256::from(1_000_000_000_000_u128); // 1000 Gwei
        let fund_tx = ShadowTransaction::new_v1(
            TransactionPacket::BlockReward(BlockRewardIncrement {
                amount: funding_amount,
            }),
            alloy_primitives::FixedBytes::ZERO,
        );
        let fund_tx = sign_shadow_tx(fund_tx, &ctx.block_producer_a, 0).await?; // Block rewards must have 0 priority fee
        mine_block(&mut node, vec![fund_tx]).await?;

        let initial_beneficiary_balance = get_balance(&node.inner, beneficiary);
        let initial_target_balance = get_balance(&node.inner, target_address);

        // Test each transaction type with priority fees
        let mut shadow_txs = Vec::new();
        let mut total_expected_fee = U256::ZERO;

        // 1. Stake (decrements balance, has target)
        let priority_fee_stake = 1_000_000_000_u128; // 1 Gwei
        let stake_tx = stake(target_address);
        let stake_tx_raw = compose_shadow_tx(1, &stake_tx, priority_fee_stake);
        total_expected_fee += U256::from(priority_fee_stake);
        shadow_txs.push(sign_tx(stake_tx_raw, &ctx.block_producer_a).await);

        // 2. Unstake (increments balance, has target)
        let priority_fee_unstake = 2_000_000_000_u128; // 2 Gwei
        let unstake_tx = unstake(target_address);
        let unstake_tx_raw = compose_shadow_tx(1, &unstake_tx, priority_fee_unstake);
        total_expected_fee += U256::from(priority_fee_unstake);
        shadow_txs.push(sign_tx(unstake_tx_raw, &ctx.block_producer_a).await);

        // 3. Pledge (decrements balance, has target)
        let priority_fee_pledge = 3_000_000_000_u128; // 3 Gwei
        let pledge_tx = pledge(target_address);
        let pledge_tx_raw = compose_shadow_tx(1, &pledge_tx, priority_fee_pledge);
        total_expected_fee += U256::from(priority_fee_pledge);
        shadow_txs.push(sign_tx(pledge_tx_raw, &ctx.block_producer_a).await);

        // 4. Unpledge (increments balance, has target)
        let priority_fee_unpledge = 4_000_000_000_u128; // 4 Gwei
        let unpledge_tx = unpledge(target_address);
        let unpledge_tx_raw = compose_shadow_tx(1, &unpledge_tx, priority_fee_unpledge);
        total_expected_fee += U256::from(priority_fee_unpledge);
        shadow_txs.push(sign_tx(unpledge_tx_raw, &ctx.block_producer_a).await);

        // 5. Storage fees (decrements balance, has target)
        let priority_fee_storage = 5_000_000_000_u128; // 5 Gwei
        let storage_tx = storage_fees(target_address);
        let storage_tx_raw = compose_shadow_tx(1, &storage_tx, priority_fee_storage);
        total_expected_fee += U256::from(priority_fee_storage);
        shadow_txs.push(sign_tx(storage_tx_raw, &ctx.block_producer_a).await);

        // Mine block with all shadow transactions
        let _block_payload = mine_block(&mut node, shadow_txs).await?;

        // Verify beneficiary received all priority fees
        // Total fees: 1 + 2 + 3 + 4 + 5 = 15 Gwei
        assert_balance_change(
            &node,
            beneficiary,
            initial_beneficiary_balance,
            total_expected_fee,
            true,
            "Beneficiary should receive all priority fees from transactions with targets",
        );

        // Verify target balance changed correctly
        // Net operations: -1 (stake) +1 (unstake) -1 (pledge) +0 (unpledge) -1 (storage) = -2 wei
        // Plus paying all the priority fees: -15 Gwei
        let net_operations = U256::from(2); // Net decrease of 2 wei from operations
        let total_decrease = net_operations + total_expected_fee;
        assert_balance_change(
            &node,
            target_address,
            initial_target_balance,
            total_decrease,
            false, // This is a decrease
            "Target should pay all priority fees and have net operation decrease",
        );

        Ok(())
    }

    #[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
    async fn can_trace_normal_tx() -> eyre::Result<()> {
        // setup
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        let normal_tx_hash = create_and_submit_normal_tx(
            &mut node,
            0,
            U256::from(1000),
            1_000_000_000_u128,
            Address::random(),
            &ctx.normal_signer,
        )
        .await?;

        let payload_node_a = advance_block(&mut node, vec![]).await?;

        assert_txs_in_block(
            &payload_node_a,
            &[normal_tx_hash],
            "expected regular tx to be included",
        );

        let debug_api = node.rpc.inner.debug_api();

        let res = debug_api
            .debug_trace_transaction(
                normal_tx_hash,
                alloy_rpc_types_trace::geth::GethDebugTracingOptions::new_tracer(
                    alloy_rpc_types_trace::geth::GethDebugBuiltInTracerType::CallTracer,
                ),
            )
            .await;

        assert!(res.is_ok(), "unexpected error tracing tx {:?}", &res);

        Ok(())
    }
}

#[cfg(any(feature = "test-utils", test))]
/// Test Utilities for Irys Reth node
pub mod test_utils {
    use super::*;
    use crate::shadow_tx::{ShadowTransaction, TransactionPacket};
    use alloy_consensus::EthereumTxEnvelope;
    use alloy_consensus::{SignableTransaction as _, TxEip1559, TxEip4844, TxLegacy};
    use alloy_genesis::Genesis;
    use alloy_network::EthereumWallet;
    use alloy_network::TxSigner;
    use alloy_primitives::Address;
    use alloy_primitives::{B256, FixedBytes, Signature};
    use alloy_primitives::{TxKind, U256};
    use alloy_rpc_types::engine::PayloadAttributes;
    use reth::providers::CanonStateSubscriptions;
    use reth_payload_primitives::PayloadBuilderAttributes as _;

    /// Default priority fee for shadow transactions in tests (1 Gwei)
    pub const DEFAULT_PRIORITY_FEE: u128 = 1_000_000_000;
    use reth::{
        api::PayloadAttributesBuilder,
        args::{DiscoveryArgs, NetworkArgs, RpcServerArgs},
        builder::{FullNode, NodeBuilder, NodeConfig, NodeHandle, rpc::RethRpcAddOns},
        providers::{AccountReader as _, BlockHashReader as _},
        rpc::api::eth::helpers::EthTransactions,
        tasks::Runtime,
    };
    use reth_e2e_test_utils::{NodeHelperType, node::NodeTestContext, wallet::Wallet};
    use reth_engine_local::LocalPayloadAttributesBuilder;
    use reth_primitives_traits::SignedTransaction as _;
    use reth_transaction_pool::{PoolTransaction as _, TransactionPool as _};
    use std::collections::HashSet;
    use std::sync::Arc;
    use tracing::{Level, span};

    /// Common setup for tests - creates wallets, nodes, and returns initialized context
    pub struct TestContext {
        pub nodes: Vec<NodeHelperType<IrysEthereumNode>>,
        pub block_producer_a: Arc<dyn TxSigner<Signature> + Send + Sync>,
        pub block_producer_b: Arc<dyn TxSigner<Signature> + Send + Sync>,
        pub normal_signer: Arc<dyn TxSigner<Signature> + Send + Sync>,
        pub target_account: Arc<dyn TxSigner<Signature> + Send + Sync>,
        pub genesis_blockhash: FixedBytes<32>,
        pub tasks: Runtime,
    }

    impl std::fmt::Debug for TestContext {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "TestContext")
        }
    }

    impl TestContext {
        pub async fn new() -> eyre::Result<Self> {
            Self::new_with_custom_attributes(eth_payload_attributes_with_beneficiary).await
        }

        pub async fn new_with_custom_attributes(
            payload_attributes: impl Fn(u64, Address) -> IrysPayloadBuilderAttributes
            + Send
            + Sync
            + Clone
            + 'static,
        ) -> eyre::Result<Self> {
            let wallets = Wallet::new(4).wallet_gen();
            let block_producer_a = EthereumWallet::from(wallets[0].clone()).default_signer();
            let block_producer_b = EthereumWallet::from(wallets[1].clone()).default_signer();
            let normal_signer = EthereumWallet::from(wallets[2].clone()).default_signer();
            let target_account = EthereumWallet::from(wallets[3].clone()).default_signer();

            let block_producer_addresses =
                vec![block_producer_a.address(), block_producer_b.address()];
            let (nodes, tasks, ..) = setup_irys_reth(
                &block_producer_addresses,
                custom_chain(),
                false,
                payload_attributes,
            )
            .await?;

            let genesis_blockhash = nodes
                .first()
                .unwrap()
                .inner
                .provider
                .consistent_provider()
                .unwrap()
                .block_hash(0)
                .unwrap()
                .unwrap();

            Ok(Self {
                tasks,
                nodes,
                block_producer_a,
                block_producer_b,
                normal_signer,
                target_account,
                genesis_blockhash,
            })
        }

        pub fn get_single_node(mut self) -> eyre::Result<(NodeHelperType<IrysEthereumNode>, Self)> {
            if self.nodes.is_empty() {
                return Err(eyre::eyre!("No nodes available"));
            }
            let node = self.nodes.remove(0);
            Ok((node, self))
        }

        pub fn get_two_nodes(
            mut self,
        ) -> eyre::Result<(
            (
                NodeHelperType<IrysEthereumNode>,
                NodeHelperType<IrysEthereumNode>,
            ),
            Self,
        )> {
            if self.nodes.len() < 2 {
                return Err(eyre::eyre!("Need at least 2 nodes"));
            }
            let second = self.nodes.pop().unwrap();
            let first = self.nodes.pop().unwrap();
            Ok(((first, second), self))
        }
    }

    /// Helper for creating and signing shadow transactions with configurable priority fee.
    pub async fn sign_shadow_tx(
        shadow_tx: ShadowTransaction,
        signer: &Arc<dyn TxSigner<Signature> + Send + Sync>,
        max_priority_fee_per_gas: u128,
    ) -> eyre::Result<EthPooledTransaction> {
        let shadow_tx_raw = compose_shadow_tx(1, &shadow_tx, max_priority_fee_per_gas);
        let shadow_pooled_tx = sign_tx(shadow_tx_raw, signer).await;
        Ok(shadow_pooled_tx)
    }

    /// Helper for creating and submitting normal transactions
    pub async fn create_and_submit_normal_tx(
        node: &mut NodeHelperType<IrysEthereumNode>,
        nonce: u64,
        value: U256,
        gas_price: u128,
        recipient: Address,
        signer: &Arc<dyn TxSigner<Signature> + Send + Sync>,
    ) -> eyre::Result<alloy_primitives::FixedBytes<32>> {
        let mut normal_tx_raw = TxLegacy {
            gas_limit: 99000,
            value,
            nonce,
            gas_price,
            chain_id: Some(1),
            input: vec![123].into(),
            to: TxKind::Call(recipient),
        };
        let signed_normal = signer.sign_transaction(&mut normal_tx_raw).await.unwrap();
        let normal_tx =
            EthereumTxEnvelope::<TxEip4844>::Legacy(normal_tx_raw.into_signed(signed_normal))
                .try_into_recovered()
                .unwrap();
        let normal_pooled_tx = EthPooledTransaction::new(normal_tx.clone(), 300);
        let outcome = node
            .inner
            .pool
            .add_transaction(
                reth_transaction_pool::TransactionOrigin::Local,
                normal_pooled_tx,
            )
            .await?;
        Ok(outcome.hash)
    }

    /// Helper for creating multiple normal transactions
    pub async fn create_and_submit_multiple_normal_txs(
        node: &mut NodeHelperType<IrysEthereumNode>,
        count: u64,
        start_nonce: u64,
        gas_price: u128,
        recipient: Address,
        signer: &Arc<dyn TxSigner<Signature> + Send + Sync>,
    ) -> eyre::Result<Vec<alloy_primitives::FixedBytes<32>>> {
        let mut tx_hashes = Vec::new();
        for i in 0..count {
            let tx_hash = create_and_submit_normal_tx(
                node,
                start_nonce + i,
                U256::from(1234_u64),
                gas_price,
                recipient,
                signer,
            )
            .await?;
            tx_hashes.push(tx_hash);
        }
        Ok(tx_hashes)
    }

    /// Helper for asserting transaction inclusion in blocks
    pub fn assert_txs_in_block(
        block_payload: &EthBuiltPayload,
        expected_txs: &[alloy_primitives::FixedBytes<32>],
        message: &str,
    ) {
        let block_txs: HashSet<_> = block_payload
            .block()
            .body()
            .transactions
            .iter()
            .map(|tx| *tx.hash())
            .collect();

        for tx_hash in expected_txs {
            assert!(
                block_txs.contains(tx_hash),
                "{message}: Transaction {tx_hash:?} not found in block"
            );
        }
    }

    /// Helper for asserting transaction exclusion from blocks
    pub fn assert_txs_not_in_block(
        block_payload: &EthBuiltPayload,
        excluded_txs: &[alloy_primitives::FixedBytes<32>],
        message: &str,
    ) {
        let block_txs: HashSet<_> = block_payload
            .block()
            .body()
            .transactions
            .iter()
            .map(|tx| *tx.hash())
            .collect();

        for tx_hash in excluded_txs {
            assert!(
                !block_txs.contains(tx_hash),
                "{message}: Transaction {tx_hash:?} should not be in block"
            );
        }
    }

    /// Helper for asserting transaction ordering in blocks
    pub fn assert_shadow_txs_before_normal_txs(
        block_payload: &EthBuiltPayload,
        shadow_txs: &[alloy_primitives::FixedBytes<32>],
        normal_txs: &[alloy_primitives::FixedBytes<32>],
    ) {
        let block_txs: Vec<_> = block_payload
            .block()
            .body()
            .transactions
            .iter()
            .map(|tx| *tx.hash())
            .collect();

        let mut last_shadow_tx_pos = 0;
        let mut first_normal_tx_pos = block_txs.len();

        for (pos, tx_hash) in block_txs.iter().enumerate() {
            if shadow_txs.contains(tx_hash) {
                last_shadow_tx_pos = last_shadow_tx_pos.max(pos);
            }
            if normal_txs.contains(tx_hash) {
                first_normal_tx_pos = first_normal_tx_pos.min(pos);
            }
        }

        assert!(
            last_shadow_tx_pos < first_normal_tx_pos,
            "Shadow transactions should appear before normal transactions. Last shadow: {last_shadow_tx_pos}, First normal: {first_normal_tx_pos}"
        );
    }

    /// Helper for asserting balance changes
    #[track_caller]
    pub fn assert_balance_change(
        node: &NodeHelperType<IrysEthereumNode>,
        address: Address,
        initial_balance: U256,
        expected_change: U256,
        is_increment: bool,
        message: &str,
    ) {
        let final_balance = get_balance(&node.inner, address);
        let expected_balance = if is_increment {
            initial_balance + expected_change
        } else {
            initial_balance - expected_change
        };

        assert_eq!(
            final_balance, expected_balance,
            "{message}: Expected balance {expected_balance}, got {final_balance}"
        );
    }

    /// Helper for asserting nonce values
    pub fn assert_nonce(
        node: &NodeHelperType<IrysEthereumNode>,
        address: Address,
        expected_nonce: u64,
        message: &str,
    ) {
        let actual_nonce = get_nonce(&node.inner, address);
        assert_eq!(
            actual_nonce, expected_nonce,
            "{message}: Expected nonce {expected_nonce}, got {actual_nonce}"
        );
    }

    /// Helper for block mining and validation
    pub async fn mine_block(
        node: &mut NodeHelperType<IrysEthereumNode>,
        shadow_txs: Vec<EthPooledTransaction>,
    ) -> eyre::Result<EthBuiltPayload> {
        let block_payload = advance_block(node, shadow_txs).await?;
        Ok(block_payload)
    }

    /// Helper for block mining and validation
    pub async fn mine_block_and_validate(
        node: &mut NodeHelperType<IrysEthereumNode>,
        shadow_txs: Vec<EthPooledTransaction>,
        expected_normal_txs: &[alloy_primitives::FixedBytes<32>],
    ) -> eyre::Result<EthBuiltPayload> {
        let expected_shadow_tx_hashes = shadow_txs.iter().map(|tx| *tx.hash()).collect::<Vec<_>>();
        let block_payload = advance_block(node, shadow_txs).await?;

        assert_txs_in_block(
            &block_payload,
            &expected_shadow_tx_hashes,
            "Shadow transactions",
        );
        assert_txs_in_block(&block_payload, expected_normal_txs, "Normal transactions");

        if !expected_shadow_tx_hashes.is_empty() && !expected_normal_txs.is_empty() {
            assert_shadow_txs_before_normal_txs(
                &block_payload,
                &expected_shadow_tx_hashes,
                expected_normal_txs,
            );
        }

        Ok(block_payload)
    }

    /// Helper to create shadow transaction based on type
    pub fn create_shadow_tx(tx_type: u8, address: Address) -> ShadowTransaction {
        use crate::shadow_tx::*;
        match tx_type {
            BLOCK_REWARD_ID => block_reward(),
            UNSTAKE_ID => unstake(address),
            STAKE_ID => stake(address),
            STORAGE_FEES_ID => storage_fees(address),
            PLEDGE_ID => pledge(address),
            UNPLEDGE_ID => unpledge(address),
            _ => panic!("Unknown shadow transaction type: {tx_type}"),
        }
    }

    /// - prepare a new payload with shadow txs via attributes
    /// - DOES NOT update the forkchoice
    pub async fn prepare_block(
        node: &mut NodeHelperType<IrysEthereumNode>,
        shadow_txs: Vec<EthPooledTransaction>,
    ) -> Result<EthBuiltPayload, eyre::Error> {
        node.payload.timestamp += 1;
        // Get base attributes from generator
        let base_attributes = (node.payload.attributes_generator)(node.payload.timestamp);
        // Create attributes with shadow_txs included
        let attributes = IrysPayloadBuilderAttributes {
            inner: base_attributes.inner,
            id: base_attributes.id,
            shadow_txs,
        };
        node.payload
            .payload_builder
            .send_new_payload(attributes.clone())
            .await
            .unwrap()?;
        node.payload.expect_attr_event(attributes.clone()).await?;
        node.payload
            .wait_for_built_payload(attributes.payload_id())
            .await;
        let payload = node.payload.expect_built_payload().await?;
        node.submit_payload(payload.clone()).await?;
        Ok(payload)
    }

    pub async fn advance_block(
        node: &mut NodeHelperType<IrysEthereumNode>,
        shadow_txs: Vec<EthPooledTransaction>,
    ) -> Result<EthBuiltPayload, eyre::Error> {
        let payload = prepare_block(node, shadow_txs).await?;
        node.update_forkchoice(payload.block().hash(), payload.block().hash())
            .await?;

        Ok(payload)
    }

    pub async fn advance_blocks(
        node: &mut NodeHelperType<IrysEthereumNode>,
        shadow_txs: Vec<Vec<ShadowTransaction>>,
        signer: &Arc<dyn alloy_network::TxSigner<Signature> + Send + Sync>,
    ) -> Result<Vec<EthBuiltPayload>, eyre::Error> {
        let mut block_payloads = Vec::new();

        for shadow_txs_raw in shadow_txs {
            let mut shadow_txs = Vec::new();
            for shadow_tx in shadow_txs_raw {
                // Use the shadow tx directly since metadata fields are removed
                let updated_shadow_tx = match &shadow_tx {
                    ShadowTransaction::V1 {
                        packet,
                        solution_hash,
                    } => ShadowTransaction::new_v1(packet.clone(), *solution_hash),
                };

                // Block rewards must have 0 priority fee, others can use default
                let priority_fee = match &shadow_tx {
                    ShadowTransaction::V1 { packet, .. } => match packet {
                        TransactionPacket::BlockReward(_) => 0,
                        _ => DEFAULT_PRIORITY_FEE,
                    },
                };

                let shadow_tx = sign_shadow_tx(updated_shadow_tx, signer, priority_fee).await?;
                shadow_txs.push(shadow_tx);
            }

            let block_payload = advance_block(node, shadow_txs).await?;
            block_payloads.push(block_payload);
        }

        Ok(block_payloads)
    }

    /// Compose a shadow tx for unstaking.
    pub fn unstake(address: Address) -> ShadowTransaction {
        ShadowTransaction::new_v1(
            TransactionPacket::UnstakeRefund(shadow_tx::BalanceIncrement {
                amount: U256::ONE,
                target: address,
                irys_ref: alloy_primitives::FixedBytes::ZERO,
            }),
            alloy_primitives::FixedBytes::ZERO,
        )
    }

    /// Compose a shadow tx for unstake-debit (fee-only via priority fee).
    pub fn unstake_debit(address: Address) -> ShadowTransaction {
        ShadowTransaction::new_v1(
            TransactionPacket::UnstakeDebit(shadow_tx::UnstakeDebit {
                target: address,
                irys_ref: alloy_primitives::FixedBytes::ZERO,
            }),
            alloy_primitives::FixedBytes::ZERO,
        )
    }

    /// Compose a shadow tx for block reward.
    pub fn block_reward() -> ShadowTransaction {
        ShadowTransaction::new_v1(
            TransactionPacket::BlockReward(shadow_tx::BlockRewardIncrement { amount: U256::ONE }),
            alloy_primitives::FixedBytes::ZERO,
        )
    }

    /// Compose a shadow tx for staking.
    pub fn stake(address: Address) -> ShadowTransaction {
        ShadowTransaction::new_v1(
            TransactionPacket::Stake(shadow_tx::BalanceDecrement {
                amount: U256::ONE,
                target: address,
                irys_ref: alloy_primitives::FixedBytes::ZERO,
            }),
            alloy_primitives::FixedBytes::ZERO,
        )
    }

    /// Compose a shadow tx for storage fees.
    pub fn storage_fees(address: Address) -> ShadowTransaction {
        ShadowTransaction::new_v1(
            TransactionPacket::StorageFees(shadow_tx::BalanceDecrement {
                amount: U256::ONE,
                target: address,
                irys_ref: alloy_primitives::FixedBytes::ZERO,
            }),
            alloy_primitives::FixedBytes::ZERO,
        )
    }

    /// Compose a shadow tx for pledge.
    pub fn pledge(address: Address) -> ShadowTransaction {
        ShadowTransaction::new_v1(
            TransactionPacket::Pledge(shadow_tx::BalanceDecrement {
                amount: U256::ONE,
                target: address,
                irys_ref: alloy_primitives::FixedBytes::ZERO,
            }),
            alloy_primitives::FixedBytes::ZERO,
        )
    }

    /// Compose a shadow tx for unpledge (fee-only via priority fee).
    pub fn unpledge(address: Address) -> ShadowTransaction {
        ShadowTransaction::new_v1(
            TransactionPacket::Unpledge(shadow_tx::UnpledgeDebit {
                target: address,
                irys_ref: alloy_primitives::FixedBytes::ZERO,
            }),
            alloy_primitives::FixedBytes::ZERO,
        )
    }

    /// Compose a shadow tx for unpledge refund (epoch-only; zero priority fee expected).
    pub fn unpledge_refund(address: Address, amount: U256) -> ShadowTransaction {
        ShadowTransaction::new_v1(
            TransactionPacket::UnpledgeRefund(shadow_tx::BalanceIncrement {
                amount,
                target: address,
                irys_ref: alloy_primitives::FixedBytes::ZERO,
            }),
            alloy_primitives::FixedBytes::ZERO,
        )
    }

    /// Assert that a log topic is present in block execution receipts at least `desired_repetitions` times.
    pub fn assert_topic_present_in_logs(
        block_execution: reth::providers::ExecutionOutcome,
        storage_fees_topic: [u8; 32],
        desired_repetitions: u64,
    ) {
        let receipts = &block_execution.receipts;
        let mut storage_fees_receipt_count = 0;
        for block_receipt in receipts {
            for receipt in block_receipt {
                if receipt.logs.iter().any(|log| {
                    log.data
                        .topics()
                        .iter()
                        .any(|topic| topic == storage_fees_topic)
                }) {
                    storage_fees_receipt_count += 1;
                }
            }
        }
        assert!(
            storage_fees_receipt_count >= desired_repetitions,
            "Expected at least {desired_repetitions} receipts, found {storage_fees_receipt_count}",
        );
    }

    /// Get the balance of an address from a node.
    pub fn get_balance<N, AddOns>(
        node: &FullNode<N, AddOns>,
        addr: Address,
    ) -> alloy_primitives::Uint<256, 4>
    where
        N: FullNodeComponents<Provider: CanonStateSubscriptions>,
        AddOns: RethRpcAddOns<N, EthApi: EthTransactions>,
        N::Types: NodeTypes<Primitives: reth::api::NodePrimitives>,
    {
        node.provider
            .basic_account(&addr)
            .map(|account_info| account_info.map_or(U256::ZERO, |acc| acc.balance))
            .unwrap_or_else(|err| {
                tracing::warn!("Failed to get signer_b balance: {}", err);
                U256::ZERO
            })
    }

    /// Get the nonce of an address from a node.
    pub fn get_nonce<N, AddOns>(node: &FullNode<N, AddOns>, addr: Address) -> u64
    where
        N: FullNodeComponents<Provider: CanonStateSubscriptions>,
        AddOns: RethRpcAddOns<N, EthApi: EthTransactions>,
        N::Types: NodeTypes<Primitives: reth::api::NodePrimitives>,
    {
        node.provider
            .basic_account(&addr)
            .map(|account_info| account_info.map_or(0, |acc| acc.nonce))
            .unwrap_or_else(|err| {
                tracing::warn!("Failed to get nonce: {}", err);
                0
            })
    }

    /// Sign a legacy transaction with the provided signer.
    pub async fn sign_tx_legacy(
        mut tx_raw: TxLegacy,
        new_signer: &Arc<dyn alloy_network::TxSigner<Signature> + Send + Sync>,
    ) -> EthPooledTransaction<alloy_consensus::EthereumTxEnvelope<TxEip4844>> {
        let signed_tx = new_signer.sign_transaction(&mut tx_raw).await.unwrap();
        let tx = alloy_consensus::EthereumTxEnvelope::Legacy(tx_raw.into_signed(signed_tx))
            .try_into_recovered()
            .unwrap();

        EthPooledTransaction::new(tx, 300)
    }

    /// Sign an EIP-1559 transaction with the provided signer.
    pub async fn sign_tx(
        mut tx_raw: TxEip1559,
        new_signer: &Arc<dyn alloy_network::TxSigner<Signature> + Send + Sync>,
    ) -> EthPooledTransaction<alloy_consensus::EthereumTxEnvelope<TxEip4844>> {
        let signed_tx = new_signer.sign_transaction(&mut tx_raw).await.unwrap();
        let tx = alloy_consensus::EthereumTxEnvelope::Eip1559(tx_raw.into_signed(signed_tx))
            .try_into_recovered()
            .unwrap();

        EthPooledTransaction::new(tx, 300)
    }

    /// Returns a custom chain spec for testing.
    pub fn custom_chain() -> Arc<ChainSpec> {
        let custom_genesis = r#"
{
  "config": {
    "chainId": 1,
    "homesteadBlock": 0,
    "daoForkSupport": true,
    "eip150Block": 0,
    "eip155Block": 0,
    "eip158Block": 0,
    "byzantiumBlock": 0,
    "constantinopleBlock": 0,
    "petersburgBlock": 0,
    "istanbulBlock": 0,
    "muirGlacierBlock": 0,
    "berlinBlock": 0,
    "londonBlock": 0,
    "arrowGlacierBlock": 0,
    "grayGlacierBlock": 0,
    "shanghaiTime": 0,
    "cancunTime": 0,
    "terminalTotalDifficulty": "0x0",
    "terminalTotalDifficultyPassed": true
  },
  "nonce": "0x0",
  "timestamp": "0x0",
  "extraData": "0x00",
  "gasLimit": "0x1c9c380",
  "difficulty": "0x0",
  "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "coinbase": "0x0000000000000000000000000000000000000000",
  "alloc": {
    "0x14dc79964da2c08b23698b3d3cc7ca32193d9955": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x15d34aaf54267db7d7c367839aaf71a00a2c6a65": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x1cbd3b2770909d4e10f157cabc84c7264073c9ec": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x23618e81e3f5cdf7f54c3d65f7fbc0abf5b21e8f": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x2546bcd3c84621e976d8185a91a922ae77ecec30": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x70997970c51812dc3a010c7d01b50e0d17dc79c8": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x71be63f3384f5fb98995898a86b02fb2426c5788": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x8626f6940e2eb28930efb4cef49b2d1f2c9c1199": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x90f79bf6eb2c4f870365e785982e1f101e93b906": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x976ea74026e726554db657fa54763abd0c3a0aa9": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x9965507d1a55bcc2695c58ba16fb37d819b0a4dc": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x9c41de96b2088cdc640c6182dfcf5491dc574a57": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0xa0ee7a142d267c1f36714e4a8f75612f20a79720": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0xbcd4042de499d14e55001ccbb24a551f3b954096": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0xbda5747bfd65f08deb54cb465eb87d40e51b197e": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0xcd3b766ccdd6ae721141f452c550ca635964ce71": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0xdd2fd4581271e230360230f9337d5c0430bf44c0": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0xdf3e18d64bc6a983f673ab319ccae4f1a57c7097": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0xfabb0ac9d68b0b445fb7357272ff202c5651694a": {
      "balance": "0xd3c21bcecceda1000000"
    }
  },
  "number": "0x0"
}
"#;
        let genesis: Genesis = serde_json::from_str(custom_genesis).unwrap();
        Arc::new(genesis.into())
    }

    /// Returns payload attributes for a given timestamp.
    pub fn eth_payload_attributes(timestamp: u64) -> IrysPayloadBuilderAttributes {
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

    /// Returns payload attributes with a custom beneficiary address.
    pub fn eth_payload_attributes_with_beneficiary(
        timestamp: u64,
        beneficiary: Address,
    ) -> IrysPayloadBuilderAttributes {
        let rpc_attributes = IrysPayloadAttributes {
            inner: PayloadAttributes {
                timestamp,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: beneficiary,
                withdrawals: Some(vec![]),
                parent_beacon_block_root: Some(B256::ZERO),
            },
            shadow_txs: vec![],
        };
        IrysPayloadBuilderAttributes::try_new(B256::ZERO, rpc_attributes, 0)
            .expect("IrysPayloadBuilderAttributes::try_new is infallible")
    }

    /// Returns payload attributes for a given timestamp and parent block hash.
    pub fn eth_payload_attributes_with_parent(
        timestamp: u64,
        parent_block_hash: B256,
    ) -> IrysPayloadBuilderAttributes {
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
        IrysPayloadBuilderAttributes::try_new(parent_block_hash, rpc_attributes, 0)
            .expect("IrysPayloadBuilderAttributes::try_new is infallible")
    }

    /// Launches and connects multiple Irys+reth nodes for integration tests.
    ///
    /// # Arguments
    /// - `num_nodes`: Addresses for allowed shadow tx origins (one per node)
    /// - `chain_spec`: Chain spec to use
    /// - `is_dev`: Whether to run in dev mode
    /// - `attributes_generator`: Function to generate payload attributes
    ///
    /// # Returns
    /// - `Vec<NodeHelperType<IrysEthereumNode>>`: Test node handles
    /// - `Runtime`: Runtime for async tasks
    /// - `Wallet`: Default wallet for test accounts
    pub async fn setup_irys_reth(
        num_nodes: &[Address],
        chain_spec: Arc<<IrysEthereumNode as NodeTypes>::ChainSpec>,
        is_dev: bool,
        attributes_generator: impl Fn(u64, Address) -> <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadBuilderAttributes + Send + Sync + Clone + 'static,
    ) -> eyre::Result<(Vec<NodeHelperType<IrysEthereumNode>>, Runtime, Wallet)>
    where
        LocalPayloadAttributesBuilder<<IrysEthereumNode as NodeTypes>::ChainSpec>:
            PayloadAttributesBuilder<
                <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadAttributes,
            >,
    {
        let tasks = Runtime::test();

        let network_config = NetworkArgs {
            discovery: DiscoveryArgs {
                disable_discovery: true,
                ..DiscoveryArgs::default()
            },
            ..NetworkArgs::default()
        };

        // Create nodes and peer them
        let mut nodes: Vec<NodeTestContext<_, _>> = Vec::with_capacity(num_nodes.len());

        for (idx, producer) in num_nodes.iter().enumerate() {
            let node_config = NodeConfig::new(chain_spec.clone())
                .with_network(network_config.clone())
                .with_unused_ports()
                .with_rpc(RpcServerArgs::default().with_unused_ports().with_http())
                .set_dev(is_dev);

            let span = span!(Level::INFO, "node", idx);
            let _enter = span.enter();

            let NodeHandle {
                node,
                node_exit_future: _,
            } = NodeBuilder::new(node_config.clone())
                .testing_node(tasks.clone())
                .node(IrysEthereumNode)
                .launch()
                .await?;

            // Create a closure that passes the producer address
            let node_attributes_generator = {
                let attributes_generator = attributes_generator.clone();
                let producer_address = *producer;
                move |timestamp| attributes_generator(timestamp, producer_address)
            };

            let mut node = NodeTestContext::new(node, node_attributes_generator).await?;

            // Connect each node in a chain.
            if let Some(previous_node) = nodes.last_mut() {
                previous_node.connect(&mut node).await;
            }

            // Connect last node with the first if there are more than two
            if idx + 1 == num_nodes.len()
                && num_nodes.len() > 2
                && let Some(first_node) = nodes.first_mut()
            {
                node.connect(first_node).await;
            }

            nodes.push(node);
        }

        Ok((
            nodes,
            tasks,
            Wallet::default().with_chain_id(chain_spec.chain().into()),
        ))
    }
}
