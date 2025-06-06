//! # Irys Reth Node
//!
//! ## System Transactions
//! System transactions are special EVM transactions used to encode protocol-level actions such as:
//! - Block rewards (must go to the Irys block producer)
//! - Storage fee collection (balance decrements)
//! - Stake management (release, stake)
//! - Nonce reset (must always be the last system tx in a block)
//!
//! The CL must validate that:
//! - Block rewards are paid to the correct block producer
//! - Balance increments correspond to rewards
//! - Balance decrements correspond to storage transaction fees
//! - Every block ends with a nonce reset system tx

use std::{sync::Arc, time::SystemTime};

use alloy_consensus::TxLegacy;
use alloy_eips::{eip7840::BlobParams, merge::EPOCH_SLOTS};
use alloy_primitives::{Address, TxKind, U256};
pub use alloy_rlp;
use alloy_rlp::{Decodable as _, Encodable as _};
use evm::{IrysBlockAssembler, IrysEvmFactory};
use reth::{
    api::{FullNodeComponents, FullNodeTypes, NodeTypes, PayloadTypes},
    builder::{
        components::{ComponentsBuilder, ExecutorBuilder, PoolBuilder},
        BuilderContext, DebugNode, Node, NodeAdapter, NodeComponentsBuilder,
        PayloadBuilderConfig as _,
    },
    payload::{EthBuiltPayload, EthPayloadBuilderAttributes},
    primitives::{EthPrimitives, InvalidTransactionError, SealedBlock},
    providers::{
        providers::ProviderFactoryBuilder, CanonStateSubscriptions as _, EthStorage,
        StateProviderFactory,
    },
    rpc::builder::constants::DEFAULT_TX_FEE_CAP_WEI,
    transaction_pool::TransactionValidationTaskExecutor,
};
use reth_chainspec::{ChainSpec, ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_ethereum_engine_primitives::EthPayloadAttributes;
use reth_ethereum_primitives::TransactionSigned;
use reth_evm_ethereum::RethReceiptBuilder;
use reth_node_ethereum::{
    node::{EthereumAddOns, EthereumConsensusBuilder, EthereumNetworkBuilder},
    EthEngineTypes, EthEvmConfig,
};
use reth_primitives_traits::constants::MINIMUM_GAS_LIMIT;
use reth_tracing::tracing;
use reth_transaction_pool::{
    blobstore::{DiskFileBlobStore, DiskFileBlobStoreConfig},
    EthPoolTransaction, EthPooledTransaction, EthTransactionValidator, Pool, PoolTransaction,
    TransactionOrigin, TransactionValidator,
};
use reth_transaction_pool::{CoinbaseTipOrdering, TransactionValidationOutcome};
use reth_trie_db::MerklePatriciaTrie;
use system_tx::SystemTransaction;
use tracing::info;

use crate::{
    payload::SystemTxStore, payload_builder_builder::IrysPayloadBuilderBuilder,
    payload_service_builder::IyrsPayloadServiceBuilder,
};

pub mod evm;
pub mod payload;
pub mod payload_builder_builder;
pub mod payload_service_builder;
pub mod system_tx;

#[must_use]
pub fn compose_system_tx(chain_id: u64, system_tx: &SystemTransaction) -> TxLegacy {
    // allocate 512 bytes for the system tx rlp, misc optimisation
    let mut system_tx_rlp = Vec::with_capacity(512);
    system_tx.encode(&mut system_tx_rlp);
    TxLegacy {
        // large enough to not be rejected by the payload builder
        gas_limit: MINIMUM_GAS_LIMIT,
        value: U256::ZERO,
        // nonce is always 0 for system txs
        nonce: 0_u64,
        // large enough to not be rejected by the payload builder
        gas_price: DEFAULT_TX_FEE_CAP_WEI,
        chain_id: Some(chain_id),
        to: TxKind::Call(Address::ZERO),
        input: system_tx_rlp.into(),
    }
}

/// Type configuration for an Irys-Ethereum node.
#[derive(Debug, Clone)]
pub struct IrysEthereumNode {
    pub system_tx_store: SystemTxStore,
}

impl NodeTypes for IrysEthereumNode {
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;
    type StateCommitment = MerklePatriciaTrie;
    type Storage = EthStorage;
    type Payload = EthEngineTypes;
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
            PayloadAttributes = EthPayloadAttributes,
            PayloadBuilderAttributes = EthPayloadBuilderAttributes,
        >,
    {
        ComponentsBuilder::default()
            .node_types::<Node>()
            .pool(IrysPoolBuilder::default())
            .executor(IrysExecutorBuilder)
            .payload(IyrsPayloadServiceBuilder::new(IrysPayloadBuilderBuilder {
                system_tx_store: self.system_tx_store.clone(),
            }))
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

    type AddOns = EthereumAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        self.components()
    }

    fn add_ons(&self) -> Self::AddOns {
        EthereumAddOns::default()
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
}

/// A custom pool builder for Irys system transaction validation and pool configuration.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct IrysPoolBuilder;

/// Implement the [`PoolBuilder`] trait for the Irys pool builder
///
/// This will be used to build the transaction pool and its maintenance tasks during launch.
///
/// Original code from:
/// <https://github.com/Irys-xyz/reth-irys/blob/67abdf25dda69a660d44040d4493421b93d8de7b/crates/ethereum/node/src/node.rs?plain=1#L322>
///
/// Notable changes from the original: we reject all system txs, as they are not allowed to land in a the pool.
impl<Node> PoolBuilder<Node> for IrysPoolBuilder
where
    Node: FullNodeTypes<Types = IrysEthereumNode>,
{
    type Pool = Pool<
        TransactionValidationTaskExecutor<
            IrysSystemTxValidator<Node::Provider, EthPooledTransaction>,
        >,
        CoinbaseTipOrdering<EthPooledTransaction>,
        DiskFileBlobStore,
    >;

    async fn build_pool(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Pool> {
        let data_dir = ctx.config().datadir();
        let pool_config = ctx.pool_config();

        let blob_cache_size = if let Some(blob_cache_size) = pool_config.blob_cache_size {
            blob_cache_size
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
            let calculated_size = blob_params
                .target_blob_count
                .saturating_mul(EPOCH_SLOTS)
                .saturating_mul(2);
            u32::try_from(calculated_size).unwrap_or(u32::MAX)
        };

        let custom_config =
            DiskFileBlobStoreConfig::default().with_max_cached_entries(blob_cache_size);

        let blob_store = DiskFileBlobStore::open(data_dir.blobstore(), custom_config)?;
        let validator = TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone())
            .with_head_timestamp(ctx.head().timestamp)
            .kzg_settings(ctx.kzg_settings()?)
            .with_local_transactions_config(pool_config.local_transactions_config.clone())
            .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
            .with_additional_tasks(ctx.config().txpool.additional_validation_tasks)
            .build_with_tasks(ctx.task_executor().clone(), blob_store.clone());
        let validator = TransactionValidationTaskExecutor {
            validator: IrysSystemTxValidator {
                eth_tx_validator: validator.validator,
            },
            to_validation_task: validator.to_validation_task,
        };

        let ordering = CoinbaseTipOrdering::default();
        let transaction_pool =
            reth_transaction_pool::Pool::new(validator, ordering, blob_store, pool_config);
        info!(target: "reth::cli", "Transaction pool initialized");

        // Cache config values before moving the transaction_pool into the block
        let max_queued_lifetime = transaction_pool.config().max_queued_lifetime;
        let no_local_exemptions = transaction_pool
            .config()
            .local_transactions_config
            .no_exemptions;

        // spawn txpool maintenance task
        {
            let pool = &transaction_pool;
            let client = ctx.provider();
            // Only spawn backup task if not disabled
            if !ctx.config().txpool.disable_transactions_backup {
                // Use configured backup path or default to data dir
                let transactions_path = ctx
                    .config()
                    .txpool
                    .transactions_backup_path
                    .clone()
                    .unwrap_or_else(|| data_dir.txpool_transactions());

                let transactions_backup_config =
                    reth_transaction_pool::maintain::LocalTransactionBackupConfig::with_local_txs_backup(transactions_path);

                ctx.task_executor()
                    .spawn_critical_with_graceful_shutdown_signal(
                        "local transactions backup task",
                        |shutdown| {
                            reth_transaction_pool::maintain::backup_local_transactions_task(
                                shutdown,
                                pool.clone(),
                                transactions_backup_config,
                            )
                        },
                    );
            }

            // spawn the maintenance task
            ctx.task_executor().spawn_critical(
                "txpool maintenance task",
                reth_transaction_pool::maintain::maintain_transaction_pool_future(
                    client.clone(),
                    pool.clone(),
                    ctx.provider().canonical_state_stream(),
                    ctx.task_executor().clone(),
                    reth_transaction_pool::maintain::MaintainPoolConfig {
                        max_tx_lifetime: max_queued_lifetime,
                        no_local_exemptions,
                        ..Default::default()
                    },
                ),
            );
        };

        Ok(transaction_pool)
    }
}

#[derive(Debug, Clone)]
pub struct IrysSystemTxValidator<Client, T> {
    eth_tx_validator: EthTransactionValidator<Client, T>,
}

impl<Client, Tx> TransactionValidator for IrysSystemTxValidator<Client, Tx>
where
    Client: ChainSpecProvider<ChainSpec: EthereumHardforks> + StateProviderFactory,
    Tx: EthPoolTransaction,
{
    type Transaction = Tx;

    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        // Try to decode as a system transaction
        let input = transaction.input();
        let Ok(_system_tx) = SystemTransaction::decode(&mut &input[..]) else {
            tracing::trace!(hash = ?transaction.hash(), "non system tx, passing to eth validator");
            return self.eth_tx_validator.validate_one(origin, transaction);
        };

        tracing::trace!("system txs submitted to the pool. Not supported. Most likely via gossip from another node post-block confirmation");
        TransactionValidationOutcome::Invalid(
            transaction,
            reth_transaction_pool::error::InvalidPoolTransactionError::Consensus(
                InvalidTransactionError::SignerAccountHasBytecode,
            ),
        )
    }

    async fn validate_transactions(
        &self,
        transactions: Vec<(TransactionOrigin, Self::Transaction)>,
    ) -> Vec<TransactionValidationOutcome<Self::Transaction>> {
        self.eth_tx_validator.validate_all(transactions)
    }

    fn on_new_head_block<B>(&self, new_tip_block: &SealedBlock<B>)
    where
        B: reth_primitives_traits::Block,
    {
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
        let evm_config = EthEvmConfig::new(ctx.chain_spec())
            .with_extra_data(ctx.payload_builder_config().extra_data_bytes());
        let spec = ctx.chain_spec();
        let evm_factory = IrysEvmFactory::default();
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

    use crate::system_tx::{
        BalanceDecrement, BalanceIncrement, SystemTransaction, TransactionPacket, BLOCK_REWARD_ID,
        RELEASE_STAKE_ID,
    };
    use crate::test_utils::*;
    use crate::test_utils::{
        advance_blocks, block_reward, eth_payload_attributes_with_parent, get_balance,
        release_stake, sign_tx, stake, storage_fees,
    };
    use alloy_consensus::{EthereumTxEnvelope, SignableTransaction, TxEip4844};
    use alloy_eips::Encodable2718;
    use alloy_network::{EthereumWallet, TxSigner};
    use alloy_primitives::{Address, Uint, B256};
    use alloy_primitives::{FixedBytes, Signature};
    use alloy_rpc_types_engine::ForkchoiceState;
    use alloy_signer_local::PrivateKeySigner;
    use reth::api::EngineApiMessageVersion;
    use reth::{
        providers::{AccountReader, BlockHashReader, BlockNumReader},
        rpc::server_types::eth::EthApiError,
    };
    use reth_e2e_test_utils::wallet::Wallet;
    use reth_transaction_pool::TransactionPool;
    use std::sync::Mutex;
    use std::time::Duration;

    /// Ensures that only the allowed system tx origin can submit system transactions.
    ///
    /// Steps:
    /// - Setup: Use `setup_irys_reth` to launch node, `block_reward` to compose system tx, `sign_tx` to sign.
    /// - Action: Inject system tx with invalid origin.
    /// - Assertion: Tx is rejected with pool error.
    #[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
    async fn external_users_cannot_submit_system_txs() -> eyre::Result<()> {
        // setup
        let ctx = TestContext::new().await?;
        let ((node, _systemtx_rx), ctx) = ctx.get_single_node()?;

        let system_tx = block_reward(ctx.block_producer_a.address(), 1, ctx.genesis_blockhash);
        let mut system_tx_raw = compose_system_tx(1, &system_tx);
        let signed_tx = ctx
            .target_account
            .sign_transaction(&mut system_tx_raw)
            .await
            .unwrap();
        let tx = EthereumTxEnvelope::<TxEip4844>::Legacy(system_tx_raw.into_signed(signed_tx))
            .encoded_2718()
            .into();

        let tx_res = node.rpc.inject_tx(tx).await;
        assert!(matches!(tx_res, Err(EthApiError::PoolError(_))));
        Ok(())
    }

    /// Ensures that stale system transactions are dropped from the pool after a commit or reorg event.
    ///
    /// Setup:
    /// - Create two nodes with the same block producer.
    /// - Submit a normal tx to node a, and a system tx to node b.
    ///
    /// Action:
    /// - Advance the block on node a.
    /// - Update forkchoice on node b to point to the new block that was created on node a.
    ///
    /// Assertion:
    /// - The system tx from node b is not included in the block.
    /// - The normal tx from node a is included in the block.
    #[test_log::test(tokio::test)]
    async fn stale_system_txs_dont_get_included_in_fcus() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (((mut node_a, system_tx_store_a), (mut node_b, system_tx_store_b)), ctx) =
            ctx.get_two_nodes()?;

        // Submit normal transaction to node a
        let normal_tx_hash = create_and_submit_normal_tx(
            &mut node_a,
            0,
            U256::from(1000),
            1_000_000_000u128,
            Address::random(),
            &ctx.normal_signer,
        )
        .await?;
        let payload_node_a = advance_block(&mut node_a, &system_tx_store_a, vec![]).await?;

        // Submit system transaction to node b
        let system_tx = create_system_tx(
            BLOCK_REWARD_ID,
            ctx.block_producer_b.address(),
            1,
            ctx.genesis_blockhash,
        );
        let system_tx = sign_system_tx(system_tx, &ctx.block_producer_b).await?;
        let system_tx_hash = *system_tx.hash();
        let _payload_node_b =
            advance_block(&mut node_b, &system_tx_store_b, vec![system_tx]).await?;

        // Update forkchoice on node b
        node_b.sync_to(payload_node_a.block().hash()).await?;

        // Assert system tx never touched the pool
        let pool_txs: Vec<_> = node_b
            .inner
            .pool
            .all_transactions()
            .all()
            .map(|tx| *tx.hash())
            .collect();
        assert!(
            !pool_txs.contains(&system_tx_hash),
            "System tx should never enter the second node pool"
        );

        // Assert system tx never touched the pool
        let pool_txs: Vec<_> = node_a
            .inner
            .pool
            .all_transactions()
            .all()
            .map(|tx| *tx.hash())
            .collect();
        assert!(
            !pool_txs.contains(&system_tx_hash),
            "System tx should never enter the first node pool"
        );

        // Assert system tx never touched the block
        assert_txs_not_in_block(
            &payload_node_a,
            &[system_tx_hash],
            "System tx should not be in block",
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
    async fn block_with_system_txs_gets_broadcasted_between_peers() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (((mut node_a, system_tx_store_a), (mut node_b, _system_tx_store_b)), ctx) =
            ctx.get_two_nodes()?;

        let initial_balance = get_balance(&node_a.inner, ctx.block_producer_a.address());

        let amount = U256::from(7000000000000000000u64);
        let system_tx = compose_system_tx(
            1,
            &SystemTransaction {
                valid_for_block_height: 1,
                parent_blockhash: ctx.genesis_blockhash,
                inner: TransactionPacket::BlockReward(BalanceIncrement {
                    amount,
                    target: ctx.block_producer_a.address(),
                }),
            },
        );
        let system_tx = sign_tx(system_tx, &ctx.block_producer_a).await;
        let system_tx_hash = *system_tx.hash();

        // make the node advance
        let payload = advance_block(&mut node_a, &system_tx_store_a, vec![system_tx]).await?;

        let block_hash = payload.block().hash();
        let block_number = payload.block().number;
        assert_eq!(block_number, 1);

        // assert the block has been committed to the blockchain
        node_a
            .assert_new_block(system_tx_hash, block_hash, block_number)
            .await?;

        // only send forkchoice update to second node
        node_b.update_forkchoice(block_hash, block_hash).await?;

        // expect second node advanced via p2p gossip
        node_b
            .assert_new_block(system_tx_hash, block_hash, block_number)
            .await?;

        assert_balance_change(
            &node_b,
            ctx.block_producer_a.address(),
            initial_balance,
            amount,
            true,
            "Producer balance should increase",
        );

        Ok(())
    }

    // assert that "incrementing" system txs update account state
    #[test_log::test(tokio::test)]
    #[rstest::rstest]
    #[case::release_stake(release_stake, signer_b())]
    #[case::release_stake_init_no_balance(release_stake, signer_random())]
    #[case::block_reward(block_reward, signer_b())]
    #[case::block_reward_init_no_balance(block_reward, signer_random())]
    async fn incr_system_txs(
        #[case] system_tx: impl Fn(Address, u64, FixedBytes<32>) -> SystemTransaction,
        #[case] target_signer: Arc<dyn TxSigner<Signature> + Send + Sync>,
    ) -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let ((mut node, system_tx_store), ctx) = ctx.get_single_node()?;

        let initial_balance = get_balance(&node.inner, target_signer.address());
        let initial_producer_balance = get_balance(&node.inner, ctx.block_producer_a.address());

        let tx_count = 5;
        let system_tx = system_tx(target_signer.address(), 1, ctx.genesis_blockhash);
        let system_tx_topic = system_tx.inner.topic().into();
        let system_tx = sign_system_tx(system_tx, &target_signer).await?;
        let system_txs = vec![system_tx.clone(); tx_count];

        let _block_payload =
            mine_block_and_validate(&mut node, &system_tx_store, system_txs, &[]).await?;

        let block_execution = node.inner.provider.get_state(0..=1).unwrap().unwrap();
        assert_topic_present_in_logs(block_execution, system_tx_topic, tx_count as u64);

        assert_balance_change(
            &node,
            target_signer.address(),
            initial_balance,
            U256::from(tx_count),
            true,
            "Target balance should increase",
        );
        assert_balance_change(
            &node,
            ctx.block_producer_a.address(),
            initial_producer_balance,
            U256::ZERO,
            true,
            "Producer balance should not change",
        );
        assert_nonce(
            &node,
            ctx.block_producer_a.address(),
            0,
            "Producer nonce should not change",
        );

        Ok(())
    }

    // check if the "decrementing" system txs update account state
    #[test_log::test(tokio::test)]
    #[rstest::rstest]
    #[case::stake(stake, signer_b())]
    #[case::storage_fees(storage_fees, signer_b())]
    async fn decr_system_txs(
        #[case] system_tx: impl Fn(Address, u64, FixedBytes<32>) -> SystemTransaction,
        #[case] target_signer: Arc<dyn TxSigner<Signature> + Send + Sync>,
    ) -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let ((mut node, system_tx_store), ctx) = ctx.get_single_node()?;

        let initial_balance = get_balance(&node.inner, target_signer.address());
        let initial_producer_balance = get_balance(&node.inner, ctx.block_producer_a.address());

        let tx_count = 2;
        let system_tx = system_tx(target_signer.address(), 1, ctx.genesis_blockhash);
        let system_tx_topic = system_tx.inner.topic().into();
        let system_tx = sign_system_tx(system_tx, &target_signer).await?;
        let system_txs = vec![system_tx.clone(); tx_count];
        let tx_hashes = system_txs.iter().map(|tx| *tx.hash()).collect::<Vec<_>>();
        let block_payload =
            mine_block_and_validate(&mut node, &system_tx_store, system_txs, &[]).await?;

        // Assertions
        let block_execution = node.inner.provider.get_state(0..=1).unwrap().unwrap();

        // Ensure all txs are included in the block
        assert_txs_in_block(&block_payload, &tx_hashes, "System transactions");

        // Assert all txs have a corresponding log
        assert_topic_present_in_logs(block_execution, system_tx_topic, tx_count as u64);

        // Assert balance for target is decremented
        assert_balance_change(
            &node,
            target_signer.address(),
            initial_balance,
            U256::from(tx_count),
            false, // is_decrement
            "Target balance should be reduced",
        );

        // Assert balance for producer remains the same (system txs cost nothing)
        assert_balance_change(
            &node,
            ctx.block_producer_a.address(),
            initial_producer_balance,
            U256::ZERO,
            true,
            "Producer balance should not change",
        );

        assert_nonce(
            &node,
            ctx.block_producer_a.address(),
            0,
            "Producer nonce should not change",
        );

        Ok(())
    }

    // expect that system txs get executed first, no matter what. Normal txs get executed only afterwards
    #[test_log::test(tokio::test)]
    async fn test_system_tx_ordering() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let ((mut node, system_tx_store), ctx) = ctx.get_single_node()?;

        // Create normal transactions with high gas price
        let normal_tx_hashes = create_and_submit_multiple_normal_txs(
            &mut node,
            3,
            0,
            10_000_000_000u128, // High gas price
            Address::random(),
            &ctx.normal_signer,
        )
        .await?;

        // Create system transactions with lower effective priority
        let system_tx = create_system_tx(
            RELEASE_STAKE_ID,
            ctx.target_account.address(),
            1,
            ctx.genesis_blockhash,
        );
        let system_tx = sign_system_tx(system_tx, &ctx.block_producer_a).await?;
        let system_txs = vec![system_tx.clone(); 2];
        let system_tx_hashes = system_txs.iter().map(|tx| *tx.hash()).collect::<Vec<_>>();

        let block_payload =
            mine_block_and_validate(&mut node, &system_tx_store, system_txs, &normal_tx_hashes)
                .await?;

        assert_txs_in_block(&block_payload, &system_tx_hashes, "System transactions");
        assert_txs_in_block(&block_payload, &normal_tx_hashes, "Normal transactions");
        assert_system_txs_before_normal_txs(&block_payload, &system_tx_hashes, &normal_tx_hashes);

        Ok(())
    }

    // test decrementing when account does not exist (expect that even receipt not created)
    #[test_log::test(tokio::test)]
    async fn test_decrement_nonexistent_account() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let ((mut node, system_tx_store), ctx) = ctx.get_single_node()?;

        // Create a random address that has never existed on chain
        let nonexistent_address = Address::random();

        // Verify the account doesn't exist
        let account = node
            .inner
            .provider
            .basic_account(&nonexistent_address)
            .unwrap();
        assert!(account.is_none(), "Test account should not exist");

        // Create and submit a system transaction trying to decrement balance of non-existent account
        let system_tx = SystemTransaction {
            inner: TransactionPacket::Stake(BalanceDecrement {
                amount: U256::ONE,
                target: nonexistent_address,
            }),
            valid_for_block_height: 1,
            parent_blockhash: ctx.genesis_blockhash,
        };
        let system_tx = sign_system_tx(system_tx, &ctx.block_producer_a).await?;
        let system_tx_hashes = vec![*system_tx.hash()];

        // Submit a normal transaction to ensure block is produced
        let normal_tx_hash = create_and_submit_normal_tx(
            &mut node,
            0,
            U256::from(1000),
            1_000_000_000u128,
            Address::random(),
            &ctx.normal_signer,
        )
        .await?;

        // Produce a new block
        let block_payload = mine_block(&mut node, &system_tx_store, vec![system_tx]).await?;

        // // Verify the system transaction is NOT included
        assert_txs_not_in_block(
            &block_payload,
            &system_tx_hashes,
            "System transaction for non-existent account should not be included in block",
        );

        // Verify the normal transaction IS included
        assert_txs_in_block(
            &block_payload,
            &[normal_tx_hash],
            "Normal transaction should be included in block",
        );

        Ok(())
    }

    // test decrementing when account exists but not enough balance (expect failed tx receipt)
    #[test_log::test(tokio::test)]
    async fn test_decrement_insufficient_balance() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let ((mut node, system_tx_store), ctx) = ctx.get_single_node()?;

        let funded_balance = get_balance(&node.inner, ctx.normal_signer.address());

        assert!(
            funded_balance > U256::ZERO,
            "Funded account should have nonzero balance"
        );

        // Create a system tx that tries to decrement more than the balance
        let decrement_amount = funded_balance + U256::ONE;
        let system_tx = SystemTransaction {
            inner: TransactionPacket::Stake(BalanceDecrement {
                amount: decrement_amount,
                target: ctx.normal_signer.address(),
            }),
            valid_for_block_height: 1,
            parent_blockhash: ctx.genesis_blockhash,
        };
        let system_tx = sign_system_tx(system_tx, &ctx.block_producer_a).await?;
        let system_tx_hashes = vec![*system_tx.hash()];

        // Produce a new block
        let block_payload = mine_block(&mut node, &system_tx_store, vec![system_tx]).await?;

        // Verify the system transaction IS included
        assert_txs_in_block(
            &block_payload,
            &system_tx_hashes,
            "System transaction should be included in block",
        );

        // Verify the receipt for the system tx is a revert/failure
        let block_execution = node.inner.provider.get_state(0..=1).unwrap().unwrap();
        let receipts = block_execution.receipts;
        let receipt = &receipts[1][0];
        assert!(
            !receipt.success,
            "Expected a revert/failure receipt for system tx with insufficient balance"
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
    fn signer_random() -> Arc<dyn TxSigner<Signature> + Send + Sync> {
        Arc::new(PrivateKeySigner::random())
    }

    /// Mines 5 blocks, each with a system (block reward) and a normal tx.
    /// Asserts both txs are present in every block.
    /// Verifies sequential block production and tx inclusion.
    /// Expects latest block number to be 5 at the end.
    #[test_log::test(tokio::test)]
    async fn mine_5_blocks_with_system_and_normal_tx() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let ((mut node, system_tx_store), ctx) = ctx.get_single_node()?;

        let mut parent_blockhash = ctx.genesis_blockhash;
        let recipient = ctx.target_account.address();

        for block_number in 1..=5 {
            // Block reward system tx
            let system_tx = block_reward(
                ctx.block_producer_a.address(),
                block_number,
                parent_blockhash,
            );
            let system_tx = sign_system_tx(system_tx, &ctx.block_producer_a).await?;

            // Normal tx
            let normal_tx_hash = create_and_submit_normal_tx(
                &mut node,
                block_number - 1,
                U256::from(1234u64),
                2_000_000_000u128, // 2 Gwei
                recipient,
                &ctx.normal_signer,
            )
            .await?;

            // Mine block
            let block_payload = mine_block_and_validate(
                &mut node,
                &system_tx_store,
                vec![system_tx],
                &[normal_tx_hash],
            )
            .await?;
            parent_blockhash = block_payload.block().hash();
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

    /// Submits a system tx with an invalid parent blockhash and a valid normal tx.
    /// Asserts the system tx is rejected (not in block), normal tx is included.
    /// Expects only valid txs to be mined.
    /// Ensures parent blockhash check is enforced for system txs.
    #[test_log::test(tokio::test)]
    async fn system_tx_with_invalid_parent_blockhash_is_rejected_by_custom_executor(
    ) -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let ((mut node, system_tx_store), ctx) = ctx.get_single_node()?;

        // Create invalid parent blockhash (random, not actual parent block)
        let invalid_parent_blockhash = FixedBytes::random();

        // Create a system tx with the invalid parent blockhash
        let system_tx = block_reward(ctx.block_producer_a.address(), 1, invalid_parent_blockhash);
        let system_tx = sign_system_tx(system_tx, &ctx.block_producer_a).await?;
        let system_tx_hashes = vec![*system_tx.hash()];

        // Create and submit a normal user tx
        let normal_tx_hash = create_and_submit_normal_tx(
            &mut node,
            0,
            U256::from(1234u64),
            2_000_000_000u128, // 2 Gwei
            ctx.normal_signer.address(),
            &ctx.normal_signer,
        )
        .await?;

        // Mine a block
        let block_payload = mine_block(&mut node, &system_tx_store, vec![system_tx]).await?;

        // Assert that the system tx is NOT present in the block
        assert_txs_not_in_block(
            &block_payload,
            &system_tx_hashes,
            "System tx with invalid parent blockhash should not be included in the block",
        );

        // Assert that the normal tx IS present in the block
        assert_txs_in_block(
            &block_payload,
            &[normal_tx_hash],
            "Normal user tx should be included in the block",
        );

        Ok(())
    }

    /// Submits a system tx with a valid parent blockhash but invalid block number, plus a normal tx.
    /// Asserts the system tx is rejected (not in block), normal tx is included.
    /// Expects only valid txs to be mined.
    /// Ensures block number check is enforced for system txs.
    #[test_log::test(tokio::test)]
    async fn system_tx_with_invalid_block_number_is_rejected_by_custom_executor() -> eyre::Result<()>
    {
        let ctx = TestContext::new().await?;
        let ((mut node, system_tx_store), ctx) = ctx.get_single_node()?;

        // Use invalid block number (should be 1 for first block, but using 2)
        let invalid_block_number = 2u64;

        // Create a system tx with the valid parent blockhash but invalid block number
        let system_tx = block_reward(
            ctx.block_producer_a.address(),
            invalid_block_number,
            ctx.genesis_blockhash,
        );
        let system_tx = sign_system_tx(system_tx, &ctx.block_producer_a).await?;
        let system_tx_hashes = vec![*system_tx.hash()];

        // Create and submit a normal user tx
        let normal_tx_hash = create_and_submit_normal_tx(
            &mut node,
            0,
            U256::from(1234u64),
            2_000_000_000u128, // 2 Gwei
            ctx.normal_signer.address(),
            &ctx.normal_signer,
        )
        .await?;

        // Mine a block
        let block_payload = mine_block(&mut node, &system_tx_store, vec![system_tx]).await?;

        // Assert that the system tx is NOT present in the block
        assert_txs_not_in_block(
            &block_payload,
            &system_tx_hashes,
            "System tx with invalid block number should not be included in the block",
        );

        // Assert that the normal tx IS present in the block
        assert_txs_in_block(
            &block_payload,
            &[normal_tx_hash],
            "Normal user tx should be included in the block",
        );

        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn rollback_state_revert_on_fork_switch() -> eyre::Result<()> {
        // Setup nodes and context
        let ctx = TestContext::new().await?;
        let (((mut node_a, system_tx_store_node_a), (mut node_b, system_tx_store_node_b)), ctx) =
            ctx.get_two_nodes()?;
        let reward_address = Address::random();

        // Node A: advance 3 blocks, 2 system txs per block
        let system_tx = block_reward(reward_address, 1, ctx.genesis_blockhash);
        let system_txs = vec![vec![system_tx; 2]; 3];

        let _block_hashes_a = advance_blocks(
            &mut node_a,
            &system_tx_store_node_a,
            system_txs,
            &ctx.block_producer_a,
            ctx.genesis_blockhash,
            1,
        )
        .await?;
        let consistent_provider_a = node_a.inner.provider.consistent_provider().unwrap();
        let account_a_on_fork = consistent_provider_a
            .basic_account(&ctx.block_producer_a.address())
            .unwrap()
            .unwrap();
        let node_a_reward_balance = get_balance(&node_a.inner, reward_address);

        // Node B: advance 4 blocks, 1 system tx per block
        let system_tx = block_reward(reward_address, 1, ctx.genesis_blockhash);
        let system_txs = vec![vec![system_tx; 1]; 4];
        let _block_hashes_b = advance_blocks(
            &mut node_b,
            &system_tx_store_node_b,
            system_txs,
            &ctx.block_producer_b,
            ctx.genesis_blockhash,
            1,
        )
        .await?;

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
        assert_eq!(reward_balance_post_switch_node_a, Uint::from(4));
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
    /// 1. Build 4 blocks with block rewards and nonce resets (balance +4, nonce reset to 0 each block)
    /// 2. Verify state after 4 blocks: balance = initial + 4, nonce = 0
    /// 3. Roll back to block 1 via forkchoice update (safe/finalized = block 1)
    /// 4. Build a new fork block (block 2) on top of the rolled-back state
    /// 5. Verify final state: balance = initial + 2, nonce = 1 (reflecting the rollback and new block)
    #[test_log::test(tokio::test)]
    async fn rollback_state_on_safe_blocks() -> eyre::Result<()> {
        // Setup custom payload attributes to control parent block hash
        let parent_tracker = Arc::new(Mutex::new(B256::ZERO));
        let payload_attributes = {
            let parent_tracker = parent_tracker.clone();
            move |timestamp: u64| {
                let parent = *parent_tracker.lock().unwrap();
                eth_payload_attributes_with_parent(timestamp, parent)
            }
        };

        let ctx = TestContext::new_with_payload_attributes(payload_attributes).await?;
        let ((mut node, system_tx_store), ctx) = ctx.get_single_node()?;

        // Initial setup and baseline measurements
        let mut parent_blockhash = ctx.genesis_blockhash;
        let initial_balance = get_balance(&node.inner, ctx.block_producer_a.address());
        let mut block_hashes = vec![parent_blockhash];

        // Phase 1: Build 4 blocks with system transactions
        tracing::info!("Phase 1: Building 4 blocks with block rewards and nonce resets");
        for block_number in 1..=4 {
            // Create block reward transaction
            let block_reward_tx = block_reward(
                ctx.block_producer_a.address(),
                block_number,
                parent_blockhash,
            );
            let block_reward_tx = sign_system_tx(block_reward_tx, &ctx.block_producer_a).await?;

            // Mine the block
            let payload =
                mine_block_and_validate(&mut node, &system_tx_store, vec![block_reward_tx], &[])
                    .await?;
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
        assert_balance_change(
            &node,
            ctx.block_producer_a.address(),
            initial_balance,
            U256::from(4),
            true,
            "Balance should reflect 4 block rewards",
        );
        assert_nonce(
            &node,
            ctx.block_producer_a.address(),
            0,
            "Nonce should be 0 after nonce resets",
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
        let fork_reward_tx = block_reward(
            ctx.block_producer_a.address(),
            fork_block_number,
            rollback_target,
        );
        let fork_reward_tx = sign_system_tx(fork_reward_tx, &ctx.block_producer_a).await?;
        let fork_payload = prepare_block(&mut node, &system_tx_store, vec![fork_reward_tx]).await?;
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
        assert_balance_change(
            &node,
            ctx.block_producer_a.address(),
            initial_balance,
            U256::from(2), // 1 from block 1 + 1 from fork block 2
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

    /// Tests that system transactions never enter the transaction pool when rolling back state to a past block.
    ///
    /// Test scenario:
    /// 1. Setup a node and create system transactions
    /// 2. Mine blocks with system transactions to establish state
    /// 3. Rollback the state to a past block
    /// 4. Verify that system transactions are never in the transaction pool past
    #[test_log::test(tokio::test)]
    async fn system_txs_never_in_pool_during_rollback() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let ((mut node, system_tx_store), ctx) = ctx.get_single_node()?;

        // Phase 1: Build initial blocks with system transactions
        let mut parent_blockhash = ctx.genesis_blockhash;
        let _initial_balance = get_balance(&node.inner, ctx.block_producer_a.address());
        let mut block_hashes = vec![parent_blockhash];
        let mut system_txs = vec![];

        // Build 3 blocks with system transactions
        for block_number in 1..=3 {
            let block_reward_tx = block_reward(
                ctx.block_producer_a.address(),
                block_number,
                parent_blockhash,
            );
            let block_reward_tx = sign_system_tx(block_reward_tx, &ctx.block_producer_a).await?;
            system_txs.push(block_reward_tx.clone());

            let normal_tx = create_and_submit_normal_tx(
                &mut node,
                block_number - 1,
                U256::from(1234u64),
                2_000_000_000u128, // 2 Gwei
                ctx.target_account.address(),
                &ctx.normal_signer,
            )
            .await?;

            let payload = mine_block_and_validate(
                &mut node,
                &system_tx_store,
                vec![block_reward_tx],
                &[normal_tx],
            )
            .await?;
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

        // Phase 4: Verify system transactions never entered the pool during rollback
        // The key invariant is that system txs should NEVER be in the transaction pool regardless of rollback state
        let pool_txs: Vec<_> = node
            .inner
            .pool
            .all_transactions()
            .all()
            .map(|tx| *tx.hash())
            .collect();

        for tx in system_txs {
            assert!(
                !pool_txs.contains(tx.hash()),
                "System tx should never be in the transaction pool during rollback"
            );
            assert!(
                pool_txs.is_empty(),
                "Transaction pool should be empty during rollback, but contains: {:?}",
                pool_txs
            );
        }

        // Phase 5: Try to submit the future system transactions directly to the pool
        // They should be rejected and never enter the pool
        let future_system_tx_1 = block_reward(
            ctx.block_producer_a.address(),
            2, // block 2
            rollback_target,
        );
        let mut tx_1_raw = compose_system_tx(1, &future_system_tx_1);
        let signed_tx_1 = ctx
            .block_producer_a
            .sign_transaction(&mut tx_1_raw)
            .await
            .unwrap();
        let tx_1_envelope =
            EthereumTxEnvelope::<TxEip4844>::Legacy(tx_1_raw.into_signed(signed_tx_1))
                .encoded_2718()
                .into();

        // These should fail since system txs are not allowed in the pool
        let tx_1_result = node.rpc.inject_tx(tx_1_envelope).await;

        assert!(
            tx_1_result.is_err(),
            "System transaction should be rejected when submitted to pool"
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
            "Transaction pool should remain empty after attempted system tx submission, but contains: {:?}",
            final_pool_txs
        );

        Ok(())
    }

    /// Tests that system transactions are executed in the exact order they were submitted.
    /// This is verified by checking the transaction hashes in the receipts match the submission order.
    ///
    /// Test scenario:
    /// 1. Create 5 different types of system transactions in a specific order
    /// 2. Submit them to the system transaction store
    /// 3. Mine a block containing these transactions
    /// 4. Verify the receipts contain the transactions in the same order as submitted
    #[test_log::test(tokio::test)]
    async fn test_system_tx_execution_order_via_receipts() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let ((mut node, system_tx_store), ctx) = ctx.get_single_node()?;

        // Create different addresses for different transaction types
        let address_a = ctx.block_producer_a.address();
        let address_b = ctx.target_account.address();
        let address_c = ctx.normal_signer.address();

        // Create 5 different system transactions in a specific order
        let mut system_txs = Vec::new();
        let mut expected_tx_hashes = Vec::new();

        // 1. Block reward
        let block_reward_tx = block_reward(address_a, 1, ctx.genesis_blockhash);
        let block_reward_tx = sign_system_tx(block_reward_tx, &ctx.block_producer_a).await?;
        expected_tx_hashes.push(*block_reward_tx.hash());
        system_txs.push(block_reward_tx);

        // 2. Release stake
        let release_stake_tx = release_stake(address_b, 1, ctx.genesis_blockhash);
        let release_stake_tx = sign_system_tx(release_stake_tx, &ctx.block_producer_a).await?;
        expected_tx_hashes.push(*release_stake_tx.hash());
        system_txs.push(release_stake_tx);

        // 3. Storage fees
        let storage_fees_tx = storage_fees(address_c, 1, ctx.genesis_blockhash);
        let storage_fees_tx = sign_system_tx(storage_fees_tx, &ctx.block_producer_a).await?;
        expected_tx_hashes.push(*storage_fees_tx.hash());
        system_txs.push(storage_fees_tx);

        // 4. Another block reward
        let block_reward_2_tx = block_reward(address_b, 1, ctx.genesis_blockhash);
        let block_reward_2_tx = sign_system_tx(block_reward_2_tx, &ctx.block_producer_a).await?;
        expected_tx_hashes.push(*block_reward_2_tx.hash());
        system_txs.push(block_reward_2_tx);

        // 5. Stake transaction
        let stake_tx = stake(address_a, 1, ctx.genesis_blockhash);
        let stake_tx = sign_system_tx(stake_tx, &ctx.block_producer_a).await?;
        expected_tx_hashes.push(*stake_tx.hash());
        system_txs.push(stake_tx);

        tracing::info!(
            "Created {} system transactions in order: {:?}",
            system_txs.len(),
            expected_tx_hashes
        );

        // Mine a block with these system transactions
        let block_payload = mine_block(&mut node, &system_tx_store, system_txs).await?;

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
            "Should have exactly {} receipts for the {} system transactions",
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

        // Verify all receipts are successful (system transactions should succeed)
        for (i, receipt) in block_1_receipts.iter().enumerate() {
            assert!(
                receipt.success,
                "Receipt at position {} should be successful for system transaction {:?}",
                i, expected_tx_hashes[i]
            );
        }

        Ok(())
    }
}

#[cfg(any(feature = "test-utils", test))]
/// Test Utilities for Irys Reth node
pub mod test_utils {
    use super::*;
    use crate::payload::DeterministicSystemTxKey;
    use crate::system_tx::{SystemTransaction, TransactionPacket};
    use alloy_consensus::EthereumTxEnvelope;
    use alloy_consensus::{SignableTransaction, TxEip4844, TxLegacy};
    use alloy_genesis::Genesis;
    use alloy_network::EthereumWallet;
    use alloy_network::TxSigner;
    use alloy_primitives::Address;
    use alloy_primitives::{FixedBytes, Signature, B256};
    use alloy_primitives::{TxKind, U256};
    use alloy_rpc_types::engine::PayloadAttributes;
    use reth::providers::CanonStateSubscriptions;
    use reth::{
        api::{FullNodePrimitives, PayloadAttributesBuilder},
        args::{DiscoveryArgs, NetworkArgs, RpcServerArgs},
        builder::{rpc::RethRpcAddOns, FullNode, NodeBuilder, NodeConfig, NodeHandle},
        providers::{AccountReader, BlockHashReader},
        rpc::api::eth::helpers::EthTransactions,
        tasks::TaskManager,
    };
    use reth_e2e_test_utils::{node::NodeTestContext, wallet::Wallet, NodeHelperType};
    use reth_engine_local::LocalPayloadAttributesBuilder;
    use reth_transaction_pool::TransactionPool;
    use std::collections::HashSet;
    use std::sync::Arc;
    use tracing::{span, Level};

    /// Common setup for tests - creates wallets, nodes, and returns initialized context
    pub struct TestContext {
        pub nodes: Vec<(NodeHelperType<IrysEthereumNode>, SystemTxStore)>,
        pub block_producer_a: Arc<dyn TxSigner<Signature> + Send + Sync>,
        pub block_producer_b: Arc<dyn TxSigner<Signature> + Send + Sync>,
        pub normal_signer: Arc<dyn TxSigner<Signature> + Send + Sync>,
        pub target_account: Arc<dyn TxSigner<Signature> + Send + Sync>,
        pub genesis_blockhash: FixedBytes<32>,
        #[allow(dead_code)]
        pub tasks: TaskManager,
    }

    impl std::fmt::Debug for TestContext {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "TestContext")
        }
    }

    impl TestContext {
        pub async fn new() -> eyre::Result<Self> {
            Self::new_with_payload_attributes(eth_payload_attributes).await
        }

        pub async fn new_with_payload_attributes(
            payload_attributes: impl Fn(u64) -> EthPayloadBuilderAttributes
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
                .0
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

        pub fn get_single_node(
            mut self,
        ) -> eyre::Result<((NodeHelperType<IrysEthereumNode>, SystemTxStore), Self)> {
            if self.nodes.is_empty() {
                return Err(eyre::eyre!("No nodes available"));
            }
            let (node, system_tx_receiver) = self.nodes.remove(0);
            Ok(((node, system_tx_receiver), self))
        }

        pub fn get_two_nodes(
            mut self,
        ) -> eyre::Result<(
            (
                (NodeHelperType<IrysEthereumNode>, SystemTxStore),
                (NodeHelperType<IrysEthereumNode>, SystemTxStore),
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

    /// Helper for creating and submitting system transactions
    pub async fn sign_system_tx(
        system_tx: SystemTransaction,
        signer: &Arc<dyn TxSigner<Signature> + Send + Sync>,
    ) -> eyre::Result<EthPooledTransaction> {
        let system_tx_raw = compose_system_tx(1, &system_tx);
        let system_pooled_tx = sign_tx(system_tx_raw, signer).await;
        Ok(system_pooled_tx)
    }

    /// Helper for creating and submitting multiple system transactions
    pub async fn create_multiple_system_txs(
        system_tx_store: &SystemTxStore,
        system_tx: &SystemTransaction,
        count: u64,
        signer: &Arc<dyn TxSigner<Signature> + Send + Sync>,
        key: DeterministicSystemTxKey,
    ) -> eyre::Result<Vec<EthPooledTransaction>> {
        let mut txs = Vec::new();
        for _ in 0..count {
            let tx = sign_system_tx(system_tx.clone(), signer).await?;
            txs.push(tx);
        }
        system_tx_store.set_system_txs(key, txs.clone());
        Ok(txs)
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
        let tx_hash = node
            .inner
            .pool
            .add_transaction(
                reth_transaction_pool::TransactionOrigin::Local,
                normal_pooled_tx,
            )
            .await?;
        Ok(tx_hash)
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
                U256::from(1234u64),
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
                "{}: Transaction {:?} not found in block",
                message,
                tx_hash
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
                "{}: Transaction {:?} should not be in block",
                message,
                tx_hash
            );
        }
    }

    /// Helper for asserting transaction ordering in blocks
    pub fn assert_system_txs_before_normal_txs(
        block_payload: &EthBuiltPayload,
        system_txs: &[alloy_primitives::FixedBytes<32>],
        normal_txs: &[alloy_primitives::FixedBytes<32>],
    ) {
        let block_txs: Vec<_> = block_payload
            .block()
            .body()
            .transactions
            .iter()
            .map(|tx| *tx.hash())
            .collect();

        let mut last_system_tx_pos = 0;
        let mut first_normal_tx_pos = block_txs.len();

        for (pos, tx_hash) in block_txs.iter().enumerate() {
            if system_txs.contains(tx_hash) {
                last_system_tx_pos = last_system_tx_pos.max(pos);
            }
            if normal_txs.contains(tx_hash) {
                first_normal_tx_pos = first_normal_tx_pos.min(pos);
            }
        }

        assert!(
            last_system_tx_pos < first_normal_tx_pos,
            "System transactions should appear before normal transactions. Last system: {}, First normal: {}",
            last_system_tx_pos,
            first_normal_tx_pos
        );
    }

    /// Helper for asserting balance changes
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
            "{}: Expected balance {}, got {}",
            message, expected_balance, final_balance
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
            "{}: Expected nonce {}, got {}",
            message, expected_nonce, actual_nonce
        );
    }

    /// Helper for block mining and validation
    pub async fn mine_block(
        node: &mut NodeHelperType<IrysEthereumNode>,
        system_tx_store: &SystemTxStore,
        system_txs: Vec<EthPooledTransaction>,
    ) -> eyre::Result<EthBuiltPayload> {
        let block_payload = advance_block(node, system_tx_store, system_txs).await?;
        Ok(block_payload)
    }

    /// Helper for block mining and validation
    pub async fn mine_block_and_validate(
        node: &mut NodeHelperType<IrysEthereumNode>,
        system_tx_store: &SystemTxStore,
        system_txs: Vec<EthPooledTransaction>,
        expected_normal_txs: &[alloy_primitives::FixedBytes<32>],
    ) -> eyre::Result<EthBuiltPayload> {
        let expected_system_tx_hashes = system_txs.iter().map(|tx| *tx.hash()).collect::<Vec<_>>();
        let block_payload = advance_block(node, system_tx_store, system_txs).await?;

        assert_txs_in_block(
            &block_payload,
            &expected_system_tx_hashes,
            "System transactions",
        );
        assert_txs_in_block(&block_payload, expected_normal_txs, "Normal transactions");

        if !expected_system_tx_hashes.is_empty() && !expected_normal_txs.is_empty() {
            assert_system_txs_before_normal_txs(
                &block_payload,
                &expected_system_tx_hashes,
                expected_normal_txs,
            );
        }

        Ok(block_payload)
    }

    /// Helper to create system transaction based on type
    pub fn create_system_tx(
        tx_type: u8,
        address: Address,
        valid_for_block_height: u64,
        parent_blockhash: FixedBytes<32>,
    ) -> SystemTransaction {
        use crate::system_tx::*;
        match tx_type {
            BLOCK_REWARD_ID => block_reward(address, valid_for_block_height, parent_blockhash),
            RELEASE_STAKE_ID => release_stake(address, valid_for_block_height, parent_blockhash),
            STAKE_ID => stake(address, valid_for_block_height, parent_blockhash),
            STORAGE_FEES_ID => storage_fees(address, valid_for_block_height, parent_blockhash),
            _ => panic!("Unknown system transaction type: {}", tx_type),
        }
    }

    /// - store system txs in the store
    /// - prepare a new payload
    /// - DOES NOT update the forkchoice
    pub async fn prepare_block(
        node: &mut NodeHelperType<IrysEthereumNode>,
        system_tx_store: &SystemTxStore,
        system_txs: Vec<EthPooledTransaction>,
    ) -> Result<EthBuiltPayload, eyre::Error> {
        node.payload.timestamp += 1;
        let attributes = (node.payload.attributes_generator)(node.payload.timestamp);
        let key = DeterministicSystemTxKey::new(attributes.payload_id());
        system_tx_store.set_system_txs(key, system_txs);
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
        system_tx_store: &SystemTxStore,
        system_txs: Vec<EthPooledTransaction>,
    ) -> Result<EthBuiltPayload, eyre::Error> {
        let payload = prepare_block(node, system_tx_store, system_txs).await?;
        node.update_forkchoice(payload.block().hash(), payload.block().hash())
            .await?;

        Ok(payload)
    }

    pub async fn advance_blocks(
        node: &mut NodeHelperType<IrysEthereumNode>,
        system_tx_store: &SystemTxStore,
        system_txs: Vec<Vec<SystemTransaction>>,
        signer: &Arc<dyn alloy_network::TxSigner<Signature> + Send + Sync>,
        mut parent_blockhash: FixedBytes<32>,
        mut block_number: u64,
    ) -> Result<Vec<EthBuiltPayload>, eyre::Error> {
        let mut block_payloads = Vec::new();

        for system_txs_raw in system_txs.into_iter() {
            let mut system_txs = Vec::new();
            for mut system_tx in system_txs_raw {
                // set the metadata fields for the system tx
                system_tx.valid_for_block_height = block_number;
                system_tx.parent_blockhash = parent_blockhash;

                let system_tx = sign_system_tx(system_tx, signer).await?;
                system_txs.push(system_tx);
            }

            let block_payload = advance_block(node, system_tx_store, system_txs).await?;
            parent_blockhash = block_payload.block().hash();
            block_number += 1;
            block_payloads.push(block_payload);
        }

        Ok(block_payloads)
    }

    /// Compose a system tx for releasing stake.
    pub fn release_stake(
        address: Address,
        valid_for_block_height: u64,
        parent_blockhash: FixedBytes<32>,
    ) -> SystemTransaction {
        SystemTransaction {
            inner: TransactionPacket::ReleaseStake(system_tx::BalanceIncrement {
                amount: U256::ONE,
                target: address,
            }),
            valid_for_block_height,
            parent_blockhash,
        }
    }

    /// Compose a system tx for block reward.
    pub fn block_reward(
        address: Address,
        valid_for_block_height: u64,
        parent_blockhash: FixedBytes<32>,
    ) -> SystemTransaction {
        SystemTransaction {
            inner: TransactionPacket::BlockReward(system_tx::BalanceIncrement {
                amount: U256::ONE,
                target: address,
            }),
            valid_for_block_height,
            parent_blockhash,
        }
    }

    /// Compose a system tx for staking.
    pub fn stake(
        address: Address,
        valid_for_block_height: u64,
        parent_blockhash: FixedBytes<32>,
    ) -> SystemTransaction {
        SystemTransaction {
            inner: TransactionPacket::Stake(system_tx::BalanceDecrement {
                amount: U256::ONE,
                target: address,
            }),
            valid_for_block_height,
            parent_blockhash,
        }
    }

    /// Compose a system tx for storage fees.
    pub fn storage_fees(
        address: Address,
        valid_for_block_height: u64,
        parent_blockhash: FixedBytes<32>,
    ) -> SystemTransaction {
        SystemTransaction {
            inner: TransactionPacket::StorageFees(system_tx::BalanceDecrement {
                amount: U256::ONE,
                target: address,
            }),
            valid_for_block_height,
            parent_blockhash,
        }
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
        N::Types: NodeTypes<Primitives: FullNodePrimitives>,
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
        N::Types: NodeTypes<Primitives: FullNodePrimitives>,
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
    pub async fn sign_tx(
        mut tx_raw: TxLegacy,
        new_signer: &Arc<dyn alloy_network::TxSigner<Signature> + Send + Sync>,
    ) -> EthPooledTransaction<alloy_consensus::EthereumTxEnvelope<TxEip4844>> {
        let signed_tx = new_signer.sign_transaction(&mut tx_raw).await.unwrap();
        let tx = alloy_consensus::EthereumTxEnvelope::Legacy(tx_raw.into_signed(signed_tx))
            .try_into_recovered()
            .unwrap();

        EthPooledTransaction::new(tx.clone(), 300)
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

    /// Returns payload attributes for a given timestamp and parent block hash.
    pub fn eth_payload_attributes_with_parent(
        timestamp: u64,
        parent_block_hash: B256,
    ) -> EthPayloadBuilderAttributes {
        let attributes = PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
        };
        EthPayloadBuilderAttributes::new(parent_block_hash, attributes)
    }

    /// Launches and connects multiple Irys+reth nodes for integration tests.
    ///
    /// # Arguments
    /// - `num_nodes`: Addresses for allowed system tx origins (one per node)
    /// - `chain_spec`: Chain spec to use
    /// - `is_dev`: Whether to run in dev mode
    /// - `attributes_generator`: Function to generate payload attributes
    ///
    /// # Returns
    /// - `Vec<NodeHelperType<IrysEthereumNode>>`: Test node handles
    /// - `TaskManager`: Task manager for async tasks
    /// - `Wallet`: Default wallet for test accounts
    pub async fn setup_irys_reth(
        num_nodes: &[Address],
        chain_spec: Arc<<IrysEthereumNode as NodeTypes>::ChainSpec>,
        is_dev: bool,
        attributes_generator: impl Fn(u64) -> <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadBuilderAttributes + Send + Sync + Clone + 'static,
    ) -> eyre::Result<(
        Vec<(NodeHelperType<IrysEthereumNode>, SystemTxStore)>,
        TaskManager,
        Wallet,
    )>
    where
        LocalPayloadAttributesBuilder<<IrysEthereumNode as NodeTypes>::ChainSpec>:
            PayloadAttributesBuilder<
                <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadAttributes,
            >,
    {
        let tasks = TaskManager::current();
        let exec = tasks.executor();

        let network_config = NetworkArgs {
            discovery: DiscoveryArgs {
                disable_discovery: true,
                ..DiscoveryArgs::default()
            },
            ..NetworkArgs::default()
        };

        // Create nodes and peer them
        let mut nodes: Vec<(NodeTestContext<_, _>, SystemTxStore)> =
            Vec::with_capacity(num_nodes.len());

        for (idx, _producer) in num_nodes.iter().enumerate() {
            let node_config = NodeConfig::new(chain_spec.clone())
                .with_network(network_config.clone())
                .with_unused_ports()
                .with_rpc(RpcServerArgs::default().with_unused_ports().with_http())
                .set_dev(is_dev);

            let span = span!(Level::INFO, "node", idx);
            let _enter = span.enter();

            // Create the MPSC channel for system transaction requests
            let (system_tx_store, _system_tx_receiver) = SystemTxStore::new_with_notifications();

            let NodeHandle {
                node,
                node_exit_future: _,
            } = NodeBuilder::new(node_config.clone())
                .testing_node(exec.clone())
                .node(IrysEthereumNode {
                    system_tx_store: system_tx_store.clone(),
                })
                .launch()
                .await?;

            let mut node = NodeTestContext::new(node, attributes_generator.clone()).await?;

            // Connect each node in a chain.
            if let Some(previous_node) = nodes.last_mut() {
                previous_node.0.connect(&mut node).await;
            }

            // Connect last node with the first if there are more than two
            if idx + 1 == num_nodes.len() && num_nodes.len() > 2 {
                if let Some(first_node) = nodes.first_mut() {
                    node.connect(&mut first_node.0).await;
                }
            }

            nodes.push((node, system_tx_store));
        }

        Ok((
            nodes,
            tasks,
            Wallet::default().with_chain_id(chain_spec.chain().into()),
        ))
    }
}
