//! A basic Ethereum payload builder implementation adapted for Irys.
//! Original impl: [here](https://github.com/paradigmxyz/reth/blob/2b283ae83f6c68b4c851206f8cd01491f63bb608/crates/ethereum/payload/src/lib.rs#L53)
//!
//! ## High-level flow
//! During block production the payload builder pulls transactions from multiple sources:
//! 1. **Shadow transactions**: bundled by the block producer outside of the mempool. These must
//!    be emitted first to guarantee deterministic inclusion.
//! 2. **Pool transactions**: standard mempool ordering (priority-driven) fetched via the
//!    `BestTransactions` iterator.
//!
//! Both sources feed into [`CombinedTransactionIterator`], which decorates the pool iterator with
//! the additional logic required for shadow transactions and Programmable Data (PD) constraints.
//!
//! ## PD transactions and chunk budgeting
//! PD-aware transactions embed a PD header and list required data chunks in their access list. The
//! builder must avoid exceeding `MAX_PD_CHUNKS_PER_BLOCK` (currently 7,500) when assembling a block.
//! Each candidate transaction is scanned for PD metadata; if it carries PD state we add its chunk
//! demand to the running total and include it only if the budget still permits.
//!
//! When the budget would be exceeded we *defer* the PD transaction by placing it on a queue owned by
//! the PD budget manager. This allows the iterator to keep yielding subsequent pool transactions
//! (including non-PD ones) and avoids losing track of the over-budget PD candidate.
//!
//! ## Why deferring matters
//! The EVM can later reject a transaction that was already emitted by the iterator (for example,
//! due to state inconsistencies), which triggers `mark_invalid`. In that case we restore the PD
//! chunk budget for that transaction, then re-check the deferred queue: if a previously-skipped PD
//! transaction now fits within the freed capacity it becomes the next candidate. This ensures the
//! builder respects mempool ordering while still honoring the PD limit.

use crate::IrysPayloadBuilderAttributes;
use crate::pd_tx::{detect_and_decode_pd_header, sum_pd_chunks_in_access_list};
use alloy_consensus::Transaction as _;
use irys_types::hardfork_config::IrysHardforkConfig;
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_chainspec::{ChainSpecProvider, EthereumHardforks};
use reth_ethereum_primitives::EthPrimitives;
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_evm_ethereum::EthEvmConfig;
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::PayloadBuilderAttributes as _;
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, EthPooledTransaction, TransactionOrigin,
    TransactionPool, ValidPoolTransaction,
    error::InvalidPoolTransactionError,
    identifier::{SenderId, TransactionId},
};
use revm_primitives::FixedBytes;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Instant,
};

use reth_ethereum_payload_builder::EthereumBuilderConfig;

// Additional imports for irys_ethereum_payload
use crate::IrysBuiltPayload;
use crate::evm::TREASURY_ACCOUNT;
use alloy_evm::Evm as _;
use alloy_primitives::U256;
use alloy_rlp::Encodable as _;
use reth_basic_payload_builder::is_better_payload;
use reth_chainspec::EthChainSpec as _;
use reth_consensus_common::validation::MAX_RLP_BLOCK_SIZE;
use reth_errors::{BlockExecutionError, BlockValidationError, ConsensusError};
use reth_evm::execute::{BlockBuilder as _, BlockBuilderOutcome};
use reth_payload_builder::{BlobSidecars, EthPayloadBuilderAttributes};
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_transaction_pool::error::Eip4844PoolTransactionError;
use revm::context_interface::Block as _;
use revm::database_interface::Database as _;
use tracing::{debug, trace, warn};

type BestTransactionsIter =
    Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<EthPooledTransaction>>>>;

