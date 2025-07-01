use crate::block_tree_service::{BlockMigratedEvent, ReorgEvent};
use crate::mempool_service::Inner;
use crate::mempool_service::TxIngressError;
use base58::ToBase58 as _;
use irys_database::insert_commitment_tx;
use irys_database::{db::IrysDatabaseExt as _, insert_tx_header};
use irys_types::{DataLedger, IrysBlockHeader, H256};
use reth_db::{transaction::DbTx as _, Database as _};
use std::sync::Arc;
use tracing::{error, info};

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

    pub fn handle_reorg(&self, event: ReorgEvent) -> eyre::Result<()> {
        tracing::debug!(
            "Processing reorg: {} orphaned blocks from height {}",
            event.old_fork.len(),
            event.fork_parent.height
        );

        // TODO: Implement mempool-specific reorg handling
        // 1. Check to see that orphaned submit ledger tx are available in the mempool if not included in the new fork (canonical chain)
        // 2. Re-post any reorged submit ledger transactions though handle_tx_ingress_message so account balances and anchors are checked
        // 3. Filter out any invalidated transactions
        // 4. If a transaction was promoted in the orphaned fork but not the new canonical chain, restore ingress proof state to mempool
        // 5. If a transaction was promoted in both forks, make sure the transaction has the ingress proofs from the canonical fork
        // 6. Similar work with commitment transactions (stake and pledge)
        //    - This may require adding some features to the commitment_snapshot so that stake/pledge tx can be rolled back and new ones applied

        tracing::info!("Reorg handled, new tip: {}", event.new_tip.0.to_base58());

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
