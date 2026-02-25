use crate::block_tree_service::ReorgEvent;
use crate::mempool_service::Inner;
use crate::mempool_service::TxIngressError;
use eyre::OptionExt as _;
use irys_database::db::IrysDatabaseExt as _;
use irys_types::{
    CommitmentTransaction, DataLedger, H256, IrysTransactionCommon, IrysTransactionId, SealedBlock,
    SystemLedger, get_ingress_proofs,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{debug, error, info, instrument, warn};

impl Inner {
    /// Updates in-memory mempool state for a confirmed block.
    ///
    /// DB persistence of included_height and promoted_height happens at confirmation
    /// time (via `BlockMigrationService::persist_metadata`). Full tx header
    /// persistence is deferred to migration time. This handler only updates:
    /// - In-memory metadata (included_height, promoted_height)
    /// - `CachedDataRoots` DB cache (needed for chunk ingress validation)
    /// - Pending tx pruning
    #[instrument(skip_all, fields(block.hash = %sealed_block.header().block_hash, block.height = sealed_block.header().height), err)]
    pub async fn handle_block_confirmed_message(
        &self,
        sealed_block: Arc<SealedBlock>,
    ) -> Result<(), TxIngressError> {
        let block = sealed_block.header();
        let submit_txids = &block.data_ledgers[DataLedger::Submit].tx_ids.0;
        let publish_txids = &block.data_ledgers[DataLedger::Publish].tx_ids.0;
        let commitment_txids = block.commitment_tx_ids();

        for txid in submit_txids.iter().chain(publish_txids.iter()) {
            self.mempool_state
                .set_data_tx_included_height_overwrite(*txid, block.height)
                .await;
        }

        for txid in commitment_txids.iter() {
            self.mempool_state
                .set_commitment_tx_included_height(*txid, block.height)
                .await;
        }

        for txid in publish_txids.iter() {
            if self
                .mempool_state
                .set_promoted_height(*txid, block.height)
                .await
                .is_some()
            {
                debug!(tx.id = %txid, promoted_height = block.height, "Promoted tx in mempool");
            }
        }

        // Update `CachedDataRoots` so that this block_hash is cached for each data_root
        for submit_tx in sealed_block
            .transactions()
            .get_ledger_txs(DataLedger::Submit)
        {
            let data_root = submit_tx.data_root;
            match self.irys_db.update_eyre(|db_tx| {
                irys_database::cache_data_root(db_tx, submit_tx, Some(block.as_ref()))?;
                Ok(())
            }) {
                Ok(()) => {
                    info!(
                        "Successfully cached data_root {:?} for tx {:?}",
                        data_root, submit_tx.id
                    );
                }
                Err(db_error) => {
                    error!(
                        "Failed to cache data_root {:?} for tx {:?}: {:?}",
                        data_root, submit_tx.id, db_error
                    );
                }
            };
        }

        self.prune_pending_txs().await;

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, fields(fork_parent.height = event.fork_parent.height))]
    pub async fn handle_reorg(&self, event: ReorgEvent) -> eyre::Result<()> {
        tracing::debug!(
            "Processing reorg: {} orphaned blocks from height {}",
            &event.old_fork.len(),
            &event.fork_parent.height
        );
        let new_tip = event.new_tip;

        self.handle_confirmed_data_tx_reorg(&event).await?;

        self.handle_confirmed_commitment_tx_reorg(&event).await?;

        self.reprocess_all_txs().await?;

        tracing::info!("Reorg handled, new tip: {:?}", &new_tip);
        Ok(())
    }

    /// Re-process all currently valid mempool txs
    /// all this does is take all valid submit & commitment txs, and passes them back through ingress
    #[instrument(skip_all)]
    pub async fn reprocess_all_txs(&self) -> eyre::Result<()> {
        // re-process all valid txs
        let (valid_submit_ledger_tx, valid_commitment_tx) =
            self.mempool_state.take_all_valid_txs().await;
        for (id, tx) in valid_submit_ledger_tx {
            match self.handle_data_tx_ingress_message_gossip(tx).await {
                Ok(_) => debug!("resubmitted data tx {} to mempool", &id),
                Err(err) => debug!("failed to resubmit data tx {} to mempool: {:?}", &id, &err),
            }
        }
        for (_address, txs) in valid_commitment_tx {
            for tx in txs {
                let id = tx.id();
                match self.handle_ingress_commitment_tx_message_gossip(tx).await {
                    Ok(_) => debug!("resubmitted commitment tx {} to mempool", &id),
                    Err(err) => debug!(
                        "failed to resubmit commitment tx {} to mempool: {:?}",
                        &id, &err
                    ),
                }
            }
        }

        Ok(())
    }

    /// Clears the promotion state (promoted_height) for a transaction in the mempool
    /// during reorg handling. Only affects in-memory state; does not persist to DB.
    #[tracing::instrument(level = "trace", skip_all, fields(tx.id = ?txid))]
    async fn mark_unpromoted_in_mempool(&self, txid: H256) -> eyre::Result<()> {
        // Try fast-path: clear in-place if present in the mempool
        if self.mempool_state.clear_promoted_height(txid).await {
            return Ok(());
        }

        tracing::debug!(tx.id = %txid, "Tx not in mempool; leaving unchanged");
        Ok(())
    }

    /// Validates a given anchor for *EXPIRY* DO NOT USE FOR REGULAR ANCHOR VALIDATION
    /// this uses modified rules compared to regular anchor validation - it doesn't care about maturity, and adds an extra grace window so that txs are only expired after anchor_expiry_depth + block_migration_depth
    /// this is to ensure txs stay in the mempool long enough for their parent block to confirm
    /// swallows errors from get_anchor_height (but does log them)
    #[tracing::instrument(level = "trace", skip_all, fields(tx.id = ?tx.id(), current_height = current_height
    ))]
    pub fn should_prune_tx(&self, current_height: u64, tx: &impl IrysTransactionCommon) -> bool {
        let anchor_height = match self
            .get_anchor_height(tx.anchor(), false /* does not need to be canonical */)
        {
            Ok(Some(h)) => h,
            // if we don't know about the anchor, we should prune
            // note: this can happen, i.e if we did a block rollback.
            Ok(None) => return true,
            Err(e) => {
                // we can't tell due to an error
                error!("Error checking if we should prune tx {} - {}", &tx.id(), e);
                return false;
            }
        };

        let effective_expiry_depth = self.config.consensus.mempool.tx_anchor_expiry_depth as u32
            + self.config.consensus.block_migration_depth
            + 5;

        let resolved_expiry_depth = current_height.saturating_sub(effective_expiry_depth as u64);

        let should_prune = anchor_height < resolved_expiry_depth;
        debug!(
            "TX {} anchor {} height {}, expiry is set to <{}, should prune? {}",
            &tx.id(),
            &tx.anchor(),
            &anchor_height,
            &resolved_expiry_depth,
            &should_prune
        );
        should_prune
    }

    /// Re-validates the anchors for every tx, using `validate_anchor_for_expiry`
    /// txs that are no longer valid are removed from the mempool and marked as invalid so we no longer accept them
    #[instrument(skip_all)]
    pub async fn prune_pending_txs(&self) {
        let current_height = match self.get_latest_block_height() {
            Ok(height) => height,
            Err(e) => {
                error!(
                    "Error getting latest block height from the block tree for anchor expiry: {:?}",
                    &e
                );
                return;
            }
        };
        // re-process all valid data txs
        let tx_ids = self.mempool_state.all_valid_submit_ledger_ids().await;
        for tx_id in tx_ids {
            let tx = {
                // TODO: change this so it's an Arc<DataTransactionHeader> so we can cheaply clone across lock points
                self.mempool_state
                    .valid_submit_ledger_tx_cloned(&tx_id)
                    .await
            };

            if let Some(tx) = tx
                && self.should_prune_tx(current_height, &tx)
            {
                self.mempool_state
                    .remove_valid_submit_ledger_tx(&tx_id)
                    .await;
                self.mempool_state
                    .mark_tx_as_invalid(tx_id, TxIngressError::InvalidAnchor(tx.anchor))
                    .await;
            }
        }

        // re-process all valid commitment txs
        let addresses = self
            .mempool_state
            .all_valid_commitment_ledger_addresses()
            .await;
        for address in addresses {
            let txs = self
                .mempool_state
                .valid_commitment_txs_cloned(&address)
                .await;

            if let Some(txs) = txs {
                for tx in txs {
                    if self.should_prune_tx(current_height, &tx) {
                        self.mempool_state.remove_commitment_tx(&tx.id()).await;
                        self.mempool_state
                            .mark_tx_as_invalid(tx.id(), TxIngressError::InvalidAnchor(tx.anchor()))
                            .await;
                    }
                }
            }
        }
    }

    fn get_confirmed_range(
        &self,
        fork: &[Arc<SealedBlock>],
    ) -> eyre::Result<Vec<Arc<SealedBlock>>> {
        let migration_depth = self.config.consensus.block_migration_depth;
        let fork_len: u32 = fork.len().try_into()?;
        let end_index: usize = migration_depth.min(fork_len).try_into()?;

        Ok(fork[0..end_index].to_vec())
    }

    /// Handles reorging confirmed commitments (system ledger txs)
    /// Goals:
    /// - resubmit orphaned commitments to the mempool
    /// Steps:
    /// 1) slice just the confirmed block ranges for each fork (old and new)
    /// 2) reduce down both forks to a `HashMap<SystemLedger, HashSet<IrysTransactionId>>`
    /// 3) reduce down to a set of SystemLedger specific orphaned transactions
    /// 4) resubmit these orphaned commitment transactions to the mempool
    #[tracing::instrument(level = "trace", skip_all, fields(fork_parent.height = event.fork_parent.height))]
    pub async fn handle_confirmed_commitment_tx_reorg(
        &self,
        event: &ReorgEvent,
    ) -> eyre::Result<()> {
        let ReorgEvent {
            old_fork, new_fork, ..
        } = event;

        let old_fork_confirmed = self.get_confirmed_range(old_fork)?;
        let new_fork_confirmed = self.get_confirmed_range(new_fork)?;

        // reduce down the system tx ledgers (or well, ledger)

        let reduce_system_ledgers = |fork: &[Arc<SealedBlock>]| -> eyre::Result<
            HashMap<SystemLedger, HashSet<IrysTransactionId>>,
        > {
            let mut ledger_txs_map = HashMap::<SystemLedger, HashSet<IrysTransactionId>>::new();
            for ledger in SystemLedger::ALL {
                // blocks can not have a system ledger if it's empty, so we pre-populate the HM with all possible ledgers so the diff algo has an easier time
                ledger_txs_map.insert(ledger, HashSet::new());
            }
            for block in fork.iter().rev() {
                for ledger in block.header().system_ledgers.iter() {
                    // we shouldn't need to deduplicate txids, but we use a HashSet for efficient set operations & as a dedupe
                    ledger_txs_map
                        .entry(ledger.ledger_id.try_into()?)
                        .or_default()
                        .extend(ledger.tx_ids.iter());
                }
            }
            Ok(ledger_txs_map)
        };

        let old_fork_reduction = reduce_system_ledgers(&old_fork_confirmed)?;
        let new_fork_reduction = reduce_system_ledgers(&new_fork_confirmed)?;

        // diff the two
        let mut orphaned_system_txs: HashMap<SystemLedger, Vec<IrysTransactionId>> = HashMap::new();

        for ledger in SystemLedger::ALL {
            let new_txs = new_fork_reduction
                .get(&ledger)
                .expect("should be populated");
            let old_txs = old_fork_reduction
                .get(&ledger)
                .expect("should be populated");
            // get the txs in old that are not in new (orphans)
            // add them to the orphaned map
            orphaned_system_txs
                .entry(ledger)
                .or_default()
                .extend(old_txs.difference(new_txs));
        }

        // resubmit orphaned system txs

        // since these are orphaned from our ""old"" fork, they should be accessible
        // extract orphaned from commitment snapshot
        let mut orphaned_full_commitment_txs =
            HashMap::<IrysTransactionId, CommitmentTransaction>::new();
        let orphaned_commitment_tx_ids = orphaned_system_txs
            .remove(&SystemLedger::Commitment)
            .ok_or_eyre("Should be populated")?;

        for block in old_fork.iter().rev() {
            let entry = self
                .block_tree_read_guard
                .read()
                .get_commitment_snapshot(&block.header().block_hash)?;
            let all_commitments = entry.get_epoch_commitments();

            // extract all the commitment txs
            // TODO: change this so the above orphan code creates a block height/hash -> orphan tx list mapping
            // so this is more efficient & so we can do block-by-block asserts
            for orphan_commitment_tx_id in orphaned_commitment_tx_ids.iter() {
                if let Some(commitment_tx) = all_commitments
                    .iter()
                    .find(|c| c.id() == *orphan_commitment_tx_id)
                {
                    orphaned_full_commitment_txs
                        .insert(*orphan_commitment_tx_id, commitment_tx.clone());
                };
            }
        }

        eyre::ensure!(
            orphaned_full_commitment_txs.iter().len() == orphaned_commitment_tx_ids.iter().len(),
            "Should always be able to get all orphaned commitment transactions"
        );

        // Clear in-memory included_height for orphaned commitment transactions before resubmitting.
        // DB metadata is already cleared by BlockMigrationService::persist_metadata() in BlockTreeService.
        for id in orphaned_commitment_tx_ids.iter() {
            if self
                .mempool_state
                .clear_commitment_tx_included_height(*id)
                .await
            {
                tracing::debug!(tx.id = %id, "Cleared included_height for orphaned commitment tx in mempool");
            }
        }

        // resubmit each commitment tx
        for (id, orphaned_full_commitment_tx) in orphaned_full_commitment_txs {
            if let Err(e) = self
                .handle_ingress_commitment_tx_message_gossip(orphaned_full_commitment_tx)
                .await
                .inspect_err(|e| {
                    error!(
                        "Error resubmitting orphaned commitment tx {}: {:?}",
                        &id, &e
                    )
                })
            {
                error!(
                    "Failed to resubmit orphaned commitment tx {}: {:?}",
                    &id, &e
                );
            }
        }

        Ok(())
    }

    /// Handles reorging confirmed data (Submit, Publish) ledger transactions
    /// Goals:
    /// - resubmit orphaned submit txs to the mempool
    /// - handle orphaned promotions
    ///     - ensure that the mempool's state doesn't have an associated ingress proof for these txs
    ///         this is so when get_best_mempool_txs is called, the txs are eligible for promotion again.
    /// - handle double-promotions (promoted in both forks)
    ///     - ensure the transaction is associated only with the ingress proofs from the new fork
    ///         this is so transactions are only associated with the canonical ingress proofs responsible for their promotion
    /// Steps:
    /// 1) slice just the confirmed block ranges for each fork (old and new)
    /// 2) reduce down both forks to a `HashMap<DataLedger, HashSet<IrysTransactionId>>`
    ///     with a secondary `HashMap<DataLedger, HashMap<IrysTransactionId, Arc<IrysBlockHeader>>>` for reverse txid -> block lookups
    /// 3) reduce these reductions down to just the list of orphaned transactions, using a set diff
    /// 4) handle orphaned Submit transactions
    ///     4.1) re-submit them back to the mempool
    /// 5) handle orphaned Publish txs
    ///     5.1) remove the orphaned ingress proofs from the mempool state
    /// 6) handle double promotions (when a publish tx is promoted in both forks)
    ///     6.1) get the associated proof from the new fork
    ///     6.2) update mempool state valid_submit_ledger_tx to store the correct ingress proof
    #[tracing::instrument(level = "trace", skip_all, fields(fork_parent.height = event.fork_parent.height))]
    pub async fn handle_confirmed_data_tx_reorg(&self, event: &ReorgEvent) -> eyre::Result<()> {
        let ReorgEvent {
            old_fork, new_fork, ..
        } = event;

        // get the range of confirmed blocks from the old fork
        // we will then reduce these down into a list of txids, which we will check against the *entire* new fork block range
        // this is because a tx that is in the tip block of the old fork could be included in the base block of the new fork

        let old_fork_confirmed = self.get_confirmed_range(old_fork)?;
        let new_fork_confirmed = self.get_confirmed_range(new_fork)?;

        // reduce the old fork and new fork into a list of ledger-specific txids
        let reduce_data_ledgers = |fork: &[Arc<SealedBlock>]| -> eyre::Result<(
            HashMap<DataLedger, HashSet<IrysTransactionId>>,
            HashMap<DataLedger, HashMap<IrysTransactionId, Arc<SealedBlock>>>,
        )> {
            let mut ledger_txs_map = HashMap::new();
            let mut tx_block_map = HashMap::new();
            for ledger in DataLedger::ALL {
                // blocks can not have a data ledger if it's empty, so we pre-populate the HM with all possible ledgers so the diff algo has an easier time
                ledger_txs_map.insert(ledger, HashSet::new());
                tx_block_map.insert(ledger, HashMap::new());
            }
            for block in fork.iter().rev() {
                for ledger in block.header().data_ledgers.iter() {
                    let ledger_id: DataLedger = ledger.ledger_id.try_into()?;
                    // we shouldn't need to deduplicate txids, but we use a HashSet for efficient set operations & as a dedupe
                    ledger_txs_map
                        .entry(ledger_id)
                        .or_default()
                        .extend(ledger.tx_ids.iter());
                    ledger.tx_ids.iter().for_each(|tx_id| {
                        tx_block_map
                            .entry(ledger_id)
                            .or_default()
                            .insert(*tx_id, Arc::clone(block));
                    });
                }
            }
            Ok((ledger_txs_map, tx_block_map))
        };

        let (old_fork_confirmed_reduction, _) = reduce_data_ledgers(&old_fork_confirmed)?;

        let (new_fork_confirmed_reduction, new_fork_tx_block_map) =
            reduce_data_ledgers(&new_fork_confirmed)?;

        // diff the two
        let mut orphaned_confirmed_ledger_txs: HashMap<DataLedger, Vec<IrysTransactionId>> =
            HashMap::new();

        for ledger in DataLedger::ALL {
            let new_txs = new_fork_confirmed_reduction
                .get(&ledger)
                .expect("should be populated");
            let old_txs = old_fork_confirmed_reduction
                .get(&ledger)
                .expect("should be populated");
            // get the txs in old that are not in new (orphans)
            // add them to the orphaned map
            let orphaned_txs = old_txs.difference(new_txs).collect::<Vec<_>>();
            debug!(
                "{:?} Ledger reorg txs, old_confirmed: {:?}, new_confirmed: {:?}, orphaned: {:?}",
                &ledger, &old_txs, &new_txs, &orphaned_txs
            );
            orphaned_confirmed_ledger_txs
                .entry(ledger)
                .or_default()
                .extend(orphaned_txs);
        }

        // if a SUBMIT a tx is CONFIRMED in the old fork, but orphaned in the new - resubmit it to the mempool
        let submit_txs = orphaned_confirmed_ledger_txs
            .get(&DataLedger::Submit)
            .cloned()
            .unwrap_or_default();

        // Clear included_height for orphaned submit transactions
        for tx_id in submit_txs.iter().copied() {
            if self
                .mempool_state
                .clear_data_tx_included_height(tx_id)
                .await
            {
                tracing::debug!(tx.id = %tx_id, "Cleared included_height for orphaned submit tx");
            }
        }

        // Extract full orphaned submit transactions
        let mut orphaned_submit_tx_map = HashMap::new();
        for block in old_fork_confirmed.iter() {
            for tx in block.transactions().get_ledger_txs(DataLedger::Submit) {
                orphaned_submit_tx_map.insert(tx.id, tx.clone());
            }
        }

        // 2. Re-post any reorged submit ledger transactions though handle_tx_ingress_message so account balances and anchors are checked
        // 3. Filter out any invalidated transactions
        for tx_id in submit_txs.iter() {
            if let Some(tx) = orphaned_submit_tx_map.remove(tx_id) {
                // TODO: handle errors better
                // note: the Skipped error is valid, so we'll need to match over the errors and abort on problematic ones (if/when appropriate)
                if let Err(e) = self
                    .handle_data_tx_ingress_message_gossip(tx)
                    .await
                    .inspect_err(|e| error!("Error re-submitting orphaned tx {} {:?}", tx_id, &e))
                {
                    error!("Failed to re-submit orphaned tx {} {:?}", tx_id, &e);
                }
            } else {
                warn!("Unable to get orphaned tx {:?} from sealed blocks", tx_id)
            }
        }

        // 4. If a transaction was promoted in the orphaned fork but not the new canonical chain, clear promotion state in mempool (promoted_height = None)

        // get the confirmed (but not published) publish ledger txs from the old fork
        let orphaned_confirmed_publish_txs = orphaned_confirmed_ledger_txs
            .get(&DataLedger::Publish)
            .cloned()
            .unwrap_or_default();

        // Clear included_height for orphaned publish transactions
        for tx_id in orphaned_confirmed_publish_txs.iter().copied() {
            if self
                .mempool_state
                .clear_data_tx_included_height(tx_id)
                .await
            {
                tracing::debug!(tx.id = %tx_id, "Cleared included_height for orphaned publish tx");
            }
        }

        // these txs have been confirmed, but NOT migrated
        for tx_id in orphaned_confirmed_publish_txs.iter().copied() {
            debug!("reorging orphaned publish tx: {}", &tx_id);
            // Clear promotion state for txs that were promoted on an orphaned fork
            if let Err(e) = self.mark_unpromoted_in_mempool(tx_id).await {
                warn!(
                    tx.id = %tx_id,
                    tx.err = %e,
                    "Failed to unpromote tx during reorg"
                );
            }
        }

        // 5. If a transaction was promoted in both forks, make sure the transaction has the ingress proofs from the canonical fork
        let published_in_both: Vec<IrysTransactionId> = old_fork_confirmed_reduction
            .get(&DataLedger::Publish)
            .expect("data ledger entry")
            .intersection(
                new_fork_confirmed_reduction
                    .get(&DataLedger::Publish)
                    .expect("data ledger entry"),
            )
            .copied()
            .collect();

        debug!("published in both forks: {:?}", &published_in_both);

        // Extract dual-published transactions
        let mut published_tx_from_blocks = HashMap::new();
        for block in new_fork_confirmed.iter() {
            for tx in block.transactions().get_ledger_txs(DataLedger::Publish) {
                published_tx_from_blocks.insert(tx.id, tx.clone());
            }
        }

        // Update in-memory mempool state for txs promoted in both forks:
        // ensure they have the ingress proofs from the new canonical fork.
        // DB metadata is already handled by BlockMigrationService (clear old fork + write new fork).
        let publish_tx_block_map = new_fork_tx_block_map.get(&DataLedger::Publish).unwrap();
        for txid in published_in_both.iter() {
            if let Some(mut tx) = published_tx_from_blocks.remove(txid) {
                let promoted_in_block = publish_tx_block_map
                    .get(txid)
                    .unwrap_or_else(|| panic!("new fork publish_tx_block_map missing tx {}", txid));

                let header = promoted_in_block.header();
                let publish_ledger = &header.data_ledgers[DataLedger::Publish];

                // Get the ingress proofs for this txid (also performs some validation)
                let tx_proofs = get_ingress_proofs(publish_ledger, txid)?;

                // Set promoted_height in metadata (in-memory only)
                tx.metadata_mut().promoted_height = Some(header.height);
                // update entry
                self.mempool_state.update_submit_transaction(tx).await;
                debug!(
                    "Reorged dual-published tx with {} proofs for {}",
                    &tx_proofs.len(),
                    txid
                );
            } else {
                eyre::bail!(
                    "Unable to get dual-published tx {:?} from sealed blocks",
                    txid
                );
            }
        }

        Ok(())
    }
}