/// Constructs an Irys payload including treasury balance.
///
/// This is a modified version of `default_ethereum_payload` that additionally extracts
/// the treasury balance from EVM state after transaction execution. The treasury balance
/// is included in the returned `IrysBuiltPayload` for use by the block producer.
///
/// Post-Sprite, the treasury is an EVM account (TREASURY_ACCOUNT) whose balance changes
/// through shadow transactions and PD fees. Pre-Sprite, we return zero as treasury is
/// tracked externally.
pub fn irys_ethereum_payload<EvmConfig, Client, F>(
    evm_config: EvmConfig,
    client: Client,
    builder_config: EthereumBuilderConfig,
    args: BuildArguments<EthPayloadBuilderAttributes, IrysBuiltPayload>,
    best_txs: F,
    is_sprite_active: bool,
) -> Result<BuildOutcome<IrysBuiltPayload>, PayloadBuilderError>
where
    EvmConfig: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks>,
    F: FnOnce(BestTransactionsAttributes) -> BestTransactionsIter,
{
    let BuildArguments {
        mut cached_reads,
        config,
        cancel,
        best_payload,
    } = args;
    let PayloadConfig {
        parent_header,
        attributes,
    } = config;

    let state_provider = client.state_by_block_hash(parent_header.hash())?;
    let state = StateProviderDatabase::new(&state_provider);
    let mut db = State::builder()
        .with_database(cached_reads.as_db_mut(state))
        .with_bundle_update()
        .build();

    let mut builder = evm_config
        .builder_for_next_block(
            &mut db,
            &parent_header,
            NextBlockEnvAttributes {
                timestamp: attributes.timestamp(),
                suggested_fee_recipient: attributes.suggested_fee_recipient(),
                prev_randao: attributes.prev_randao(),
                gas_limit: builder_config.gas_limit(parent_header.gas_limit),
                parent_beacon_block_root: attributes.parent_beacon_block_root(),
                withdrawals: Some(attributes.withdrawals().clone()),
                extra_data: builder_config.extra_data.clone(),
            },
        )
        .map_err(PayloadBuilderError::other)?;

    let chain_spec = client.chain_spec();

    debug!(target: "payload_builder", id=%attributes.id, parent_header = ?parent_header.hash(), parent_number = parent_header.number, "building new payload");
    let mut cumulative_gas_used = 0;
    let block_gas_limit: u64 = builder.evm_mut().block().gas_limit();
    let base_fee = builder.evm_mut().block().basefee();

    let mut best_txs = best_txs(BestTransactionsAttributes::new(
        base_fee,
        builder
            .evm_mut()
            .block()
            .blob_gasprice()
            .map(|gasprice| gasprice as u64),
    ));
    let mut total_fees = U256::ZERO;

    builder.apply_pre_execution_changes().map_err(|err| {
        warn!(target: "payload_builder", %err, "failed to apply pre-execution changes");
        PayloadBuilderError::Internal(err.into())
    })?;

    let blob_sidecars = BlobSidecars::Empty;
    let mut block_blob_count = 0;
    let mut block_transactions_rlp_length = 0;

    let blob_params = chain_spec.blob_params_at_timestamp(attributes.timestamp());
    let protocol_max_blob_count = blob_params
        .as_ref()
        .map(|params| params.max_blob_count)
        .unwrap_or_default();

    let max_blob_count = builder_config
        .max_blobs_per_block
        .map(|user_limit| std::cmp::min(user_limit, protocol_max_blob_count).max(1))
        .unwrap_or(protocol_max_blob_count);

    let is_osaka = chain_spec.is_osaka_active_at_timestamp(attributes.timestamp());

    while let Some(pool_tx) = best_txs.next() {
        if cumulative_gas_used + pool_tx.gas_limit() > block_gas_limit {
            best_txs.mark_invalid(
                &pool_tx,
                &InvalidPoolTransactionError::ExceedsGasLimit(pool_tx.gas_limit(), block_gas_limit),
            );
            continue;
        }

        if cancel.is_cancelled() {
            return Ok(BuildOutcome::Cancelled);
        }

        let tx = pool_tx.to_consensus();

        let estimated_block_size_with_tx = block_transactions_rlp_length
            + tx.inner().length()
            + attributes.withdrawals().length()
            + 1024;

        if is_osaka && estimated_block_size_with_tx > MAX_RLP_BLOCK_SIZE {
            best_txs.mark_invalid(
                &pool_tx,
                &InvalidPoolTransactionError::OversizedData {
                    size: estimated_block_size_with_tx,
                    limit: MAX_RLP_BLOCK_SIZE,
                },
            );
            continue;
        }

        // Blob transaction handling - Irys doesn't use blob transactions, so we skip them
        if let Some(blob_tx) = tx.as_eip4844() {
            let tx_blob_count = blob_tx.tx().blob_versioned_hashes.len() as u64;

            if block_blob_count + tx_blob_count > max_blob_count {
                trace!(target: "payload_builder", tx=?tx.hash(), ?block_blob_count, "skipping blob transaction because it would exceed the max blob count per block");
                best_txs.mark_invalid(
                    &pool_tx,
                    &InvalidPoolTransactionError::Eip4844(
                        Eip4844PoolTransactionError::TooManyEip4844Blobs {
                            have: block_blob_count + tx_blob_count,
                            permitted: max_blob_count,
                        },
                    ),
                );
                continue;
            }
            // Note: We don't fetch blob sidecars here as Irys doesn't use blob transactions.
        }

        let gas_used = match builder.execute_transaction(tx.clone()) {
            Ok(gas_used) => gas_used,
            Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                error, ..
            })) => {
                if error.is_nonce_too_low() {
                    trace!(target: "payload_builder", %error, ?tx, "skipping nonce too low transaction");
                } else {
                    trace!(target: "payload_builder", %error, ?tx, "skipping invalid transaction and its descendants");
                    best_txs.mark_invalid(
                        &pool_tx,
                        &InvalidPoolTransactionError::Consensus(
                            InvalidTransactionError::TxTypeNotSupported,
                        ),
                    );
                }
                continue;
            }
            Err(err) => return Err(PayloadBuilderError::evm(err)),
        };

        if let Some(blob_tx) = tx.as_eip4844() {
            block_blob_count += blob_tx.tx().blob_versioned_hashes.len() as u64;
            if block_blob_count == max_blob_count {
                best_txs.skip_blobs();
            }
        }

        block_transactions_rlp_length += tx.inner().length();

        let miner_fee = tx
            .effective_tip_per_gas(base_fee)
            .expect("fee is always valid; execution succeeded");
        total_fees += U256::from(miner_fee) * U256::from(gas_used);
        cumulative_gas_used += gas_used;
        // Note: Blob sidecars are not collected as Irys doesn't use blob transactions
    }

    // Check if we have a better payload
    if !is_better_payload(best_payload.as_ref(), total_fees) {
        drop(builder);
        return Ok(BuildOutcome::Aborted {
            fees: total_fees,
            cached_reads,
        });
    }

    let BlockBuilderOutcome {
        execution_result,
        block,
        ..
    } = builder.finish(&state_provider)?;

    // Extract treasury balance from EVM state after transaction execution
    let treasury_balance = if is_sprite_active {
        // Query TREASURY_ACCOUNT balance from the post-execution state
        // Use db.basic() which queries both bundle_state (modified accounts) AND
        // the underlying StateProviderDatabase (parent state)
        let treasury_addr = *TREASURY_ACCOUNT;
        let treasury_account = db.basic(treasury_addr);
        treasury_account
            .ok()
            .flatten()
            .map(|info| info.balance)
            .unwrap_or(U256::ZERO)
    } else {
        // Pre-Sprite: treasury tracked externally by shadow_tx_generator
        U256::ZERO
    };

    let requests = chain_spec
        .is_prague_active_at_timestamp(attributes.timestamp())
        .then_some(execution_result.requests);

    let sealed_block = Arc::new(block.sealed_block().clone());
    debug!(target: "payload_builder", id=%attributes.id, sealed_block_header = ?sealed_block.sealed_header(), treasury_balance = %treasury_balance, "sealed built block with treasury");

    if is_osaka && sealed_block.rlp_length() > MAX_RLP_BLOCK_SIZE {
        return Err(PayloadBuilderError::other(ConsensusError::BlockTooLarge {
            rlp_length: sealed_block.rlp_length(),
            max_rlp_length: MAX_RLP_BLOCK_SIZE,
        }));
    }

    // Create the inner EthBuiltPayload
    let eth_payload = reth_payload_builder::EthBuiltPayload::new(
        attributes.id,
        sealed_block,
        total_fees,
        requests,
    )
    .with_sidecars(blob_sidecars);

    // Wrap in IrysBuiltPayload with treasury balance
    let payload = IrysBuiltPayload::new(eth_payload, treasury_balance);

    Ok(BuildOutcome::Better {
        payload,
        cached_reads,
    })
}

