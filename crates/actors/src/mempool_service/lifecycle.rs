use crate::block_tree_service::{BlockMigratedEvent, ReorgEvent};
use crate::mempool_service::Inner;
use crate::mempool_service::TxIngressError;
use eyre::OptionExt as _;
use irys_database::{db::IrysDatabaseExt as _, insert_tx_header};
use irys_database::{insert_commitment_tx, SystemLedger};
use irys_types::{CommitmentTransaction, DataLedger, IrysBlockHeader, IrysTransactionId, H256};
use reth_db::{transaction::DbTx as _, Database as _};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{debug, error, info, instrument, warn};

impl Inner {
    /// read publish txs from block. Overwrite copies in mempool with proof
    pub async fn handle_block_confirmed_message(
        &mut self,
        block: Arc<IrysBlockHeader>,
    ) -> Result<(), TxIngressError> {
        let published_txids = &block.data_ledgers[DataLedger::Publish].tx_ids.0;

        // notify that we have a new anchor
        self.notify_anchor(block.block_hash).await;

        // FIXME: Loop though the promoted transactions and insert their ingress proofs
        // into the mempool. In the future on a multi node network we may keep
        // ingress proofs around longer
        if !published_txids.is_empty() {
            for (i, txid) in block.data_ledgers[DataLedger::Publish]
                .tx_ids
                .0
                .iter()
                .enumerate()
            {
                // Retrieve the promoted transactions header
                let tx_headers_result = self.handle_get_data_tx_message(vec![*txid]).await;
                let mut tx_header = match tx_headers_result.as_slice() {
                    [Some(header)] => header.clone(),
                    [None] => {
                        error!("No transaction header found for txid: {}", txid);
                        continue;
                    }
                    _ => {
                        error!("Unexpected number of txids. Error fetching transaction header for txid: {}", txid);
                        continue;
                    }
                };

                // TODO: In a single node world there is only one ingress proof
                // per promoted tx, but in the future there will be multiple proofs.
                let proofs = block.data_ledgers[DataLedger::Publish]
                    .proofs
                    .as_ref()
                    .unwrap();
                let proof = proofs.0[i].clone();
                tx_header.ingress_proofs = Some(proof);

                // Update the header record in the mempool to include the ingress
                // proof, indicating it is promoted.
                let mempool_state = &self.mempool_state.clone();
                let mut mempool_state_write_guard = mempool_state.write().await;

                mempool_state_write_guard
                    .valid_submit_ledger_tx
                    .insert(tx_header.id, tx_header.clone());
                mempool_state_write_guard
                    .recent_valid_tx
                    .put(tx_header.id, ());
                drop(mempool_state_write_guard);

                info!("Promoted tx:\n{:?}", tx_header);
            }
        }

        // notify for all the txs in all the ledgers
        for ledger in &block.data_ledgers {
            for tx in ledger.tx_ids.iter() {
                self.notify_anchor(*tx).await;
            }
        }

        for ledger in &block.system_ledgers {
            for tx in ledger.tx_ids.iter() {
                self.notify_anchor(*tx).await;
            }
        }

        Ok(())
    }

    pub async fn handle_reorg(&mut self, event: ReorgEvent) -> eyre::Result<()> {
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

        self.revalidate_all_txs().await?;

        tracing::info!("Reorg handled, new tip: {:?}", &new_tip);
        Ok(())
    }

