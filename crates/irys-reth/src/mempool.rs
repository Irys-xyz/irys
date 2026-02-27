use std::sync::Arc;
use std::time::SystemTime;

use crate::pd_tx::sum_pd_chunks_in_access_list;
use alloy_eips::{eip7840::BlobParams, merge::EPOCH_SLOTS};
use irys_types::UnixTimestamp;
use irys_types::hardfork_config::IrysHardforkConfig;
use reth::{
    api::FullNodeTypes,
    builder::{BuilderContext, components::PoolBuilder},
    primitives::SealedBlock,
    providers::StateProviderFactory,
    transaction_pool::TransactionValidationTaskExecutor,
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_ethereum_primitives::EthPrimitives;
use reth_evm::ConfigureEvm;
use reth_primitives_traits::BlockTy;
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_provider::CanonStateSubscriptions as _;
use reth_tracing::tracing;
use reth_transaction_pool::{
    EthPoolTransaction, EthPooledTransaction, EthTransactionValidator, Pool, TransactionOrigin,
    TransactionValidationOutcome, TransactionValidator,
    blobstore::{DiskFileBlobStore, DiskFileBlobStoreConfig},
};
use reth_transaction_pool::{Priority as PoolPriority, TransactionOrdering};
use std::marker::PhantomData;
use tracing::info;

use crate::IrysEthereumNode;

/// A custom pool builder for Irys shadow transaction validation and pool configuration.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct IrysPoolBuilder {
    /// Hardfork configuration for checking Sprite activation.
    hardfork_config: Arc<IrysHardforkConfig>,
}

impl IrysPoolBuilder {
    /// Creates a new pool builder with the given hardfork configuration.
    pub fn new(hardfork_config: Arc<IrysHardforkConfig>) -> Self {
        Self { hardfork_config }
    }
}

