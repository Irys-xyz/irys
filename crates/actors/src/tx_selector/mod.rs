pub(crate) mod helpers;

use crate::block_discovery::get_data_tx_in_parallel_inner;
use crate::block_validation::get_assigned_ingress_proofs;
use crate::chunk_ingress_service::{ChunkIngressServiceInner, ChunkIngressState};
use crate::mempool_service::{AtomicMempoolState, MempoolTxs, validate_commitment_transaction};
use crate::shadow_tx_generator::PublishLedgerWithTxs;
use eyre::{OptionExt as _, eyre};
use futures::FutureExt as _;
use irys_database::db::IrysDatabaseExt as _;
use irys_database::db_cache::CachedDataRoot;
use irys_database::tables::IngressProofs;
use irys_database::{
    cached_data_root_by_data_root, ingress_proofs_by_data_root, tx_header_by_txid,
};
use irys_domain::{
    BlockTreeEntry, BlockTreeReadGuard, CommitmentSnapshotStatus, get_optimistic_chain,
};
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_types::ingress::{CachedIngressProof, IngressProof};
use irys_types::transaction::fee_distribution::{PublishFeeCharges, TermFeeCharges};
use irys_types::{
    BlockHash, CommitmentTypeV2, DataLedger, DataTransactionHeader, IngressProofsList,
};
use irys_types::{Config, H256, IrysTransactionCommon as _, U256, app_state::DatabaseProvider};
use irys_types::{IrysAddress, SystemLedger, UnixTimestamp};
use reth::rpc::types::BlockId;
use reth_db::Database as _;
use reth_db::cursor::*;
use std::collections::{HashMap, HashSet};
use tracing::{debug, info, instrument, trace, warn};

/// Borrowed dependencies for transaction selection.
pub struct TxSelectionContext<'a> {
    pub block_tree: &'a BlockTreeReadGuard,
    pub db: &'a DatabaseProvider,
    pub reth_adapter: &'a IrysRethNodeAdapter,
    pub config: &'a Config,
    pub mempool_state: &'a AtomicMempoolState,
    pub chunk_ingress_state: &'a ChunkIngressState,
}