    /// Re-process all currently valid mempool txs
    /// all this does is take all valid submit & commitment txs, and passes them back through ingress
    /// right now, all this is used for is to validate the anchor state of a tx
    /// (when a reorg happens, the anchor a tx used to make it into valid txs could now be pending)
    #[instrument(skip_all)]
    pub async fn revalidate_all_txs(&mut self) -> eyre::Result<()> {
        // re-process all valid txs
        let (valid_submit_ledger_tx, valid_commitment_tx) = {
            let mut state = self.mempool_state.write().await;
            state.recent_valid_tx.clear();
            (
                std::mem::take(&mut state.valid_submit_ledger_tx),
                std::mem::take(&mut state.valid_commitment_tx),
            )
        };
        for (id, tx) in valid_submit_ledger_tx {
            match self.handle_data_tx_ingress_message(tx).await {
                Ok(_) => debug!("resubmitted data tx {} to mempool", &id),
                Err(err) => debug!("failed to resubmit data tx {} to mempool: {:?}", &id, &err),
            }
        }
        for (_address, txs) in valid_commitment_tx {
            for tx in txs {
                let id = tx.id;
                match self.handle_ingress_commitment_tx_message(tx).await {
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
    pub async fn handle_confirmed_commitment_tx_reorg(
        &mut self,
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
                    .find(|c| c.id == *orphan_commitment_tx_id)
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

        // resubmit each commitment tx
        for (id, orphaned_full_commitment_tx) in orphaned_full_commitment_txs {
            let _ = self
                .handle_ingress_commitment_tx_message(orphaned_full_commitment_tx)
                .await
                .inspect_err(|e| {
                    error!(
                        "Error resubmitting orphaned commitment tx {}: {:?}",
                        &id, &e
                    )
                });
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
    pub async fn handle_confirmed_data_tx_reorg(&mut self, event: &ReorgEvent) -> eyre::Result<()> {
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

        // these txs should be present in the database, as they're part of the (technically sort of still current) chain
        let full_orphaned_submit_txs = self.handle_get_data_tx_message(submit_txs.clone()).await;

        // 2. Re-post any reorged submit ledger transactions though handle_tx_ingress_message so account balances and anchors are checked
        // 3. Filter out any invalidated transactions
        for (idx, tx) in full_orphaned_submit_txs.clone().into_iter().enumerate() {
            if let Some(tx) = tx {
                let tx_id = tx.id;
                // TODO: handle errors better
                // note: the Skipped error is valid, so we'll need to match over the errors and abort on problematic ones (if/when appropriate)
                let _ = self
                    .handle_data_tx_ingress_message(tx)
                    .await
                    .inspect_err(|e| error!("Error re-submitting orphaned tx {} {:?}", &tx_id, &e));
            } else {
                warn!("Unable to get orphaned tx {:?}", &submit_txs.get(idx))
            }
        }

        // 4. If a transaction was promoted in the orphaned fork but not the new canonical chain, restore ingress proof state to mempool

        // get the confirmed (but not published) publish ledger txs from the old fork
        let orphaned_confirmed_publish_txs = orphaned_confirmed_ledger_txs
            .get(&DataLedger::Publish)
            .cloned()
            .unwrap_or_default();

        // these txs have been confirmed, but NOT migrated
        {
            let mut mempool_state_write_guard = self.mempool_state.write().await;

            for tx in orphaned_confirmed_publish_txs {
                debug!("reorging orphaned publish tx: {}", &tx);
                // if the tx is in `valid_submit_ledger_tx`, update it so `ingress_proofs` is `none`
                // note: sometimes txs are *not* in this list, and I don't currently understand why
                mempool_state_write_guard
                    .valid_submit_ledger_tx
                    .entry(tx)
                    .and_modify(|tx| tx.ingress_proofs = None);
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
        for (idx, tx) in full_published_txs.into_iter().enumerate() {
            if let Some(mut tx) = tx {
                let id = tx.id;
                let promoted_in_block = publish_tx_block_map.get(&tx.id).unwrap_or_else(|| {
                    panic!("new fork publish_tx_block_map missing tx {}", &tx.id)
                });

                let publish_ledger = &promoted_in_block.data_ledgers[DataLedger::Publish];
                // get publish tx pos
                let proof_idx = publish_ledger
                    .tx_ids
                    .iter()
                    .position(|tx_id| *tx_id == tx.id)
                    .unwrap_or_else(|| {
                        panic!(
                            "publish tx {} to be present in block {}",
                            &tx.id, &promoted_in_block.block_hash
                        )
                    });
                let proofs = publish_ledger.proofs.clone().unwrap_or_else(|| {
                    panic!(
                        "Publish ledger of block {} to have proofs",
                        &promoted_in_block.block_hash
                    )
                });
                let proof = proofs.get(proof_idx).unwrap_or_else(|| {
                    panic!(
                        "Publish ledger of block {} to have a proof at index {} for publish tx {}",
                        &promoted_in_block.block_hash, &proof_idx, &tx.id
                    )
                });
                tx.ingress_proofs = Some(proof.clone());
                // update entry
                {
                    let mut mempool_state_write_guard = self.mempool_state.write().await;
                    mempool_state_write_guard
                        .valid_submit_ledger_tx
                        .entry(tx.id)
                        .and_modify(|t| *t = tx);
                }
                debug!("Reorged dual-published proof {} for {}", &proof.proof, &id);
            } else {
                eyre::bail!(
                    "Unable to get dual-published tx {:?}",
                    &published_in_both.get(idx)
                );
            }
        }

        Ok(())
    }

    /// When a block is migrated from the block_tree to the block_index at the migration depth
    /// it moves from "the cache" (largely the mempool) to "the index" (long term storage, usually
    /// in a database or disk)
    pub async fn handle_block_migrated(&mut self, event: BlockMigratedEvent) -> eyre::Result<()> {
        tracing::debug!(
            "Processing block migrated broadcast: {} height: {}",
            event.block.block_hash,
            event.block.height
        );

        let mut migrated_block = (*event.block).clone();
        let data_ledger_txs = migrated_block.get_data_ledger_tx_ids();

        // stage 1: move commitment transactions from tree to index
        let commitment_tx_ids = migrated_block.get_commitment_ledger_tx_ids();
        let commitments = self
            .handle_get_commitment_tx_message(commitment_tx_ids)
            .await;

        let tx = self
            .irys_db
            .tx_mut()
            .expect("to get a mutable tx reference from the db");

        for commitment_tx in commitments.values() {
            // Insert the commitment transaction in to the db, perform migration
            insert_commitment_tx(&tx, commitment_tx)?;
            // Remove the commitment tx from the mempool cache, completing the migration
            self.remove_commitment_tx(&commitment_tx.id).await;
        }
        tx.inner.commit()?;

        // stage 2: move submit transactions from tree to index
        let submit_tx_ids: Vec<H256> = data_ledger_txs
            .get(&DataLedger::Submit)
            .unwrap()
            .iter()
            .copied()
            .collect();
        {
            let mut_tx = self
                .irys_db
                .tx_mut()
                .map_err(|e| {
                    error!("Failed to create mdbx transaction: {}", e);
                })
                .expect("expected to read/write to database");

            // FIXME: this next line is less efficient than it needs to be?
            //        why would we read mdbx txs when we are migrating?
            let data_tx_headers = self.handle_get_data_tx_message(submit_tx_ids.clone()).await;
            data_tx_headers
                .into_iter()
                .for_each(|maybe_header| match maybe_header {
                    Some(ref header) => {
                        if let Err(err) = insert_tx_header(&mut_tx, header) {
                            error!(
                                "Could not insert transaction header - txid: {} err: {}",
                                header.id, err
                            );
                        }
                    }
                    None => {
                        error!("Could not find transaction header in mempool");
                    }
                });
            mut_tx.commit().expect("expect to commit to database");
        }

        // stage 3: publish txs: update submit transactions in the index now they have ingress proofs
        let publish_tx_ids: Vec<H256> = data_ledger_txs
            .get(&DataLedger::Publish)
            .unwrap()
            .iter()
            .copied()
            .collect();
        {
            let mut_tx = self
                .irys_db
                .tx_mut()
                .map_err(|e| {
                    error!("Failed to create mdbx transaction: {}", e);
                })
                .expect("expected to read/write to database");

            let publish_tx_headers = self
                .handle_get_data_tx_message(publish_tx_ids.clone())
                .await;
            publish_tx_headers
                .into_iter()
                .for_each(|maybe_header| match maybe_header {
                    Some(ref header) => {
                        // When a block was confirmed, handle_block_confirmed_message() updates the mempool submit tx headers with ingress proofs
                        // this means the header includes the proofs when we now insert (overwrite existing entry) into the database
                        if let Err(err) = insert_tx_header(&mut_tx, header) {
                            error!(
                                "Could not insert transaction header - txid: {} err: {}",
                                header.id, err
                            );
                        }
                    }
                    None => {
                        error!("Could not find transaction header in mempool");
                    }
                });
            mut_tx.commit().expect("expect to commit to database");
        }

        let mempool_state = &self.mempool_state.clone();

        // Remove the submit tx from the pending valid_submit_ledger_tx pool
        {
            let mut mempool_state_write_guard = mempool_state.write().await;
            for txid in submit_tx_ids.iter() {
                mempool_state_write_guard
                    .valid_submit_ledger_tx
                    .remove(txid);
                mempool_state_write_guard.recent_valid_tx.pop(txid);
            }
        }

        // add block with optional poa chunk to index
        {
            let mempool_state_read_guard = mempool_state.read().await;
            migrated_block.poa.chunk = mempool_state_read_guard
                .prevalidated_blocks_poa
                .get(&migrated_block.block_hash)
                .cloned();
            self.irys_db
                .update_eyre(|tx| irys_database::insert_block_header(tx, &migrated_block))
                .unwrap();
        }

        // Remove migrated block and poa chunk from mempool cache
        {
            let mut state = self.mempool_state.write().await;
            state.prevalidated_blocks.remove(&migrated_block.block_hash);
            state
                .prevalidated_blocks_poa
                .remove(&migrated_block.block_hash);
        }

        Ok(())
    }
}
