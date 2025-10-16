use std::time::SystemTime;

use alloy_eips::{eip7840::BlobParams, merge::EPOCH_SLOTS};
use reth::{
    api::FullNodeTypes,
    builder::{components::PoolBuilder, BuilderContext},
    primitives::{InvalidTransactionError, SealedBlock},
    providers::StateProviderFactory,
    transaction_pool::TransactionValidationTaskExecutor,
};
use reth_chainspec::{ChainSpec, ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_provider::CanonStateSubscriptions as _;
use reth_tracing::tracing;
use reth_transaction_pool::{
    blobstore::{DiskFileBlobStore, DiskFileBlobStoreConfig},
    CoinbaseTipOrdering, EthPoolTransaction, EthPooledTransaction, EthTransactionValidator, Pool,
    TransactionOrigin, TransactionValidator, TransactionValidationOutcome,
};
use tracing::info;

use crate::IrysEthereumNode;

/// A custom pool builder for Irys shadow transaction validation and pool configuration.
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
/// Notable changes from the original: we reject all shadow txs, as they are not allowed to land in a the pool.
impl<Node> PoolBuilder<Node> for IrysPoolBuilder
where
    Node: FullNodeTypes<Types = IrysEthereumNode>,
{
    type Pool = Pool<
        TransactionValidationTaskExecutor<
            IrysShadowTxValidator<Node::Provider, EthPooledTransaction>,
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
            validator: IrysShadowTxValidator {
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
pub struct IrysShadowTxValidator<Client, T> {
    eth_tx_validator: EthTransactionValidator<Client, T>,
}

impl<Client, Tx> IrysShadowTxValidator<Client, Tx>
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
                    sender = ?tx.sender(),
                    tx_hash = ?tx.hash(),
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

impl<Client, Tx> TransactionValidator for IrysShadowTxValidator<Client, Tx>
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
        let transaction = match self.prefilter_tx(transaction) {
            Ok(tx) => tx,
            Err(outcome) => return outcome,
        };

        tracing::trace!(hash = ?transaction.hash(), "non shadow tx, passing to eth validator");
        self.eth_tx_validator.validate_one(origin, transaction)
    }

    async fn validate_transactions(
        &self,
        transactions: Vec<(TransactionOrigin, Self::Transaction)>,
    ) -> Vec<TransactionValidationOutcome<Self::Transaction>> {
        transactions
            .into_iter()
            .map(|(origin, tx)| match self.prefilter_tx(tx) {
                Ok(tx) => self.eth_tx_validator.validate_one(origin, tx),
                Err(outcome) => outcome,
            })
            .collect()
    }

    fn on_new_head_block<B>(&self, new_tip_block: &SealedBlock<B>)
    where
        B: reth_primitives_traits::Block,
    {
        self.eth_tx_validator.on_new_head_block(new_tip_block);
    }
}
