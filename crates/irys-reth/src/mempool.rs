use std::collections::HashSet;
use std::sync::Arc;
use std::time::SystemTime;

use alloy_consensus::Transaction as _;
use alloy_eips::{eip7840::BlobParams, merge::EPOCH_SLOTS};
use alloy_primitives::B256;
use futures::StreamExt as _;
use irys_types::UnixTimestamp;
use irys_types::chunk_provider::{PdChunkMessage, PdChunkSender};
use irys_types::hardfork_config::IrysHardforkConfig;
use reth::{
    api::FullNodeTypes,
    builder::{BuilderContext, components::PoolBuilder},
    primitives::SealedBlock,
    providers::StateProviderFactory,
    tasks::shutdown::GracefulShutdown,
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
    TransactionPool, TransactionValidationOutcome, TransactionValidator,
    blobstore::{DiskFileBlobStore, DiskFileBlobStoreConfig},
};
use reth_transaction_pool::{Priority as PoolPriority, TransactionOrdering};
use std::marker::PhantomData;
use tracing::{debug, info, trace};

use crate::IrysEthereumNode;

/// A custom pool builder for Irys shadow transaction validation and pool configuration.
#[derive(Clone)]
#[non_exhaustive]
pub struct IrysPoolBuilder {
    /// Hardfork configuration for checking Sprite activation.
    hardfork_config: Arc<IrysHardforkConfig>,
    /// PD chunk sender for mempool monitoring.
    /// The pool builder will spawn a monitoring task to detect
    /// PD transactions and send messages to the PdChunkManager.
    pd_chunk_sender: PdChunkSender,
}

impl std::fmt::Debug for IrysPoolBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrysPoolBuilder")
            .field("hardfork_config", &self.hardfork_config)
            .field("pd_chunk_sender", &"<sender>")
            .finish()
    }
}