/// Ethereum payload builder
#[derive(Debug, Clone)]
pub struct IrysPayloadBuilder<Pool, Client, EvmConfig = EthEvmConfig> {
    /// Client providing access to node state.
    client: Client,
    /// Transaction pool.
    pool: Pool,
    /// The type responsible for creating the evm.
    evm_config: EvmConfig,
    /// Payload builder configuration.
    builder_config: EthereumBuilderConfig,
    /// Maximum number of PD chunks that can be included in a single block.
    max_pd_chunks_per_block: u64,
    /// Hardfork configuration for determining Sprite activation.
    hardforks: Arc<IrysHardforkConfig>,
}

/// Combined iterator that yields shadow transactions first, then pool transactions
pub struct CombinedTransactionIterator {
    shadow: ShadowTxQueue,
    pd_budget: PdChunkBudget,
    pool_iter: BestTransactionsIter,
    /// Whether the Sprite hardfork is active (enables PD chunk budgeting)
    is_sprite_active: bool,
}

struct ShadowTxQueue {
    pending: VecDeque<Arc<ValidPoolTransaction<EthPooledTransaction>>>,
    hashes: HashSet<FixedBytes<32>>,
}

impl ShadowTxQueue {
    fn from_shadow_transactions(timestamp: Instant, shadow_txs: Vec<EthPooledTransaction>) -> Self {
        let pending = shadow_txs
            .into_iter()
            .map(|tx| ValidPoolTransaction {
                // todo - recheck the SenderId::from(0) - what are the consequences of using `0`
                transaction_id: TransactionId::new(SenderId::from(0), tx.nonce()),
                transaction: tx,
                propagate: false,
                timestamp,
                origin: TransactionOrigin::Private,
                authority_ids: None,
            })
            .map(Arc::new)
            .collect::<VecDeque<_>>();
        let hashes = pending.iter().map(|tx| *tx.hash()).collect::<HashSet<_>>();

        Self { pending, hashes }
    }