/// Main entry point: selects the best transactions from the mempool for block production.
#[instrument(skip(ctx), fields(block.parent_block_hash = ?parent_block_hash), err)]
pub async fn select_best_txs(
    parent_block_hash: BlockHash,
    ctx: &TxSelectionContext<'_>,
) -> eyre::Result<MempoolTxs> {
    let mempool_state = ctx.mempool_state;
    let mut fees_spent_per_address: HashMap<IrysAddress, U256> = HashMap::new();
    let mut confirmed_commitments = HashSet::new();
    let mut commitment_tx = Vec::new();
    let mut unfunded_address = HashSet::new();

    let max_commitments: usize = ctx
        .config
        .node_config
        .consensus_config()
        .mempool
        .max_commitment_txs_per_block
        .try_into()
        .expect("max_commitment_txs_per_block to fit into usize");

    let (
        canonical,
        parent_block_height,
        parent_evm_block_id,
        commitment_snapshot,
        epoch_snapshot,
        ema_snapshot,
        block_timestamp_secs,
    ) = {
        let tree = ctx.block_tree.read();

        // Get the canonical chain for use in get_publish_txs_and_proofs
        let (canonical, _) = tree.get_canonical_chain();

        eyre::ensure!(
            // todo if you change this to .last() instead of .any() then some poor fork tests start braeking
            canonical
                .iter()
                .any(|entry| entry.block_hash() == parent_block_hash),
            "Provided parent_block_hash {:?} is not on the canonical chain. Canonical tip: {:?}",
            parent_block_hash,
            canonical.last().map(BlockTreeEntry::block_hash)
        );

        let block = tree
            .get_block(&parent_block_hash)
            .ok_or_eyre(format!("Block not found: {:?}", parent_block_hash))?;

        // Extract only the data we need before the tree guard is dropped
        let block_height = block.height;
        let evm_block_id = Some(BlockId::Hash(block.evm_block_hash.into()));
        // Get the parent block's timestamp (millis) and convert to seconds for hardfork params
        let block_timestamp_secs = block.timestamp_secs();

        let ema_snapshot = tree
            .get_ema_snapshot(&parent_block_hash)
            .ok_or_else(|| eyre!("EMA snapshot not found for block {:?}", parent_block_hash))?;
        let epoch_snapshot = tree
            .get_epoch_snapshot(&parent_block_hash)
            .ok_or_else(|| eyre!("Epoch snapshot not found for block {:?}", parent_block_hash))?;
        let commitment_snapshot =
            tree.get_commitment_snapshot(&parent_block_hash)
                .map_err(|e| {
                    eyre!(
                        "Failed to get commitment snapshot for block {:?}: {}",
                        parent_block_hash,
                        e
                    )
                })?;

        (
            canonical,
            block_height,
            evm_block_id,
            commitment_snapshot,
            epoch_snapshot,
            ema_snapshot,
            block_timestamp_secs,
        )
    };

    let current_height = parent_block_height;
    let next_block_height = parent_block_height + 1;
    // Use parent block's timestamp for hardfork params (seconds since epoch)
    let current_timestamp = block_timestamp_secs;
    let min_anchor_height = current_height.saturating_sub(
        (ctx.config.consensus.mempool.tx_anchor_expiry_depth as u64)
            .saturating_sub(ctx.config.consensus.block_migration_depth as u64),
    );

    let max_anchor_height =
        current_height.saturating_sub(ctx.config.consensus.block_migration_depth as u64);

    let mut balances: HashMap<IrysAddress, U256> = HashMap::new();

    info!(
        block.height = parent_block_height,
        block.hash = ?parent_block_hash,
        "Starting mempool transaction selection"
    );

    // Collect confirmed commitment transactions from canonical chain to avoid duplicates
    for entry in canonical.iter() {
        let commitment_ledger = entry
            .header()
            .system_ledgers
            .iter()
            .find(|l| l.ledger_id == SystemLedger::Commitment as u32);
        if let Some(commitment_ledger) = commitment_ledger {
            for tx_id in &commitment_ledger.tx_ids.0 {
                confirmed_commitments.insert(*tx_id);
            }
        }
    }

    // Collect all stake and pledge commitments from mempool
    let mut sorted_commitments = mempool_state.sorted_commitments().await;

    // Sort all commitments according to our priority rules
    sorted_commitments.sort();

    // Filter out commitment transactions with versions below the hardfork minimum
    // Use current time (not parent block timestamp) because the new block will have
    // a timestamp of approximately now(), and validators check versions against block timestamp
    ctx.config
        .consensus
        .hardforks
        .retain_valid_commitment_versions(&mut sorted_commitments, UnixTimestamp::now()?);

    balances.extend(
        helpers::fetch_balances_for_transactions(
            ctx.reth_adapter,
            parent_evm_block_id,
            &sorted_commitments,
        )
        .await,
    );

    // Process sorted commitments
    // create a throw away commitment snapshot so we can simulate behaviour before including a commitment tx in returned txs
    let mut simulation_commitment_snapshot = commitment_snapshot.as_ref().clone();
    for tx in &sorted_commitments {
        if confirmed_commitments.contains(&tx.id()) {
            debug!(
                tx.id = ?tx.id(),
                tx.commitment_type = ?tx.commitment_type(),
                tx.signer = ?tx.signer(),
                "Skipping already confirmed commitment transaction"
            );
            continue;
        }

        // Full validation (fee, funding at parent block, value) before simulation
        if let Err(error) = validate_commitment_transaction(
            ctx.reth_adapter,
            &ctx.config.consensus,
            tx,
            parent_evm_block_id,
        )
        .await
        {
            tracing::warn!(tx.error = ?error, "rejecting commitment tx");
            continue;
        }

        if !crate::anchor_validation::validate_anchor_for_inclusion(
            ctx.block_tree,
            ctx.db,
            min_anchor_height,
            max_anchor_height,
            tx,
        )? {
            debug!(
                tx.id = ?tx.id(),
                tx.signer = ?tx.signer(),
                tx.commitment_type = ?tx.commitment_type(),
                tx.anchor = ?tx.anchor(),
                min_anchor_height = min_anchor_height,
                max_anchor_height = max_anchor_height,
                "Not promoting commitment tx - anchor validation failed"
            );
            continue;
        }

        // signer stake status check
        if matches!(tx.commitment_type(), CommitmentTypeV2::Stake) {
            let is_staked = epoch_snapshot.is_staked(tx.signer());
            debug!(
                tx.id = ?tx.id(),
                tx.signer = ?tx.signer(),
                tx.is_staked = is_staked,
                "Checking stake status for commitment tx"
            );
            if is_staked {
                // if a signer has stake commitments in the mempool, but is already staked, we should ignore them
                debug!(
                    tx.id = ?tx.id(),
                    tx.signer = ?tx.signer(),
                    tx.commitment_type = ?tx.commitment_type(),
                    "Not promoting commitment tx - signer already staked"
                );
                continue;
            }
        }
        // simulation check
        {
            let simulation = simulation_commitment_snapshot.add_commitment(tx, &epoch_snapshot);

            // skip commitments that would not be accepted
            if simulation != CommitmentSnapshotStatus::Accepted {
                warn!(
                    tx.commitment_type = ?tx.commitment_type(),
                    tx.id = ?tx.id(),
                    tx.simulation_status = ?simulation,
                    "Commitment tx rejected by simulation"
                );
                continue;
            }
        }

        trace!(
            tx.id = ?tx.id(),
            tx.signer = ?tx.signer(),
            tx.fee = ?tx.total_cost(),
            "Checking funding for commitment transaction"
        );
        if helpers::check_funding(
            tx,
            &balances,
            &mut unfunded_address,
            &mut fees_spent_per_address,
        ) {
            trace!(
                tx.id = ?tx.id(),
                tx.signer = ?tx.signer(),
                tx.fee = ?tx.total_cost(),
                tx.selected_count = commitment_tx.len() + 1,
                tx.max_commitments = max_commitments,
                "Commitment transaction passed funding check"
            );
        } else {
            trace!(
                tx.id = ?tx.id(),
                tx.signer = ?tx.signer(),
                tx.fee = ?tx.total_cost(),
                tx.validation_failed_reason = "insufficient_funds",
                "Data transaction failed funding check"
            );
            continue;
        }

        debug!(
            tx.id = ?tx.id(),
            tx.commitment_type = ?tx.commitment_type(),
            tx.signer = ?tx.signer(),
            tx.fee = ?tx.total_cost(),
            tx.selected_count = commitment_tx.len() + 1,
            tx.max_commitments = max_commitments,
            "Adding commitment transaction to block"
        );

        commitment_tx.push(tx.clone());

        // if we have reached the maximum allowed number of commitment txs per block
        // do not push anymore
        if commitment_tx.len() >= max_commitments {
            break;
        }
    }

    // Log commitment selection summary
    if !commitment_tx.is_empty() {
        let (stakes, pledges, unpledges, unstakes, update_reward_addresses) =
            commitment_tx.iter().fold(
                (0_usize, 0_usize, 0_usize, 0_usize, 0_usize),
                |(stakes, pledges, unpledges, unstakes, update_reward_addresses), tx| match tx
                    .commitment_type()
                {
                    CommitmentTypeV2::Stake => (
                        stakes + 1,
                        pledges,
                        unpledges,
                        unstakes,
                        update_reward_addresses,
                    ),
                    CommitmentTypeV2::Pledge { .. } => (
                        stakes,
                        pledges + 1,
                        unpledges,
                        unstakes,
                        update_reward_addresses,
                    ),
                    CommitmentTypeV2::Unpledge { .. } => (
                        stakes,
                        pledges,
                        unpledges + 1,
                        unstakes,
                        update_reward_addresses,
                    ),
                    CommitmentTypeV2::Unstake => (
                        stakes,
                        pledges,
                        unpledges,
                        unstakes + 1,
                        update_reward_addresses,
                    ),
                    CommitmentTypeV2::UpdateRewardAddress { .. } => (
                        stakes,
                        pledges,
                        unpledges,
                        unstakes,
                        update_reward_addresses + 1,
                    ),
                },
            );
        info!(
            commitment_selection.selected_commitments = commitment_tx.len(),
            commitment_selection.stake_txs = stakes,
            commitment_selection.pledge_txs = pledges,
            commitment_selection.unpledge_txs = unpledges,
            commitment_selection.unstake_txs = unstakes,
            commitment_selection.update_reward_address_txs = update_reward_addresses,
            commitment_selection.max_allowed = max_commitments,
            "Completed commitment transaction selection"
        );
    }

    // Prepare data transactions for inclusion after commitments
    let mut submit_ledger_txs = get_pending_submit_ledger_txs(ctx).await;
    let total_data_available = submit_ledger_txs.len();

    // Sort data transactions by fee (highest first) to maximize revenue
    // The miner will get proportionally higher rewards for higher term fee values
    submit_ledger_txs.sort_by(|a, b| match b.user_fee().cmp(&a.user_fee()) {
        std::cmp::Ordering::Equal => a.id.cmp(&b.id),
        fee_ordering => fee_ordering,
    });

    // Apply block size constraint and funding checks to data transactions
    let mut submit_tx = Vec::new();
    let max_data_txs: usize = ctx
        .config
        .node_config
        .consensus_config()
        .mempool
        .max_data_txs_per_block
        .try_into()
        .expect("max_data_txs_per_block to fit into usize");

    balances.extend(
        helpers::fetch_balances_for_transactions(
            ctx.reth_adapter,
            parent_evm_block_id,
            &submit_ledger_txs,
        )
        .await,
    );

    // Select data transactions in fee-priority order, respecting funding limits
    // and maximum transaction count per block
    for tx in submit_ledger_txs {
        // Validate fees based on ledger type
        let Ok(ledger) = irys_types::DataLedger::try_from(tx.ledger_id) else {
            debug!(
                tx.id = ?tx.id,
                tx.ledger_id = tx.ledger_id,
                "Skipping tx: invalid ledger ID"
            );
            continue;
        };
        match ledger {
            irys_types::DataLedger::Publish => {
                // For Publish ledger, validate both term and perm fees
                // Calculate expected fees based on current EMA for the next block height
                let Ok(expected_term_fee) = helpers::calculate_term_storage_fee(
                    ctx.config,
                    tx.data_size,
                    &ema_snapshot,
                    next_block_height,
                    current_timestamp,
                ) else {
                    debug!(
                        tx.id = ?tx.id,
                        tx.data_size = tx.data_size,
                        "Failed to calculate term fee"
                    );
                    continue;
                };

                let Ok(expected_perm_fee) = helpers::calculate_perm_storage_fee(
                    ctx.config,
                    tx.data_size,
                    expected_term_fee,
                    &ema_snapshot,
                    current_timestamp,
                ) else {
                    debug!(
                        tx.id = ?tx.id,
                        tx.data_size = tx.data_size,
                        "Failed to calculate perm fee"
                    );
                    continue;
                };

                // Validate term fee
                if tx.term_fee < expected_term_fee {
                    debug!(
                        tx.id = ?tx.id,
                        tx.actual_term_fee = ?tx.term_fee,
                        tx.expected_term_fee = ?expected_term_fee,
                        "Skipping Publish tx: insufficient term_fee"
                    );
                    continue;
                }

                // Validate perm fee must be present for Publish ledger
                let Some(perm_fee) = tx.perm_fee else {
                    // Missing perm_fee for Publish ledger transaction is invalid
                    warn!(
                        tx.id = ?tx.id,
                        tx.signer = ?tx.signer,
                        "Invalid Publish tx: missing perm_fee"
                    );
                    // todo: add to list of invalid txs because all publish txs must have perm fee present
                    continue;
                };
                if perm_fee < expected_perm_fee.amount {
                    debug!(
                        tx.id = ?tx.id,
                        tx.actual_perm_fee = ?perm_fee,
                        tx.expected_perm_fee = ?expected_perm_fee.amount,
                        "Skipping Publish tx: insufficient perm_fee"
                    );
                    continue;
                }

                // Mirror API-only structural checks to filter gossip txs that would fail API validation
                if TermFeeCharges::new(tx.term_fee, &ctx.config.node_config.consensus_config())
                    .is_err()
                {
                    debug!(
                        tx.id = ?tx.id,
                        tx.term_fee = ?tx.term_fee,
                        "Skipping Publish tx: invalid term fee structure"
                    );
                    continue;
                }

                let number_of_ingress_proofs_total = ctx
                    .config
                    .number_of_ingress_proofs_total_at(current_timestamp);
                if PublishFeeCharges::new(
                    perm_fee,
                    tx.term_fee,
                    &ctx.config.node_config.consensus_config(),
                    number_of_ingress_proofs_total,
                )
                .is_err()
                {
                    debug!(
                        tx.id = ?tx.id,
                        tx.perm_fee = ?perm_fee,
                        tx.term_fee = ?tx.term_fee,
                        "Skipping Publish tx: invalid perm fee structure"
                    );
                    continue;
                }
            }
            irys_types::DataLedger::Submit => {
                // todo: add to list of invalid txs because we don't support Submit txs
                debug!(
                    tx.id = ?tx.id,
                    tx.signer = ?tx.signer(),
                    "Not promoting data tx - Submit ledger not eligible for promotion"
                );
                continue;
            }
            DataLedger::OneYear | DataLedger::ThirtyDay => {
                warn!(
                    tx.id = ?tx.id,
                    tx.ledger = ?ledger,
                    "Skipping unsupported term ledger"
                );
                continue;
            }
        }

        if !crate::anchor_validation::validate_anchor_for_inclusion(
            ctx.block_tree,
            ctx.db,
            min_anchor_height,
            max_anchor_height,
            &tx,
        )? {
            debug!(
                tx.id = ?tx.id,
                tx.signer = ?tx.signer(),
                tx.anchor = ?tx.anchor,
                min_anchor_height = min_anchor_height,
                max_anchor_height = max_anchor_height,
                "Not promoting data tx - anchor validation failed"
            );
            continue;
        }

        trace!(
            tx.id = ?tx.id,
            tx.signer = ?tx.signer(),
            tx.fee = ?tx.total_cost(),
            "Checking funding for data transaction"
        );
        if helpers::check_funding(
            &tx,
            &balances,
            &mut unfunded_address,
            &mut fees_spent_per_address,
        ) {
            trace!(
                tx.id = ?tx.id,
                tx.signer = ?tx.signer(),
                tx.fee = ?tx.total_cost(),
                tx.selected_count = submit_tx.len() + 1,
                tx.max_data_txs = max_data_txs,
                "Data transaction passed funding check"
            );
            submit_tx.push(tx);
            if submit_tx.len() >= max_data_txs {
                break;
            }
        } else {
            trace!(
                tx.id = ?tx.id,
                tx.signer = ?tx.signer(),
                tx.fee = ?tx.total_cost(),
                tx.validation_failed_reason = "insufficient_funds",
                "Data transaction failed funding check"
            );
        }
    }

    // note: publish txs are sorted internally by the get_publish_txs_and_proofs fn
    let publish_txs_and_proofs = get_publish_txs_and_proofs(
        ctx,
        &canonical,
        &submit_tx,
        current_height,
        current_timestamp,
    )
    .await?;

    // Calculate total fees and log final summary
    let total_fee_collected: U256 = submit_tx
        .iter()
        .map(irys_types::IrysTransactionCommon::user_fee)
        .fold(U256::zero(), irys_types::U256::saturating_add)
        .saturating_add(
            commitment_tx
                .iter()
                .map(irys_types::IrysTransactionCommon::total_cost)
                .fold(U256::zero(), irys_types::U256::saturating_add),
        );

    info!(
        mempool_selected.commitment_txs = commitment_tx.len(),
        mempool_selected.data_txs = submit_tx.len(),
        mempool_selected.publish_txs = publish_txs_and_proofs.txs.len(),
        mempool_selected.total_fee_collected = ?total_fee_collected,
        mempool_selected.unfunded_addresses = unfunded_address.len(),
        "Mempool transaction selection completed"
    );

    // Check for high rejection rate
    let total_commitments_available = sorted_commitments.len();
    let total_available = total_commitments_available + total_data_available;
    let total_selected = commitment_tx.len() + submit_tx.len();

    if total_available > 0 {
        const REJECTION_RATE_THRESHOLD: usize = 70;
        let rejection_rate = ((total_available - total_selected) * 100) / total_available;
        if rejection_rate > REJECTION_RATE_THRESHOLD {
            warn!(
                mempool_selected.rejection_rate = rejection_rate,
                mempool_selected.total_available = total_available,
                mempool_selected.total_selected = total_selected,
                mempool_selected.commitments_available = total_commitments_available,
                mempool_selected.commitments_selected = commitment_tx.len(),
                mempool_selected.data_available = total_data_available,
                mempool_selected.data_selected = submit_tx.len(),
                mempool_selected.unfunded_addresses = unfunded_address.len(),
                "High transaction rejection rate detected"
            );
        }
    }

    // Return selected transactions grouped by type
    Ok(MempoolTxs {
        commitment_tx,
        submit_tx,
        publish_tx: publish_txs_and_proofs,
    })
}