impl IrysPoolBuilder {
    /// Creates a new pool builder with the given hardfork configuration.
    pub fn new(hardfork_config: Arc<IrysHardforkConfig>, pd_chunk_sender: PdChunkSender) -> Self {
        Self {
            hardfork_config,
            pd_chunk_sender,
        }
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

        // Clone hardfork_config for use in the PD monitoring task before moving it to ordering
        let hardfork_for_pd_monitor = hardfork_config.clone();

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

        // Spawn PD transaction monitoring task
        {
            let pool_clone = transaction_pool.clone();
            let sender = self.pd_chunk_sender;
            let hardfork_clone = hardfork_for_pd_monitor;
            ctx.task_executor()
                .spawn_critical_with_graceful_shutdown_signal(
                    "pd transaction monitoring task",
                    |shutdown| pd_transaction_monitor(pool_clone, sender, hardfork_clone, shutdown),
                );
            info!(target: "reth::cli", "PD transaction monitoring task spawned");
        }

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
            let access_list = tx.access_list().cloned().unwrap_or_default();
            if !matches!(
                crate::pd_tx::parse_pd_transaction(&access_list),
                crate::pd_tx::PdParseResult::NotPd
            ) {
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

        // Validate PD transaction structure and minimum fees when Sprite is active
        if self.is_sprite_active() {
            let access_list = tx.access_list().cloned().unwrap_or_default();
            match crate::pd_tx::parse_pd_transaction(&access_list) {
                crate::pd_tx::PdParseResult::InvalidPd(err) => {
                    tracing::trace!(
                        sender = ?tx.sender(),
                        tx_hash = ?tx.hash(),
                        ?err,
                        "PD transaction rejected: invalid PD metadata"
                    );
                    return Err(TransactionValidationOutcome::Invalid(
                        tx,
                        reth_transaction_pool::error::InvalidPoolTransactionError::Consensus(
                            InvalidTransactionError::TxTypeNotSupported,
                        ),
                    ));
                }
                crate::pd_tx::PdParseResult::ValidPd(meta) => {
                    // Calculate total fees in IRYS tokens:
                    // (base_fee_cap_per_chunk + priority_fee_per_chunk) × total_chunks
                    let total_per_chunk = meta
                        .base_fee_cap_per_chunk
                        .saturating_add(meta.priority_fee_per_chunk);
                    let total_fees = total_per_chunk
                        .saturating_mul(alloy_primitives::U256::from(meta.total_chunks));

                    // Basic sanity check: PD transactions must have non-zero fees.
                    // The full min_pd_transaction_cost validation (USD→IRYS conversion)
                    // happens at EVM execution time.
                    if total_fees.is_zero() {
                        tracing::trace!(
                            sender = ?tx.sender(),
                            tx_hash = ?tx.hash(),
                            chunks = meta.total_chunks,
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
                crate::pd_tx::PdParseResult::NotPd => {}
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
            let access_list = transaction.access_list().cloned().unwrap_or_default();
            match crate::pd_tx::parse_pd_transaction(&access_list) {
                crate::pd_tx::PdParseResult::ValidPd(meta) => {
                    alloy_primitives::U256::from(meta.total_chunks)
                        .saturating_mul(meta.priority_fee_per_chunk)
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

// ============================================================================
// PD Transaction Monitoring
// ============================================================================

/// Monitors the transaction pool for PD transactions and sends messages to the PdChunkManager.
///
/// This task is event-driven, using reth's pool listener APIs:
/// - Listens for new transactions entering the pool
/// - Detects PD transactions and extracts chunk specs from access lists
/// - Sends NewTransaction messages to start chunk fetching
/// - Listens for transaction lifecycle events (removals)
/// - Sends TransactionRemoved messages when PD transactions leave the pool
async fn pd_transaction_monitor<P>(
    pool: P,
    chunk_sender: PdChunkSender,
    hardfork_config: Arc<IrysHardforkConfig>,
    mut shutdown: GracefulShutdown,
) where
    P: TransactionPool,
    P::Transaction: EthPoolTransaction,
{
    use reth_transaction_pool::TransactionListenerKind;

    let mut known_pd_txs: HashSet<B256> = HashSet::new();
    let mut new_tx_listener = pool.new_transactions_listener_for(TransactionListenerKind::All);
    let mut all_events = pool.all_transactions_event_listener();

    debug!(target: "reth::pd", "PD transaction monitor started (event-driven)");

    loop {
        tokio::select! {
            biased;

            _ = &mut shutdown => {
                debug!(target: "reth::pd", "PD transaction monitor received shutdown signal");
                break;
            }

            // New transaction entered the pool
            Some(event) = new_tx_listener.recv() => {
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let timestamp = irys_types::UnixTimestamp::from_secs(now_secs);
                if !hardfork_config.is_sprite_active(timestamp) {
                    continue;
                }

                let tx = &event.transaction;
                let tx_hash = *tx.hash();

                if let Some(access_list) = tx.transaction.access_list()
                    && let crate::pd_tx::PdParseResult::ValidPd(meta) =
                        crate::pd_tx::parse_pd_transaction(access_list)
                    && known_pd_txs.insert(tx_hash)
                {
                    let chunk_specs = meta.data_reads;
                    if !chunk_specs.is_empty() {
                        trace!(
                            target: "reth::pd",
                            tx_hash = %tx_hash,
                            chunk_specs_count = chunk_specs.len(),
                            "Detected new PD transaction, sending for provisioning"
                        );
                        let _ = chunk_sender.send(PdChunkMessage::NewTransaction {
                            tx_hash,
                            chunk_specs,
                        });
                    }
                }
            }

            // Transaction lifecycle events (removals)
            Some(event) = all_events.next() => {
                use reth_transaction_pool::FullTransactionEvent;
                let removed_hash = match event {
                    FullTransactionEvent::Discarded(h)
                    | FullTransactionEvent::Invalid(h)
                    | FullTransactionEvent::Mined { tx_hash: h, .. } => Some(h),
                    FullTransactionEvent::Replaced { transaction, .. } => {
                        // `transaction` is the OLD tx being evicted (not the replacement)
                        Some(*transaction.hash())
                    }
                    _ => None,
                };
                if let Some(tx_hash) = removed_hash
                    && known_pd_txs.remove(&tx_hash)
                {
                    trace!(
                        target: "reth::pd",
                        tx_hash = %tx_hash,
                        "PD transaction removed from pool, notifying chunk manager"
                    );
                    let _ = chunk_sender.send(PdChunkMessage::TransactionRemoved { tx_hash });
                }
            }
        }
    }

    debug!(target: "reth::pd", "PD transaction monitor stopped");
}