    fn pop_front(&mut self) -> Option<Arc<ValidPoolTransaction<EthPooledTransaction>>> {
        self.pending.pop_front()
    }

    fn contains_hash(&self, hash: &FixedBytes<32>) -> bool {
        self.hashes.contains(hash)
    }
}

struct PdChunkBudget {
    max: u64,
    used: u64,
    deferred: VecDeque<Arc<ValidPoolTransaction<EthPooledTransaction>>>,
    accounted: HashMap<FixedBytes<32>, u64>,
}

impl PdChunkBudget {
    fn new(max_pd_chunks_per_block: u64) -> Self {
        Self {
            max: max_pd_chunks_per_block,
            used: 0,
            deferred: VecDeque::new(),
            accounted: HashMap::new(),
        }
    }

    fn can_fit(&self, chunks: u64) -> bool {
        self.used.saturating_add(chunks) <= self.max
    }

    fn try_consume(
        &mut self,
        tx: &Arc<ValidPoolTransaction<EthPooledTransaction>>,
        chunks: u64,
    ) -> bool {
        if self.can_fit(chunks) {
            self.used = self.used.saturating_add(chunks);
            self.accounted.insert(*tx.hash(), chunks);
            return true;
        }
        false
    }

    fn defer(&mut self, tx: Arc<ValidPoolTransaction<EthPooledTransaction>>) {
        self.deferred.push_back(tx);
    }

    fn pop_ready_deferred(
        &mut self,
        mut chunk_counter: impl FnMut(&ValidPoolTransaction<EthPooledTransaction>) -> u64,
    ) -> Option<Arc<ValidPoolTransaction<EthPooledTransaction>>> {
        if let Some(candidate) = self.deferred.front() {
            let chunks = chunk_counter(candidate.as_ref());
            if self.can_fit(chunks) {
                return self.deferred.pop_front();
            }
        }
        None
    }

    fn on_invalid(&mut self, tx: &Arc<ValidPoolTransaction<EthPooledTransaction>>) {
        let hash = *tx.hash();
        if let Some(chunks) = self.accounted.remove(&hash) {
            self.used = self.used.saturating_sub(chunks);
            return;
        }

        if let Some(pos) = self
            .deferred
            .iter()
            .position(|candidate| candidate.hash() == tx.hash())
        {
            self.deferred.remove(pos);
        }
    }
}

