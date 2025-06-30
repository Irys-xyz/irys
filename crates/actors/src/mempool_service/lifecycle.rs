use crate::block_tree_service::{BlockMigratedEvent, ReorgEvent};
use crate::mempool_service::Inner;
use crate::mempool_service::TxIngressError;
use eyre::eyre;
use eyre::OptionExt as _;
use irys_database::{db::IrysDatabaseExt as _, insert_tx_header};
use irys_database::{insert_commitment_tx, SystemLedger};
use irys_types::{CommitmentTransaction, DataLedger, IrysBlockHeader, IrysTransactionId, H256};
use reth_db::{transaction::DbTx as _, Database as _};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

impl Inner {
    /// read publish txs from block. Overwrite copies in mempool with proof
    pub async fn handle_block_confirmed_message(
        &mut self,
        block: Arc<IrysBlockHeader>,
    ) -> Result<(), TxIngressError> {
        let published_txids = &block.data_ledgers[DataLedger::Publish].tx_ids.0;

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

                // debug_assert!(
                //     mempool_state_write_guard
                //         .valid_submit_ledger_tx
                //         .contains_key(&tx_header.id),
                //     "assert failed: valid_submit_ledger_tx {} {}",
                //     &tx_header.id,
                //     &block.block_hash
                // );

                // debug_assert!(
                //     mempool_state_write_guard
                //         .recent_valid_tx
                //         .contains(&tx_header.id),
                //     "assert failed: recent_valid_tx {} {}",
                //     &tx_header.id,
                //     &block.block_hash
                // );

                mempool_state_write_guard
                    .valid_submit_ledger_tx
                    .insert(tx_header.id, tx_header.clone());
                mempool_state_write_guard
                    .recent_valid_tx
                    .insert(tx_header.id);
                drop(mempool_state_write_guard);

                info!("Promoted tx:\n{:?}", tx_header);
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

        // currently done (`handle_confirmed_data_tx_reorg`)

        // block confirmation does:
        // 1.) grabs all publish txs, then
        // for each publish tx:
        // 1.1) grab their full header
        // 1.2) add the proof into the header
        // 1.3) update `valid_submit_ledger_tx` to include the published tx -- this is fine
        // 1.4) update the `recent_valid_tx` to include the tx's ID

        // reorg logic
        // 1.) get the orphan transactions, by ledger, by comparing the old fork with the new fork
        // and finding all the transactions that are in the old fork but not in the new
        // for every orphan transaction:
        // if it's part the [`DataLedger::Submit`]:
        // 1.2) resubmit it to the mempool
        // if it's part of [`DataLedger::Publish`]:
        // (as `handle_block_confirmed_message` only processes Publish txs)
        // 1.1) remove it from valid_submit_ledger_tx
        // 1.2) remove it from recent_valid_tx
        // 1.3) remove the ingress proof from the header
        // 1.4) resubmit it to the mempool

        // needs to be done:
        // re-org support for migrated blocks

        // block migration does:
        // 1.) moves commitment transactions from tree to index
        // this means:
        // 1.1) get all commitment txs (ids -> full headers)
        // 1.2) insert these into the database
        // 1.3) remove the commitment tx from the mempool

        // 2.) move submit transactions from tree to index
        // for each submit tx:
        // 2.1) get the full tx header from the mempool
        // 2.2) insert it into the database

        // 3.) publish txs: update submit transactions in the index now they have ingress proofs
        // for each publish tx:
        // 3.1) get the full tx header
        // 3.2) insert it into the database

        // 4.) Remove the submit tx from the pending valid_submit_ledger_tx pool
        // for every submit tx:
        // 4.1) remove it from the `valid_submit_ledger_tx` list
        // 4.2) remove it from the `recent_valid_tx` list

        // 5.) add block with optional poa chunk to index
        // 5.1) grab the POA chunk from `prevalidated_blocks_poa`
        // 5.2) insert it & the block header into the database

        // 6.) Remove migrated block and poa chunk from mempool cache
        // 6.1) remove the header from `prevalidated_blocks`
        // 6.2) remove the the POA from `prevalidated_blocks_poa`

        self.handle_confirmed_data_tx_reorg(event.clone()).await?;

        // self.handle_commitment_tx_reorg(event).await?;

        tracing::info!("Reorg handled, new tip: {}", &new_tip);
        Ok(())
    }