#[tracing::instrument(level = "trace", skip_all, fields(canonical.len = canonical.len(), submit_tx.count = submit_tx.len()))]
async fn get_publish_txs_and_proofs(
    ctx: &TxSelectionContext<'_>,
    canonical: &[BlockTreeEntry],
    submit_tx: &[DataTransactionHeader],
    current_height: u64,
    current_timestamp: UnixTimestamp,
) -> Result<PublishLedgerWithTxs, eyre::Error> {
    let mut publish_txs: Vec<DataTransactionHeader> = Vec::new();
    let mut publish_proofs: Vec<IngressProof> = Vec::new();
    // IMPORTANT: must be valid for THE HEIGHT WE ARE ABOUT TO PRODUCE
    let next_block_height = current_height + 1;

    // only max anchor age is constrained for ingress proofs
    let min_ingress_proof_anchor_height = next_block_height.saturating_sub(
        ctx.config
            .consensus
            .mempool
            .ingress_proof_anchor_expiry_depth as u64,
    );

    {
        let (publish_txids, cached_data_roots) = ctx
            .db
            .view_eyre(|tx| {
                let mut read_cursor = tx
                    .new_cursor::<IngressProofs>()
                    .map_err(|e| eyre!("Failed to create DB read cursor: {}", e))?;

                let walker = read_cursor
                    .walk(None)
                    .map_err(|e| eyre!("Failed to create DB read cursor walker: {}", e))?;

                let ingress_proofs = walker
                    .collect::<Result<HashMap<_, _>, _>>()
                    .map_err(|e| eyre!("Failed to collect ingress proofs from database: {}", e))?;

                let mut publish_txids: Vec<H256> = Vec::new();
                let mut cached_data_roots: HashMap<H256, CachedDataRoot> = HashMap::new();

                // Loop through all the data_roots with ingress proofs and find corresponding transaction ids
                for data_root in ingress_proofs.keys() {
                    let cached_data_root = cached_data_root_by_data_root(tx, *data_root).unwrap();
                    if let Some(cached_data_root) = cached_data_root {
                        let txids = cached_data_root.txid_set.clone();
                        trace!(tx.ids = ?txids, "Publish candidates");
                        publish_txids.extend(txids);
                        cached_data_roots.insert(*data_root, cached_data_root);
                    }
                }
                Ok((publish_txids, cached_data_roots))
            })
            .map_err(|e| eyre!("Failed to create DB transaction: {}", e))?;

        // Loop through all the pending tx to see which haven't been promoted
        let txs = get_data_txs(ctx, publish_txids.clone()).await;

        // Note: get_data_tx_in_parallel_inner() read from both the mempool and
        //       db as publishing can happen to a tx that is no longer in the mempool
        // TODO: improve this
        let mut tx_headers = get_data_tx_in_parallel_inner(
            publish_txids,
            |_tx_ids| {
                {
                    let txs = txs.clone(); // whyyyy
                    async move { Ok(txs) }
                }
                .boxed()
            },
            ctx.db,
        )
        .await
        .unwrap_or(vec![]);

        // Sort the resulting publish_txs & proofs
        tx_headers.sort_by(|a, b| a.id.cmp(&b.id));

        // Filter out any tx headers with the wrong data_size for this data_root
        tx_headers.retain(|tx| {
            cached_data_roots
                .get(&tx.data_root)
                .is_none_or(|cdr| !cdr.data_size_confirmed || tx.data_size == cdr.data_size)
        });

        // reduce down the canonical chain to the txs in the submit ledger
        let submit_txs_from_canonical = canonical.iter().fold(HashSet::new(), |mut acc, v| {
            acc.extend(v.header().data_ledgers[DataLedger::Submit].tx_ids.0.clone());
            acc
        });

        let epoch_snapshot = ctx.block_tree.read().canonical_epoch_snapshot();

        for tx_header in &tx_headers {
            debug!(
                "Processing publish candidate tx {} {:#?}",
                &tx_header.id, &tx_header
            );
            let is_promoted = tx_header.promoted_height().is_some();

            if is_promoted {
                // If it's promoted skip it
                warn!(
                    tx.id = ?tx_header.id,
                    tx.promoted_height = ?tx_header.promoted_height(),
                    "Publish candidate is already promoted"
                );
                continue;
            }
            // check for previous submit inclusion
            // we do this by checking if the tx is in the block tree or database.
            // if it is, we know it could've only gotten there by being included in the submit ledger.
            // if it's not, we also check if the submit ledger for this block contains the tx (single-block promotion). if it does, we also promote it.
            if !submit_txs_from_canonical.contains(&tx_header.id) {
                // check for single-block promotion
                if !submit_tx.iter().any(|tx| tx.id == tx_header.id) {
                    // check database
                    if ctx
                        .db
                        .view_eyre(|tx| tx_header_by_txid(tx, &tx_header.id))?
                        .is_none()
                    {
                        // no previous inclusion
                        warn!(
                            tx.id = ?tx_header.id,
                            tx.data_root = ?tx_header.data_root,
                            "Unable to find previous submit inclusion for publish candidate"
                        );
                        continue;
                    }
                }
            }

            // If it's not promoted, validate the proofs

            // Get all the proofs for this tx
            let mut all_proofs = ctx
                .db
                .view_eyre(|read_tx| ingress_proofs_by_data_root(read_tx, tx_header.data_root))?
                .into_iter()
                .filter(|(_root, cached_proof)| {
                    let expired = ChunkIngressServiceInner::is_ingress_proof_expired_static(
                        ctx.block_tree,
                        ctx.db,
                        ctx.config,
                        &cached_proof.proof,
                    )
                    .expired_or_invalid;
                    !expired
                })
                .collect::<Vec<_>>();

            // Dedup by signer address in-place, keeping first proof per address
            let pre_dedup_len = all_proofs.len();
            let mut seen_addresses = HashSet::new();
            all_proofs.retain(|(_, cached_proof)| seen_addresses.insert(cached_proof.0.address));
            if all_proofs.len() < pre_dedup_len {
                warn!(
                    tx.id = ?tx_header.id,
                    tx.data_root = ?tx_header.data_root,
                    before = pre_dedup_len,
                    after = all_proofs.len(),
                    "Duplicate ingress proof signers detected for data root, deduplicating"
                );
            }

            // Check for minimum number of ingress proofs
            let total_miners = epoch_snapshot.commitment_state.stake_commitments.len();

            // Take the smallest value, the configured total proofs count or the number
            // of staked miners that can produce a valid proof.
            let number_of_ingress_proofs_total = ctx
                .config
                .number_of_ingress_proofs_total_at(current_timestamp);
            let proofs_per_tx =
                std::cmp::min(number_of_ingress_proofs_total as usize, total_miners);

            if all_proofs.len() < proofs_per_tx {
                info!(
                    "Not promoting tx {} - insufficient proofs (got {} wanted {})",
                    &tx_header.id,
                    &all_proofs.len(),
                    proofs_per_tx
                );
                continue;
            }

            let mut all_tx_proofs: Vec<CachedIngressProof> = Vec::with_capacity(all_proofs.len());

            //filter all these ingress proofs by their anchor validity
            for (_hash, cached) in all_proofs {
                let cached_proof = cached.0;
                // validate the anchor is still valid
                let anchor_is_valid =
                    crate::anchor_validation::validate_ingress_proof_anchor_for_inclusion(
                        ctx.block_tree,
                        ctx.db,
                        min_ingress_proof_anchor_height,
                        &cached_proof.proof,
                    )?;
                if anchor_is_valid {
                    all_tx_proofs.push(cached_proof)
                }
                // note: data root lifecycle work includes code to handle ingress proofs we find as invalid
            }

            // Extract IngressProofs for get_assigned_ingress_proofs API
            let proofs_only: Vec<IngressProof> =
                all_tx_proofs.iter().map(|c| c.proof.clone()).collect();

            // Get assigned and unassigned proofs using the existing utility function
            let (assigned_proofs, assigned_miners) = match get_assigned_ingress_proofs(
                &proofs_only,
                tx_header,
                ctx.block_tree,
                ctx.db,
                ctx.config,
            )
            .await
            {
                Ok(result) => result,
                Err(e) => {
                    warn!(
                        "Failed to get assigned proofs for tx {}: {}",
                        &tx_header.id, e
                    );
                    continue;
                }
            };

            // Calculate expected assigned proofs, clamping to available miners
            let number_of_ingress_proofs_from_assignees = ctx
                .config
                .number_of_ingress_proofs_from_assignees_at(current_timestamp);
            let mut expected_assigned_proofs = number_of_ingress_proofs_from_assignees as usize;

            if assigned_miners < expected_assigned_proofs {
                warn!(
                    "Clamping expected_assigned_proofs from {} to {} for tx {}",
                    expected_assigned_proofs, assigned_miners, &tx_header.id
                );
                expected_assigned_proofs = assigned_miners;
            }

            // Check if we have enough assigned proofs
            if assigned_proofs.len() < expected_assigned_proofs {
                info!(
                    "Not promoting tx {} - insufficient assigned proofs (got {} wanted {})",
                    &tx_header.id,
                    assigned_proofs.len(),
                    expected_assigned_proofs
                );
                continue;
            }

            // Separate assigned and unassigned proofs
            let assigned_proof_set: HashSet<_> = assigned_proofs
                .iter()
                .map(|p| &p.proof.0) // Use signature as unique identifier
                .collect();

            let unassigned_proofs: Vec<IngressProof> = all_tx_proofs
                .iter()
                .filter(|c| !assigned_proof_set.contains(&c.proof.proof.0))
                .filter(|c| {
                    // Filter out proofs from unstaked signers
                    epoch_snapshot.is_staked(c.address)
                })
                .map(|c| c.proof.clone())
                .collect();

            // Build the final proof list
            let mut final_proofs = Vec::new();

            // First, add assigned proofs up to the total network limit
            // Use all available assigned proofs, but don't exceed the network total
            let total_network_limit = number_of_ingress_proofs_total as usize;
            let assigned_to_use = std::cmp::min(assigned_proofs.len(), total_network_limit);
            final_proofs.extend_from_slice(&assigned_proofs[..assigned_to_use]);

            // Then fill remaining slots with unassigned proofs if needed
            let remaining_slots = total_network_limit - final_proofs.len();
            if remaining_slots > 0 {
                let unassigned_to_use = std::cmp::min(unassigned_proofs.len(), remaining_slots);
                final_proofs.extend_from_slice(&unassigned_proofs[..unassigned_to_use]);
            }

            // Final check - do we have enough total proofs?
            if final_proofs.len() < number_of_ingress_proofs_total as usize {
                info!(
                    "Not promoting tx {} - insufficient total proofs after assignment filtering (got {} wanted {})",
                    &tx_header.id,
                    final_proofs.len(),
                    number_of_ingress_proofs_total
                );
                continue;
            }

            // Success - add this transaction and its proofs
            publish_txs.push(tx_header.clone());
            publish_proofs.extend(final_proofs.clone()); // Clone to avoid moving final_proofs

            info!(
                "Promoting tx {} with {} assigned proofs and {} total proofs",
                &tx_header.id,
                assigned_to_use, // Show actual assigned proofs used (capped by network limit)
                final_proofs.len()
            );
        }
    }

    let txs = &publish_txs.iter().map(|h| h.id).collect::<Vec<_>>();
    debug!(tx.ids = ?txs, "Publish transactions");

    debug!("Processing Publish transactions {:#?}", &publish_txs);

    Ok(PublishLedgerWithTxs {
        txs: publish_txs,
        proofs: if publish_proofs.is_empty() {
            None
        } else {
            Some(IngressProofsList::from(publish_proofs))
        },
    })
}