impl CombinedTransactionIterator {
    /// Count PD chunks for a transaction. Returns 0 if Sprite hardfork is not active.
    fn pd_chunks_for_transaction(
        &self,
        transaction: &ValidPoolTransaction<EthPooledTransaction>,
    ) -> u64 {
        // Skip PD chunk counting if Sprite hardfork is not active
        if !self.is_sprite_active {
            return 0;
        }

        match detect_and_decode_pd_header(transaction.transaction.input()) {
            Ok(Some(_)) => transaction
                .transaction
                .access_list()
                .map(sum_pd_chunks_in_access_list)
                .unwrap_or_default(),
            _ => 0,
        }
    }

    pub fn new(
        timestamp: Instant,
        shadow_txs: Vec<EthPooledTransaction>,
        pool_iter: BestTransactionsIter,
        max_pd_chunks_per_block: u64,
        is_sprite_active: bool,
    ) -> Self {
        Self {
            shadow: ShadowTxQueue::from_shadow_transactions(timestamp, shadow_txs),
            pd_budget: PdChunkBudget::new(max_pd_chunks_per_block),
            pool_iter,
            is_sprite_active,
        }
    }

    fn next_candidate(&mut self) -> Option<Arc<ValidPoolTransaction<EthPooledTransaction>>> {
        if let Some(tx) = self.shadow.pop_front() {
            return Some(tx);
        }

        // Check deferred PD transactions - need to capture is_sprite_active for closure
        let is_sprite_active = self.is_sprite_active;
        if let Some(tx) = self.pd_budget.pop_ready_deferred(|tx| {
            if !is_sprite_active {
                return 0;
            }
            match detect_and_decode_pd_header(tx.transaction.input()) {
                Ok(Some(_)) => tx
                    .transaction
                    .access_list()
                    .map(sum_pd_chunks_in_access_list)
                    .unwrap_or_default(),
                _ => 0,
            }
        }) {
            return Some(tx);
        }

        self.pool_iter.next()
    }
}

impl Iterator for CombinedTransactionIterator {
    type Item = Arc<ValidPoolTransaction<EthPooledTransaction>>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(tx) = self.next_candidate() {
            let chunks = self.pd_chunks_for_transaction(tx.as_ref());
            if chunks == 0 || self.pd_budget.try_consume(&tx, chunks) {
                return Some(tx);
            }

            self.pd_budget.defer(tx);
        }

        None
    }
}

impl BestTransactions for CombinedTransactionIterator {
    fn mark_invalid(&mut self, transaction: &Self::Item, kind: &InvalidPoolTransactionError) {
        if self.shadow.contains_hash(transaction.hash()) {
            // Shadow txs are already removed from the queue, so we don't need to do anything
            // NOTE FOR READER: if you refactor the code here, ensure that we *never*
            // try to mark a shadow tx as invalid by calling the underlying pool_iter.
            // This for some reason `clear` the whole pool_iter.
            return;
        }

        self.pd_budget.on_invalid(transaction);

        // For pool transactions, delegate to the pool iterator
        self.pool_iter.mark_invalid(transaction, kind);
    }

    fn no_updates(&mut self) {
        self.pool_iter.no_updates();
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        self.pool_iter.set_skip_blobs(skip_blobs);
    }
}

impl<Pool, Client, EvmConfig> IrysPayloadBuilder<Pool, Client, EvmConfig> {
    /// `IrysPayloadBuilder` constructor.
    pub fn new(
        client: Client,
        pool: Pool,
        evm_config: EvmConfig,
        builder_config: EthereumBuilderConfig,
        max_pd_chunks_per_block: u64,
        hardforks: Arc<IrysHardforkConfig>,
    ) -> Self {
        Self {
            client,
            pool,
            evm_config,
            builder_config,
            max_pd_chunks_per_block,
            hardforks,
        }
    }
}

// Default implementation of [PayloadBuilder] for unit type
impl<Pool, Client, EvmConfig> IrysPayloadBuilder<Pool, Client, EvmConfig>
where
    EvmConfig: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks> + Clone,
    Pool: TransactionPool<Transaction = EthPooledTransaction>,
{
    /// Creates a combined transaction iterator with shadow transactions first, then pool transactions.
    ///
    /// Shadow transactions are passed directly from the payload attributes.
    pub fn best_transactions_with_attributes(
        &self,
        attributes: BestTransactionsAttributes,
        shadow_txs: Vec<EthPooledTransaction>,
        is_sprite_active: bool,
    ) -> BestTransactionsIter {
        let timestamp = Instant::now();

        // Get pool transactions iterator
        let pool_txs = self.pool.best_transactions_with_attributes(attributes);

        // Create combined iterator with shadow txs from attributes
        Box::new(CombinedTransactionIterator::new(
            timestamp,
            shadow_txs,
            pool_txs,
            self.max_pd_chunks_per_block,
            is_sprite_active,
        ))
    }
}