    pub async fn handle_commitment_tx_reorg(&mut self, event: ReorgEvent) -> eyre::Result<()> {
        let ReorgEvent {
            old_fork, new_fork, ..
        } = event;

        // reduce down the system tx ledgers (or well, ledger)

        let reduce_system_ledgers = |fork: &Arc<Vec<Arc<IrysBlockHeader>>>| -> eyre::Result<
            HashMap<SystemLedger, HashSet<IrysTransactionId>>,
        > {
            let mut hm = HashMap::<SystemLedger, HashSet<IrysTransactionId>>::new();
            for ledger in SystemLedger::ALL {
                // blocks can not have a system ledger if it's empty, so we pre-populate the HM with all possible ledgers so the diff algo has an easier time
                hm.insert(ledger, HashSet::new());
            }
            for block in fork.iter().rev() {
                for ledger in block.system_ledgers.iter() {
                    // we shouldn't need to deduplicate txids, but we use a HashSet for efficient set operations & as a dedupe
                    hm.entry(ledger.ledger_id.try_into()?)
                        .or_default()
                        .extend(ledger.tx_ids.iter());
                }
            }
            Ok(hm)
        };

        let old_fork_red = reduce_system_ledgers(&old_fork)?;
        let new_fork_red = reduce_system_ledgers(&new_fork)?;

        // diff the two
        let mut orphaned_system_txs: HashMap<SystemLedger, Vec<IrysTransactionId>> = HashMap::new();

        for ledger in SystemLedger::ALL {
            let new_txs = new_fork_red.get(&ledger).expect("should be populated");
            let old_txs = old_fork_red.get(&ledger).expect("should be populated");
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
            let all_commitments = entry.get_all_commitments();

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
        assert_eq!(
            orphaned_full_commitment_txs.iter().len(),
            orphaned_commitment_tx_ids.iter().len()
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

    pub async fn handle_confirmed_data_tx_reorg(&mut self, event: ReorgEvent) -> eyre::Result<()> {
        let ReorgEvent {
            old_fork, new_fork, ..
        } = event;

        // we check for orphaned confirmed transactions from our fork, against the entire block range of the new fork

        // get the range of confirmed blocks from the old fork
        // we will then reduce these down into a list of txids, which we will check against the *entire* new fork block range
        // this is because a tx that is in the tip block of the old fork could be included in the base block of the new fork
        let old_fork_confirmed = old_fork[0_usize
            ..(self
                .config
                .consensus
                .block_migration_depth
                .min(old_fork.len().try_into()?))
            .try_into()?]
            .to_vec();

        let new_fork_confirmed = new_fork[0_usize
            ..(self
                .config
                .consensus
                .block_migration_depth
                .min(new_fork.len().try_into()?))
            .try_into()?]
            .to_vec();

        // reduce the old fork and new fork into a list of ledger-specific txids
        // todo: if we only use the reductions to produce orphaned_ledger_txs, combine the logic
        let reduce_data_ledgers = |fork: &Arc<Vec<Arc<IrysBlockHeader>>>| -> eyre::Result<(
            HashMap<DataLedger, HashSet<IrysTransactionId>>,
            HashMap<IrysTransactionId, Arc<IrysBlockHeader>>,
        )> {
            let mut hm = HashMap::new();
            let mut tx_blk_map = HashMap::new();
            for ledger in DataLedger::ALL {
                // blocks can not have a data ledger if it's empty, so we pre-populate the HM with all possible ledgers so the diff algo has an easier time
                hm.insert(ledger, HashSet::new());
            }
            for block in fork.iter().rev() {
                for ledger in block.data_ledgers.iter() {
                    // we shouldn't need to deduplicate txids, but we use a HashSet for efficient set operations & as a dedupe
                    hm.entry(ledger.ledger_id.try_into()?)
                        .or_default()
                        .extend(ledger.tx_ids.iter());
                    ledger.tx_ids.iter().for_each(|tx_id| {
                        tx_blk_map.insert(*tx_id, Arc::clone(block));
                    });
                }
            }
            Ok((hm, tx_blk_map))
        };

        // let old_fork_red = reduce_data_ledgers(old_fork)?;
        // reduction of the confirmed txs of the old fork
        let (old_fork_confirmed_reduction, _) = reduce_data_ledgers(&old_fork_confirmed.into())?;

        let (new_fork_confirmed_reduction, new_fork_tx_blk_map) =
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
            orphaned_confirmed_ledger_txs
                .entry(ledger)
                .or_default()
                .extend(old_txs.difference(new_txs));
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
                self.handle_data_tx_ingress_message(tx)
                    .await
                    .map_err(|e| eyre!("Error re-submitting orphaned tx {} {:?}", &tx_id, &e))?;
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
                // if the tx is in `valid_submit_ledger_tx`, update it so `ingress_proofs` is `none`
                // note: sometimes txs are *not* in this list, and I don't currently understand why

                mempool_state_write_guard
                    .valid_submit_ledger_tx
                    .entry(tx)
                    .and_modify(|tx| tx.ingress_proofs = None);
                // mempool_state_write_guard.recent_valid_tx.remove(&tx);
            }
        }

        // 5. If a transaction was promoted in both forks, make sure the transaction has the ingress proofs from the canonical fork

        let published_in_both: Vec<IrysTransactionId> = old_fork_confirmed_reduction
            .get(&DataLedger::Publish)
            .expect("data ledger entry")
            .difference(
                new_fork_confirmed_reduction
                    .get(&DataLedger::Publish)
                    .expect("data ledger entry"),
            )
            .copied()
            .collect();

        let full_published_txs = self
            .handle_get_data_tx_message(published_in_both.clone())
            .await;
        for (idx, tx) in full_published_txs.into_iter().enumerate() {
            if let Some(mut tx) = tx {
                let promoted_in_block = new_fork_tx_blk_map.get(&tx.id).unwrap();
                let publish_ledger = &promoted_in_block.data_ledgers[DataLedger::Publish];
                // get publish tx pos
                let proof_idx = publish_ledger
                    .tx_ids
                    .iter()
                    .position(|tx_id| *tx_id == tx.id)
                    .unwrap();
                let proofs = publish_ledger.proofs.clone().unwrap();
                let proof = proofs.get(proof_idx).unwrap();
                tx.ingress_proofs = Some(proof.clone());
                // update entry
                {
                    let mut mempool_state_write_guard = self.mempool_state.write().await;
                    mempool_state_write_guard
                        .valid_submit_ledger_tx
                        .insert(tx.id, tx);
                }
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
                debug!(
                    "JESSEDEBUG2 REMOVING TX {} {}",
                    &txid, &migrated_block.block_hash
                );
                mempool_state_write_guard
                    .valid_submit_ledger_tx
                    .remove(txid);
                mempool_state_write_guard.recent_valid_tx.remove(txid);
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