/// Implement the [`PoolBuilder`] trait for the Irys pool builder
///
/// This will be used to build the transaction pool and its maintenance tasks during launch.
///
/// Original code from:
/// <https://github.com/Irys-xyz/reth-irys/blob/67abdf25dda69a660d44040d4493421b93d8de7b/crates/ethereum/node/src/node.rs?plain=1#L322>
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
        PdAwareCoinbaseTipOrdering<EthPooledTransaction>,
        DiskFileBlobStore,
    >;

    async fn build_pool(
        self,
        ctx: &BuilderContext<Node>,
        evm_config: Evm,
    ) -> eyre::Result<Self::Pool> {
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
        let hardfork_config = self.hardfork_config;
        let validator =
            TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone(), evm_config)
                .kzg_settings(ctx.kzg_settings()?)
                .with_local_transactions_config(pool_config.local_transactions_config.clone())
                .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
                .with_additional_tasks(ctx.config().txpool.additional_validation_tasks)
                .build_with_tasks(ctx.task_executor().clone(), blob_store.clone())
                .map(|eth_validator| IrysShadowTxValidator {
                    eth_tx_validator: eth_validator,
                    hardfork_config: hardfork_config.clone(),
                });

        let ordering = PdAwareCoinbaseTipOrdering::new(hardfork_config);
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
            ctx.task_executor().spawn_critical_task(
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

#[derive(Debug)]
pub struct IrysShadowTxValidator<Client, T, Evm> {
    eth_tx_validator: EthTransactionValidator<Client, T, Evm>,
    /// Hardfork configuration for checking Sprite activation.
    hardfork_config: Arc<IrysHardforkConfig>,
}

impl<Client, Tx, Evm> IrysShadowTxValidator<Client, Tx, Evm>
where
    Tx: EthPoolTransaction,
{
    /// Returns true if Sprite hardfork is currently active based on system time.
    fn is_sprite_active(&self) -> bool {
        let current_time = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.hardfork_config
            .is_sprite_active(UnixTimestamp::from_secs(current_time))
    }

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

        // Reject PD transactions before Sprite hardfork is active
        if !self.is_sprite_active() {
            if let Ok(Some(_)) = crate::pd_tx::detect_and_decode_pd_header(input) {
                tracing::trace!(
                    sender = ?tx.sender(),
                    tx_hash = ?tx.hash(),
                    "PD transaction rejected: Sprite hardfork not active"
                );
                return Err(TransactionValidationOutcome::Invalid(
                    tx,
                    reth_transaction_pool::error::InvalidPoolTransactionError::Consensus(
                        InvalidTransactionError::TxTypeNotSupported,
                    ),
                ));
            }
        }

        // Validate minimum PD transaction cost when Sprite is active
        if self.is_sprite_active() {
            if let Ok(Some((pd_header, _))) = crate::pd_tx::detect_and_decode_pd_header(input) {
                // Count PD chunks from access list
                let chunks = tx
                    .access_list()
                    .map(sum_pd_chunks_in_access_list)
                    .unwrap_or(0);

                // Calculate total fees in IRYS tokens:
                // (max_base_fee_per_chunk + max_priority_fee_per_chunk) × chunks
                let total_per_chunk = pd_header
                    .max_base_fee_per_chunk
                    .saturating_add(pd_header.max_priority_fee_per_chunk);
                let total_fees =
                    total_per_chunk.saturating_mul(alloy_primitives::U256::from(chunks));

                // Basic sanity check: PD transactions must have non-zero fees.
                // The full min_pd_transaction_cost validation (USD→IRYS conversion)
                // happens at EVM execution time.
                if total_fees.is_zero() {
                    tracing::trace!(
                        sender = ?tx.sender(),
                        tx_hash = ?tx.hash(),
                        chunks = chunks,
                        "PD transaction rejected: zero total fees"
                    );
                    return Err(TransactionValidationOutcome::Invalid(
                        tx,
                        reth_transaction_pool::error::InvalidPoolTransactionError::Consensus(
                            InvalidTransactionError::FeeCapTooLow,
                        ),
                    ));
                }
            }
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

        tracing::trace!(hash = ?transaction.hash(), "non shadow tx, passing to eth validator");
        self.eth_tx_validator.validate_one(origin, transaction)
    }

    fn on_new_head_block(&self, new_tip_block: &SealedBlock<Self::Block>) {
        self.eth_tx_validator.on_new_head_block(new_tip_block);
    }
}

/// PD-aware ordering that ranks transactions by effective fee rate, combining the regular gas
/// priority fee per gas and the PD priority fee per chunk (if a PD header is present).
#[derive(Debug)]
pub struct PdAwareCoinbaseTipOrdering<T> {
    /// Hardfork configuration for checking Sprite activation.
    hardfork_config: Arc<IrysHardforkConfig>,
    _phantom: PhantomData<T>,
}

impl<T> PdAwareCoinbaseTipOrdering<T> {
    /// Creates a new ordering with the given hardfork configuration.
    pub fn new(hardfork_config: Arc<IrysHardforkConfig>) -> Self {
        Self {
            hardfork_config,
            _phantom: PhantomData,
        }
    }

    /// Returns true if Sprite hardfork is currently active based on system time.
    fn is_sprite_active(&self) -> bool {
        let current_time = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.hardfork_config
            .is_sprite_active(UnixTimestamp::from_secs(current_time))
    }
}

impl<T> Clone for PdAwareCoinbaseTipOrdering<T> {
    fn clone(&self) -> Self {
        Self {
            hardfork_config: self.hardfork_config.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<T> TransactionOrdering for PdAwareCoinbaseTipOrdering<T>
where
    T: EthPoolTransaction + 'static,
{
    type PriorityValue = alloy_primitives::U256;
    type Transaction = T;

    fn priority(
        &self,
        transaction: &Self::Transaction,
        base_fee: u64,
    ) -> PoolPriority<Self::PriorityValue> {
        // Base gas tip per gas
        let gas_tip = transaction
            .effective_tip_per_gas(base_fee)
            .map(alloy_primitives::U256::from)
            .unwrap_or_default();

        // PD header max priority fee per chunk and chunk count, if present.
        // Only consider PD tips when Sprite hardfork is active.
        let pd_total_tip = if self.is_sprite_active() {
            match crate::pd_tx::detect_and_decode_pd_header(transaction.input()) {
                Ok(Some((hdr, _))) => {
                    let pd_tip_per_chunk = hdr.max_priority_fee_per_chunk;
                    // Always infer PD chunk count from the access list.
                    let chunks_u64 = transaction
                        .access_list()
                        .map(sum_pd_chunks_in_access_list)
                        .unwrap_or(0_u64);
                    alloy_primitives::U256::from(chunks_u64).saturating_mul(pd_tip_per_chunk)
                }
                _ => alloy_primitives::U256::ZERO,
            }
        } else {
            alloy_primitives::U256::ZERO
        };

        // Effective priority: gas tip + total PD tip (per-chunk tip * chunk count).
        let effective = gas_tip.saturating_add(pd_total_tip);
        PoolPriority::Value(effective)
    }
}