// Default implementation of [PayloadBuilder] for unit type
impl<Pool, Client, EvmConfig> PayloadBuilder for IrysPayloadBuilder<Pool, Client, EvmConfig>
where
    EvmConfig: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks> + Clone,
    Pool: TransactionPool<Transaction = EthPooledTransaction>,
{
    type Attributes = IrysPayloadBuilderAttributes;
    type BuiltPayload = IrysBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<IrysPayloadBuilderAttributes, IrysBuiltPayload>,
    ) -> Result<BuildOutcome<IrysBuiltPayload>, PayloadBuilderError> {
        // Extract shadow transactions from attributes
        let shadow_txs = args.config.attributes.shadow_txs.clone();

        // Check if Sprite hardfork is active at this block's timestamp
        let block_timestamp =
            irys_types::UnixTimestamp::from_secs(args.config.attributes.timestamp());
        let is_sprite_active = self.hardforks.is_sprite_active(block_timestamp);

        // Convert IrysPayloadBuilderAttributes to EthPayloadBuilderAttributes for the inner builder
        let BuildArguments {
            cached_reads,
            config,
            cancel,
            best_payload,
        } = args;
        let PayloadConfig {
            parent_header,
            attributes,
        } = config;
        let eth_args = BuildArguments {
            cached_reads,
            config: PayloadConfig {
                parent_header,
                attributes: attributes.inner,
            },
            cancel,
            best_payload,
        };
        irys_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.builder_config.clone(),
            eth_args,
            |attributes| {
                self.best_transactions_with_attributes(
                    attributes,
                    shadow_txs.clone(),
                    is_sprite_active,
                )
            },
            is_sprite_active,
        )
    }

    fn on_missing_payload(
        &self,
        _args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> MissingPayloadBehaviour<Self::BuiltPayload> {
        if self.builder_config.await_payload_on_missing {
            MissingPayloadBehaviour::AwaitInProgress
        } else {
            MissingPayloadBehaviour::RaceEmptyPayload
        }
    }

    fn build_empty_payload(
        &self,
        config: PayloadConfig<Self::Attributes>,
    ) -> Result<IrysBuiltPayload, PayloadBuilderError> {
        // Extract shadow transactions from attributes
        let shadow_txs = config.attributes.shadow_txs.clone();

        // Check if Sprite hardfork is active at this block's timestamp
        let block_timestamp = irys_types::UnixTimestamp::from_secs(config.attributes.timestamp());
        let is_sprite_active = self.hardforks.is_sprite_active(block_timestamp);

        // Convert IrysPayloadBuilderAttributes to EthPayloadBuilderAttributes for the inner builder
        let PayloadConfig {
            parent_header,
            attributes,
        } = config;
        let eth_config = PayloadConfig {
            parent_header,
            attributes: attributes.inner,
        };
        let args = BuildArguments::new(Default::default(), eth_config, Default::default(), None);

        irys_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.builder_config.clone(),
            args,
            |attributes| {
                self.best_transactions_with_attributes(
                    attributes,
                    shadow_txs.clone(),
                    is_sprite_active,
                )
            },
            is_sprite_active,
        )?
        .into_payload()
        .ok_or_else(|| PayloadBuilderError::MissingPayload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pd_tx::{PdHeaderV1, build_pd_access_list, prepend_pd_header_v1_to_calldata};
    use alloy_primitives::U256;
    use alloy_primitives::aliases::U200;
    use irys_types::range_specifier::ChunkRangeSpecifier;
    use rand09::{SeedableRng as _, rngs::StdRng};
    use reth_primitives_traits::SignedTransaction;
    use reth_transaction_pool::{PoolTransaction as _, test_utils::TransactionGenerator};
    use std::collections::VecDeque;

    fn pd_pooled_transaction(
        generator: &mut TransactionGenerator<StdRng>,
        nonce: u64,
        chunk_count: u16,
    ) -> EthPooledTransaction {
        let header = PdHeaderV1 {
            max_priority_fee_per_chunk: U256::from(1),
            max_base_fee_per_chunk: U256::from(1),
        };
        let calldata = prepend_pd_header_v1_to_calldata(&header, &[]);
        let access_list = build_pd_access_list([ChunkRangeSpecifier {
            partition_index: U200::ZERO,
            offset: 0,
            chunk_count,
        }]);
        let signed = generator
            .transaction()
            .nonce(nonce)
            .access_list(access_list)
            .input(calldata)
            .into_eip1559();
        let recovered = SignedTransaction::try_into_recovered(signed).unwrap();
        EthPooledTransaction::try_from_consensus(recovered).unwrap()
    }

    fn normal_pooled_transaction(
        generator: &mut TransactionGenerator<StdRng>,
        nonce: u64,
    ) -> EthPooledTransaction {
        let signed = generator.transaction().nonce(nonce).into_eip1559();
        let recovered = SignedTransaction::try_into_recovered(signed).unwrap();
        EthPooledTransaction::try_from_consensus(recovered).unwrap()
    }

    fn make_valid(
        tx: EthPooledTransaction,
        sender: u64,
        timestamp: Instant,
    ) -> Arc<ValidPoolTransaction<EthPooledTransaction>> {
        Arc::new(ValidPoolTransaction {
            transaction_id: TransactionId::new(SenderId::from(sender), tx.nonce()),
            transaction: tx,
            propagate: true,
            timestamp,
            origin: TransactionOrigin::Local,
            authority_ids: None,
        })
    }

    struct StubBestTxIter {
        inner: VecDeque<Arc<ValidPoolTransaction<EthPooledTransaction>>>,
    }

    impl StubBestTxIter {
        fn new(txs: Vec<Arc<ValidPoolTransaction<EthPooledTransaction>>>) -> Self {
            Self { inner: txs.into() }
        }
    }

    impl Iterator for StubBestTxIter {
        type Item = Arc<ValidPoolTransaction<EthPooledTransaction>>;

        fn next(&mut self) -> Option<Self::Item> {
            self.inner.pop_front()
        }
    }

    impl BestTransactions for StubBestTxIter {
        fn mark_invalid(&mut self, _transaction: &Self::Item, _kind: &InvalidPoolTransactionError) {
        }

        fn no_updates(&mut self) {}

        fn set_skip_blobs(&mut self, _skip_blobs: bool) {}
    }

    #[test]
    fn combined_iterator_prioritizes_shadow_and_respects_pd_budget() {
        let mut generator = TransactionGenerator::with_num_signers(StdRng::seed_from_u64(1), 1);
        generator.set_base_fee(1);
        generator.set_gas_limit(210_000);

        let timestamp = Instant::now();

        // Prepare transactions
        let shadow_tx = generator.gen_eip1559_pooled();
        let shadow_hash = *shadow_tx.hash();

        let pd_small = pd_pooled_transaction(&mut generator, 0, 2_000);
        let pd_small_hash = *pd_small.hash();

        let pd_large = pd_pooled_transaction(&mut generator, 1, 7_000);
        let pd_large_hash = *pd_large.hash();

        let normal_tx = normal_pooled_transaction(&mut generator, 2);
        let normal_hash = *normal_tx.hash();

        // `pd_small` consumes 2k chunks, `pd_large` would push the total above 7.5k, so it should
        // be deferred and not appear before the normal tx that follows.
        let pool_iter: BestTransactionsIter = Box::new(StubBestTxIter::new(vec![
            make_valid(pd_small, 11, timestamp),
            make_valid(pd_large, 12, timestamp),
            make_valid(normal_tx, 13, timestamp),
        ]));

        // is_sprite_active = true to enable PD chunk budgeting for this test
        let mut iterator =
            CombinedTransactionIterator::new(timestamp, vec![shadow_tx], pool_iter, 7_500, true);

        let collected: Vec<_> = (&mut iterator).take(3).collect();

        assert_eq!(collected.len(), 3);
        assert_eq!(collected[0].hash(), &shadow_hash);
        assert_eq!(collected[1].hash(), &pd_small_hash);
        assert_eq!(collected[2].hash(), &normal_hash);
        assert!(collected.iter().all(|tx| tx.hash() != &pd_large_hash));
        assert!(iterator.next().is_none());
    }
}
