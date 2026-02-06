use crate::block_tree_service::{BlockMigratedEvent, ReorgEvent};
use crate::mempool_service::Inner;
use crate::mempool_service::TxIngressError;
use eyre::OptionExt as _;
use irys_database::{db::IrysDatabaseExt as _, insert_tx_header};
use irys_database::{insert_commitment_tx, tx_header_by_txid};
use irys_types::{
    get_ingress_proofs, CommitmentTransaction, DataLedger, IrysBlockHeader, IrysTransactionCommon,
    IrysTransactionId, SystemLedger, H256,
};
use reth_db::Database as _;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{debug, error, info, instrument, warn};

impl Inner {
    /// read publish txs from block. Overwrite copies in mempool with proof
    #[instrument(skip_all, fields(block.hash= %block.block_hash(), block.height = %block.height()), err)]
    pub async fn handle_block_confirmed_message(
        &self,
        block: Arc<IrysBlockHeader>,
    ) -> Result<(), TxIngressError> {
        // Persist included_height for all txs in this confirmed block.
        // This drives the /v1/tx/{txId}/status endpoint's INCLUDED status.
        let submit_txids = block.data_ledgers[DataLedger::Submit].tx_ids.0.clone();
        let publish_txids = block.data_ledgers[DataLedger::Publish].tx_ids.0.clone();
        let commitment_txids = block.get_commitment_ledger_tx_ids();

        let all_tx_ids: Vec<H256> = submit_txids
            .iter()
            .chain(publish_txids.iter())
            .chain(commitment_txids.iter())
            .copied()
            .collect();

        if !all_tx_ids.is_empty() {
            // Persist included_height to database - this is critical for transaction status
            self.irys_db.update_eyre(|tx| {
                // Set included_height for data transactions
                let data_tx_ids: Vec<_> = submit_txids
                    .iter()
                    .chain(publish_txids.iter())
                    .copied()
                    .collect();
                if !data_tx_ids.is_empty() {
                    irys_database::batch_set_data_tx_included_height(
                        tx,
                        &data_tx_ids,
                        block.height,
                    )
                    .map_err(|e| eyre::eyre!("Failed to batch set data tx included_height for {} txs at block {}: {:?}", data_tx_ids.len(), block.height, e))?;
                }
                // Set included_height for commitment transactions
                if !commitment_txids.is_empty() {
                    irys_database::batch_set_commitment_tx_included_height(
                        tx,
                        &commitment_txids,
                        block.height,
                    )
                    .map_err(|e| eyre::eyre!("Failed to batch set commitment tx included_height for {} txs at block {}: {:?}", commitment_txids.len(), block.height, e))?;
                }
                Ok(())
            }).map_err(|e| TxIngressError::DatabaseError(format!("Failed to persist included_height to database: {}", e)))?;

            // Also update mempool metadata for data transactions (with overwrite for canonical blocks)
            for txid in submit_txids.iter().chain(publish_txids.iter()) {
                self.mempool_state
                    .set_data_tx_included_height_overwrite(*txid, block.height)
                    .await;
            }

            // Update commitment transactions in mempool
            for txid in commitment_txids.iter() {
                self.mempool_state
                    .set_commitment_tx_included_height(*txid, block.height)
                    .await;
            }
        }

        let published_txids = &block.data_ledgers[DataLedger::Publish].tx_ids.0;

        // Persist promoted_height for publish ledger txs.
        if !published_txids.is_empty() {
            if let Err(e) = self.irys_db.update_eyre(|tx| {
                irys_database::batch_set_data_tx_promoted_height(tx, published_txids, block.height)
                    .map_err(|e| eyre::eyre!("{:?}", e))
            }) {
                tracing::error!("Failed to batch set promoted_height in database: {}", e);
            }
        }

        if !published_txids.is_empty() {
            for txid in block.data_ledgers[DataLedger::Publish].tx_ids.0.iter() {
                // Check if tx exists in DB first
                let db_header = self
                    .irys_db
                    .view(|tx| tx_header_by_txid(tx, txid))
                    .map_err(|e| {
                        warn!("Failed to open DB read transaction: {}", e);
                        e
                    })
                    .ok()
                    .and_then(|result| match result {
                        Ok(Some(h)) => {
                            debug!("Got tx {} from DB", txid);
                            Some(h)
                        }
                        Ok(None) => {
                            debug!("Tx {} not found in DB", txid);
                            None
                        }
                        Err(e) => {
                            warn!("DB error loading tx {}: {}", txid, e);
                            None
                        }
                    });

                // Try atomic mempool update first - this holds the lock
                if let Some(promoted_header) = self
                    .mempool_state
                    .set_promoted_height(*txid, block.height)
                    .await
                {
                    // Only update DB if the tx was already in DB (to match original behavior)
                    // This prevents writing mempool-only txs to DB which would cause reorg issues
                    if let Some(ref mut db_tx) = db_header.clone() {
                        if db_tx.promoted_height().is_none() {
                            // Set promoted_height in metadata
                            db_tx.metadata_mut().promoted_height = Some(block.height);
                            if let Err(e) = self.irys_db.update(|tx| insert_tx_header(tx, db_tx)) {
                                error!("Failed to update tx header in DB for tx {}: {}", txid, e);
                            }
                        }
                    }
                    info!("Promoted tx:\n{:#?}", promoted_header);
                    continue;
                }

                // Tx not in mempool - fall back to DB path
                debug!("Tx {} not in mempool, checking DB", txid);

                let Some(mut header) = db_header else {
                    error!("No transaction header found for txid: {}", txid);
                    continue;
                };

                if header.promoted_height().is_none() {
                    // Set promoted_height in metadata
                    header.metadata_mut().promoted_height = Some(block.height);
                }

                // Update DB with promoted header
                if let Err(e) = self.irys_db.update(|tx| insert_tx_header(tx, &header)) {
                    error!("Failed to update tx header in DB for tx {}: {}", txid, e);
                }

                // Also insert into mempool for consistency
                self.mempool_state
                    .bounded_insert_data_tx(header.clone())
                    .await;

                info!("Promoted tx (from DB):\n{:#?}", header);
            }
        }

        // Update `CachedDataRoots` so that this block_hash is cached for each data_root
        let submit_txids = block.data_ledgers[DataLedger::Submit].tx_ids.0.clone();
        let submit_tx_headers = self.handle_get_data_tx_message(submit_txids).await;

        for (i, submit_tx) in submit_tx_headers.iter().enumerate() {
            let Some(submit_tx) = submit_tx else {
                error!(
                    "No transaction header found for txid: {}",
                    block.data_ledgers[DataLedger::Submit].tx_ids.0[i]
                );
                continue;
            };

            let data_root = submit_tx.data_root;
            match self.irys_db.update_eyre(|db_tx| {
                irys_database::cache_data_root(db_tx, submit_tx, Some(&block))?;
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

        // TODO: Implement mempool-specific reorg handling
        // 1. Check to see that orphaned submit ledger tx are available in the mempool if not included in the new fork (canonical chain)
        // 2. Re-post any reorged submit ledger transactions though handle_tx_ingress_message so account balances and anchors are checked
        // 3. Filter out any invalidated transactions
        // 4. If a transaction was promoted in the orphaned fork but not the new canonical chain, restore ingress proof state to mempool
        // 5. If a transaction was promoted in both forks, make sure the transaction has the ingress proofs from the canonical fork
        // 6. Similar work with commitment transactions (stake and pledge)
        //    - This may require adding some features to the commitment_snapshot so that stake/pledge tx can be rolled back and new ones applied

        // TODO: re-org support for migrated blocks

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

    /// Clears the promotion state for a data transaction in the mempool when its prior
    /// promotion occurred on an orphaned fork. This should only be invoked from reorg
    /// handling code paths to ensure that promotion state is rolled back correctly for
    /// transactions that are no longer promoted on the new canonical chain.
    ///
    /// Behavior:
    /// - If the transaction header exists in `mempool_state.valid_submit_ledger_tx`, set
    ///   `promoted_height` to `None` and update `recent_valid_tx`.
    /// - If the header is not in the mempool, leave it unchanged; do not load or insert from DB.
    /// - Logging: Emits debug logs whether the tx was updated or left unchanged.
    ///
    /// Notes:
    /// - This method only mutates in-memory mempool state. It does not persist changes to the DB.
    /// - Do not call from normal ingress paths; promotion state should be preserved during ingress.
    /// - Intended usage is within reorg handlers (e.g., `handle_confirmed_data_tx_reorg`) to
    ///   revert promotion for txs promoted on orphaned forks.
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

            // TODO: unwrap here? we should always be able to get the value if the key exists
            if let Some(tx) = tx {
                if self.should_prune_tx(current_height, &tx) {
                    self.mempool_state
                        .remove_valid_submit_ledger_tx(&tx_id)
                        .await;
                    self.mempool_state
                        .mark_tx_as_invalid(tx_id, TxIngressError::InvalidAnchor(tx.anchor))
                        .await;
                }
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

            // TODO: unwrap here? we should always be able to get the value if the key exists
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
        fork: &[Arc<IrysBlockHeader>],
    ) -> eyre::Result<Vec<Arc<IrysBlockHeader>>> {
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

        let reduce_system_ledgers = |fork: &Arc<Vec<Arc<IrysBlockHeader>>>| -> eyre::Result<
            HashMap<SystemLedger, HashSet<IrysTransactionId>>,
        > {
            let mut ledger_txs_map = HashMap::<SystemLedger, HashSet<IrysTransactionId>>::new();
            for ledger in SystemLedger::ALL {
                // blocks can not have a system ledger if it's empty, so we pre-populate the HM with all possible ledgers so the diff algo has an easier time
                ledger_txs_map.insert(ledger, HashSet::new());
            }
            for block in fork.iter().rev() {
                for ledger in block.system_ledgers.iter() {
                    // we shouldn't need to deduplicate txids, but we use a HashSet for efficient set operations & as a dedupe
                    ledger_txs_map
                        .entry(ledger.ledger_id.try_into()?)
                        .or_default()
                        .extend(ledger.tx_ids.iter());
                }
            }
            Ok(ledger_txs_map)
        };

        let old_fork_reduction = reduce_system_ledgers(&old_fork_confirmed.into())?;
        let new_fork_reduction = reduce_system_ledgers(&new_fork_confirmed.into())?;

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
            .get(&SystemLedger::Commitment)
            .ok_or_eyre("Should be populated")?
            .clone();

        for block in old_fork.iter().rev() {
            let entry = self
                .block_tree_read_guard
                .read()
                .get_commitment_snapshot(&block.block_hash)?;
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

        // Clear included_height for orphaned commitment transactions before resubmitting
        for id in orphaned_commitment_tx_ids.iter() {
            // Clear in-memory mempool state
            if self
                .mempool_state
                .clear_commitment_tx_included_height(*id)
                .await
            {
                tracing::debug!(tx.id = %id, "Cleared included_height for orphaned commitment tx in mempool");
            }

            // Also clear the persisted included_height in the database
            if let Err(e) = self.irys_db.update_eyre(|tx| {
                irys_database::clear_commitment_tx_metadata(tx, id)
                    .map_err(|e| eyre::eyre!("{:?}", e))
            }) {
                tracing::warn!(tx.id = %id, error = %e, "Failed to clear included_height in DB for orphaned commitment tx");
            } else {
                tracing::debug!(tx.id = %id, "Cleared included_height for orphaned commitment tx in DB");
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
        let reduce_data_ledgers = |fork: &Arc<Vec<Arc<IrysBlockHeader>>>| -> eyre::Result<(
            HashMap<DataLedger, HashSet<IrysTransactionId>>,
            HashMap<DataLedger, HashMap<IrysTransactionId, Arc<IrysBlockHeader>>>,
        )> {
            let mut ledger_txs_map = HashMap::new();
            let mut tx_block_map = HashMap::new();
            for ledger in DataLedger::ALL {
                // blocks can not have a data ledger if it's empty, so we pre-populate the HM with all possible ledgers so the diff algo has an easier time
                ledger_txs_map.insert(ledger, HashSet::new());
                tx_block_map.insert(ledger, HashMap::new());
            }
            for block in fork.iter().rev() {
                for ledger in block.data_ledgers.iter() {
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

        let (old_fork_confirmed_reduction, _) = reduce_data_ledgers(&old_fork_confirmed.into())?;

        let (new_fork_confirmed_reduction, new_fork_tx_block_map) =
            reduce_data_ledgers(&new_fork_confirmed.into())?;

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

        // these txs should be present in the database, as they're part of the (technically sort of still current) chain
        let full_orphaned_submit_txs = self.handle_get_data_tx_message(submit_txs.clone()).await;

        // 2. Re-post any reorged submit ledger transactions though handle_tx_ingress_message so account balances and anchors are checked
        // 3. Filter out any invalidated transactions
        for (idx, tx) in full_orphaned_submit_txs.clone().into_iter().enumerate() {
            if let Some(tx) = tx {
                let tx_id = tx.id;
                // TODO: handle errors better
                // note: the Skipped error is valid, so we'll need to match over the errors and abort on problematic ones (if/when appropriate)
                if let Err(e) = self
                    .handle_data_tx_ingress_message_gossip(tx)
                    .await
                    .inspect_err(|e| error!("Error re-submitting orphaned tx {} {:?}", &tx_id, &e))
                {
                    error!("Failed to re-submit orphaned tx {} {:?}", &tx_id, &e);
                }
            } else {
                warn!("Unable to get orphaned tx {:?}", &submit_txs.get(idx))
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

        // Clear promoted_height in database for publish-ledger txs orphaned by the reorg.
        if !orphaned_confirmed_publish_txs.is_empty() {
            if let Err(e) = self.irys_db.update_eyre(|tx| {
                irys_database::batch_clear_data_tx_promoted_height(
                    tx,
                    &orphaned_confirmed_publish_txs,
                )
                .map_err(|e| eyre::eyre!("{:?}", e))
            }) {
                error!(
                    "Failed to batch clear promoted_height in database during reorg: {}",
                    e
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

        let full_published_txs = self
            .handle_get_data_tx_message(published_in_both.clone())
            .await;

        let publish_tx_block_map = new_fork_tx_block_map.get(&DataLedger::Publish).unwrap();
        let mut promoted_height_updates: Vec<(H256, u64)> = Vec::new();
        for (idx, tx) in full_published_txs.into_iter().enumerate() {
            if let Some(mut tx) = tx {
                let txid = tx.id;
                let promoted_in_block = publish_tx_block_map.get(&tx.id).unwrap_or_else(|| {
                    panic!("new fork publish_tx_block_map missing tx {}", &tx.id)
                });

                let publish_ledger = &promoted_in_block.data_ledgers[DataLedger::Publish];

                // Get the ingress proofs for this txid (also performs some validation)
                let tx_proofs = get_ingress_proofs(publish_ledger, &txid)?;

                // Set promoted_height in metadata
                tx.metadata_mut().promoted_height = Some(promoted_in_block.height);
                promoted_height_updates.push((txid, promoted_in_block.height));
                // update entry
                self.mempool_state.update_submit_transaction(tx).await;
                debug!(
                    "Reorged dual-published tx with {} proofs for {}",
                    &tx_proofs.len(),
                    &txid
                );
            } else {
                eyre::bail!(
                    "Unable to get dual-published tx {:?}",
                    &published_in_both.get(idx)
                );
            }
        }

        // Ensure the metadata table reflects the canonical promoted heights for txs published in both forks.
        if !promoted_height_updates.is_empty() {
            if let Err(e) = self.irys_db.update_eyre(|db_tx| {
                for (tx_id, height) in &promoted_height_updates {
                    irys_database::set_data_tx_promoted_height(db_tx, tx_id, *height)
                        .map_err(|e| eyre::eyre!("{:?}", e))?;
                }
                Ok(())
            }) {
                error!(
                    "Failed to update promoted_height in database during reorg reconciliation: {}",
                    e
                );
            }
        }

        // Batch clear included_height in database for all orphaned data transactions
        let all_orphaned_tx_ids: Vec<H256> = orphaned_confirmed_ledger_txs
            .values()
            .flat_map(|txs| txs.iter().copied())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        if !all_orphaned_tx_ids.is_empty() {
            if let Err(e) = self.irys_db.update_eyre(|tx| {
                irys_database::batch_clear_data_tx_metadata(tx, &all_orphaned_tx_ids)
                    .map_err(|e| eyre::eyre!("{:?}", e))
            }) {
                error!(
                    "Failed to batch clear included_height in database during reorg: {}",
                    e
                );
            }
        }

        Ok(())
    }

    /// When a block is migrated from the block_tree to the block_index at the migration depth
    /// it moves from "the cache" (largely the mempool) to "the index" (long term storage, usually
    /// in a database or disk)
    #[tracing::instrument(level = "trace", skip_all, fields(block.hash = %event.block.block_hash, block.height = event.block.height))]
    pub async fn handle_block_migrated(&self, event: BlockMigratedEvent) -> eyre::Result<()> {
        tracing::debug!(
            "Processing block migrated broadcast: {} height: {}",
            event.block.block_hash,
            event.block.height
        );

        let migrated_block = (*event.block).clone();
        // Yet the code is architected in a way where we could migrate a block without a PoA chunk being present.
        eyre::ensure!(
            migrated_block.poa.chunk.is_some(),
            "poa chunk must be present"
        );

        // Use transactions directly from the event instead of fetching from mempool
        let transactions = &event.transactions;

        // stage 1: move commitment transactions from tree to index
        // Use commitment transactions directly from the event
        let commitment_txs = &transactions.commitment_txs;
        let commitment_tx_ids: Vec<H256> = commitment_txs
            .iter()
            .map(irys_types::CommitmentTransaction::id)
            .collect();

        // Remove all commitments from mempool in one batch operation
        self.mempool_state
            .remove_commitment_txs(commitment_txs.iter().map(CommitmentTransaction::id))
            .await;

        // stage 1: insert commitment transactions into database
        self.irys_db.update_eyre(|tx| {
            for commitment_tx in commitment_txs {
                // Insert the commitment transaction in to the db, perform migration
                insert_commitment_tx(tx, commitment_tx)?;
            }
            Ok(())
        })?;

        // stage 2: move submit transactions from tree to index
        // Use submit transactions directly from the event
        let submit_txs = transactions
            .data_txs
            .get(&DataLedger::Submit)
            .cloned()
            .unwrap_or_default();
        let submit_tx_ids: Vec<H256> = submit_txs.iter().map(|tx| tx.id).collect();
        {
            self.irys_db.update_eyre(|tx| {
                for header in &submit_txs {
                    if let Err(err) = insert_tx_header(tx, header) {
                        error!(
                            "Could not insert transaction header - txid: {} err: {}",
                            header.id, err
                        );
                    }
                }
                Ok(())
            })?;
        }

        // stage 3: publish txs: update submit transactions in the index now they have ingress proofs
        // Use publish transactions directly from the event
        let publish_txs = transactions
            .data_txs
            .get(&DataLedger::Publish)
            .cloned()
            .unwrap_or_default();
        let publish_tx_ids: Vec<H256> = publish_txs.iter().map(|tx| tx.id).collect();
        {
            self.irys_db.update_eyre(|mut_tx| {
                for mut header in publish_txs {
                    if header.promoted_height().is_none() {
                        // Set promoted_height in metadata
                        header.metadata_mut().promoted_height = Some(event.block.height);
                    }

                    if let Err(err) = insert_tx_header(mut_tx, &header) {
                        error!(
                            "Could not insert transaction header - txid: {} err: {}",
                            header.id, err
                        );
                    }
                }
                Ok(())
            })?;
        }

        let mempool_state = &self.mempool_state.clone();

        // Remove the submit tx from the pending valid_submit_ledger_tx pool
        mempool_state
            .remove_transactions_from_pending_valid_pool(&submit_tx_ids)
            .await;

        // Fallback: ensure included_height metadata exists for all migrated txs.
        // (INCLUDED should already have been set at block confirmation time.)
        let block_height = event.block.height;

        // Persist metadata to database in batch for all migrated transactions
        let all_tx_ids: Vec<H256> = submit_tx_ids
            .iter()
            .chain(publish_tx_ids.iter())
            .chain(commitment_tx_ids.iter())
            .copied()
            .collect();

        if !all_tx_ids.is_empty() {
            if let Err(e) = self.irys_db.update_eyre(|tx| {
                // Set included_height for data transactions
                let data_tx_ids: Vec<_> = submit_tx_ids
                    .iter()
                    .chain(publish_tx_ids.iter())
                    .copied()
                    .collect();
                if !data_tx_ids.is_empty() {
                    irys_database::batch_set_data_tx_included_height(
                        tx,
                        &data_tx_ids,
                        block_height,
                    )
                    .map_err(|e| eyre::eyre!("{:?}", e))?;
                }
                // Set included_height for commitment transactions
                if !commitment_tx_ids.is_empty() {
                    irys_database::batch_set_commitment_tx_included_height(
                        tx,
                        &commitment_tx_ids,
                        block_height,
                    )
                    .map_err(|e| eyre::eyre!("{:?}", e))?;
                }
                Ok(())
            }) {
                error!("Failed to batch set included_height in database: {}", e);
            }
        }

        // Fallback: ensure promoted_height metadata exists for publish-ledger txs in this migrated block.
        if !publish_tx_ids.is_empty() {
            if let Err(e) = self.irys_db.update_eyre(|tx| {
                irys_database::batch_set_data_tx_promoted_height(tx, &publish_tx_ids, block_height)
                    .map_err(|e| eyre::eyre!("{:?}", e))
            }) {
                error!("Failed to batch set promoted_height in database: {}", e);
            }
        }

        // add block with optional poa chunk to index
        self.irys_db
            .update_eyre(|tx| irys_database::insert_block_header(tx, &migrated_block))?;

        Ok(())
    }
}