/// Returns all Submit ledger transactions that are pending inclusion in future blocks.
///
/// This function specifically filters the Submit ledger mempool to exclude transactions
/// that have already been included in recent canonical blocks within the anchor expiry
/// window. Unlike the general mempool filter, this focuses solely on Submit transactions.
///
/// # Algorithm
/// 1. Starts with all valid Submit ledger transactions from mempool
/// 2. Walks backwards through canonical chain within anchor expiry depth
/// 3. Removes Submit transactions that already exist in historical blocks
/// 4. Returns remaining pending Submit transactions
///
/// # Returns
/// A vector of `DataTransactionHeader` representing Submit ledger transactions
/// that are pending inclusion and have not been processed in recent blocks.
///
/// # Notes
/// - Only considers Submit ledger transactions (filters out Publish, etc.)
/// - Only examines blocks within the configured `anchor_expiry_depth`
async fn get_pending_submit_ledger_txs(ctx: &TxSelectionContext<'_>) -> Vec<DataTransactionHeader> {
    // Get the current canonical chain head to establish our starting point for block traversal
    // TODO: `get_optimistic_chain` and `get_canonical_chain` can be 2 different entries!
    let optimistic = get_optimistic_chain(ctx.block_tree.clone()).await.unwrap();
    let (canonical, _) = ctx.block_tree.read().get_canonical_chain();
    let canonical_head_entry = canonical.last().unwrap();

    // This is just here to catch any oddities in the debug log. The optimistic
    // and canonical should always have the same results from my reading of the code.
    // if the tests are stable and this hasn't come up it can be removed.
    if optimistic.last().unwrap().0 != canonical_head_entry.block_hash() {
        debug!("Optimistic and Canonical have different heads");
    }

    let block_hash = canonical_head_entry.block_hash();
    let block_height = canonical_head_entry.height();

    // retrieve block from block tree or database
    // be aware that genesis starts its life immediately in the database
    let mut block =
        crate::block_header_lookup::get_block_header(ctx.block_tree, ctx.db, block_hash, false)
            .expect("block header lookup should succeed")
            .unwrap_or_else(|| {
                panic!(
                    "No block header found for hash {} ({})",
                    block_hash, block_height
                )
            });

    // Calculate the minimum block height we need to check for transaction conflicts
    // Only transactions anchored within this depth window are considered valid
    let anchor_expiry_depth = ctx.config.consensus.mempool.tx_anchor_expiry_depth as u64;
    let min_anchor_height = block_height.saturating_sub(anchor_expiry_depth);

    // Start with all valid Submit ledger transactions - we'll filter out already-included ones
    let mut pending_valid_submit_ledger_tx =
        ctx.mempool_state.all_valid_submit_ledgers_cloned().await;

    // Walk backwards through the canonical chain, removing Submit transactions
    // that have already been included in recent blocks within the anchor expiry window
    while block.height >= min_anchor_height {
        let block_data_tx_ids = block.get_data_ledger_tx_ids();

        // Check if this block contains any Submit ledger transactions
        if let Some(submit_txids) = block_data_tx_ids.get(&DataLedger::Submit) {
            // Remove Submit transactions that already exist in this historical block
            // This prevents double-inclusion and ensures we only return truly pending transactions
            for txid in submit_txids.iter() {
                pending_valid_submit_ledger_tx.remove(txid);
            }
        }

        // Stop if we've reached the genesis block
        if block.height == 0 {
            break;
        }

        // Move to the parent block and continue the traversal backwards
        let parent_block = crate::block_header_lookup::get_block_header(
            ctx.block_tree,
            ctx.db,
            block.previous_block_hash,
            false,
        )
        .expect("block header lookup should succeed")
        .expect("to find the parent block header");

        block = parent_block;
    }

    // Return all remaining Submit transactions by consuming the map
    // These represent Submit transactions that are pending and haven't been included in any recent block
    pending_valid_submit_ledger_tx.into_values().collect()
}

/// Helper to look up data transactions from mempool and DB, mirroring
/// `Inner::handle_get_data_tx_message` but using the selection context.
async fn get_data_txs(
    ctx: &TxSelectionContext<'_>,
    txids: Vec<H256>,
) -> Vec<Option<DataTransactionHeader>> {
    // Batch mempool lookup: single READ lock for all txids
    let mempool_results = ctx
        .mempool_state
        .batch_valid_submit_ledger_tx_cloned(&txids)
        .await;

    let mut found_txs = Vec::with_capacity(txids.len());

    for (tx_id, mempool_result) in txids.iter().zip(mempool_results) {
        if let Some(tx_header) = mempool_result {
            trace!("Got tx {} from mempool", tx_id);
            found_txs.push(Some(tx_header));
            continue;
        }

        // Fall back to DB for txs not in mempool
        let db_result = ctx
            .db
            .view(|read_tx| tx_header_by_txid(read_tx, tx_id))
            .map_err(|e| {
                warn!("Failed to open DB read transaction: {}", e);
                e
            })
            .ok()
            .and_then(|result| match result {
                Ok(Some(tx_header)) => {
                    trace!("Got tx {} from DB", tx_id);
                    Some(tx_header)
                }
                Ok(None) => {
                    debug!("Tx {} not found in DB", tx_id);
                    None
                }
                Err(e) => {
                    warn!("DB error reading tx {}: {}", tx_id, e);
                    None
                }
            });

        found_txs.push(db_result);
    }

    found_txs
}
