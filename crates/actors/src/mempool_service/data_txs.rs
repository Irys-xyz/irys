use crate::block_tree_service::get_optimistic_chain;
use crate::mempool_service::{Inner, TxReadError};
use crate::mempool_service::{MempoolServiceMessage, TxIngressError};
use base58::ToBase58 as _;
use eyre::eyre;
use irys_database::{
    block_header_by_hash, db::IrysDatabaseExt as _, tables::DataRootLRU, tx_header_by_txid,
};
use irys_reth_node_bridge::ext::IrysRethRpcTestContextExt as _;
use irys_types::{
    DataLedger, GossipBroadcastMessage, IrysTransactionCommon as _, IrysTransactionHeader,
    IrysTransactionId, H256, U256,
};
use reth_db::{transaction::DbTx as _, transaction::DbTxMut as _, Database as _};
use std::collections::HashMap;
use tracing::{debug, error, info, warn};

impl Inner {
    /// check the mempool and mdbx for data transaction
    pub async fn handle_get_data_tx_message(
        &self,
        txs: Vec<H256>,
    ) -> Vec<Option<IrysTransactionHeader>> {
        let mut found_txs = Vec::with_capacity(txs.len());
        let mempool_state = &self.mempool_state.clone();
        let mempool_state_guard = mempool_state.read().await;

        for tx in txs {
            // if data tx exists in mempool
            if let Some(tx_header) = mempool_state_guard.valid_submit_ledger_tx.get(&tx) {
                found_txs.push(Some(tx_header.clone()));
                continue;
            }
            // if data tx exists in mdbx
            if let Ok(read_tx) = self.read_tx() {
                if let Some(tx_header) = tx_header_by_txid(&read_tx, &tx).unwrap_or(None) {
                    found_txs.push(Some(tx_header.clone()));
                    continue;
                }
            }
            // not found anywhere
            found_txs.push(None);
        }

        drop(mempool_state_guard);
        found_txs
    }

    pub async fn handle_data_tx_ingress_message(
        &mut self,
        tx: IrysTransactionHeader,
    ) -> Result<(), TxIngressError> {
        debug!(
            "received tx {:?} (data_root {:?})",
            &tx.id.0.to_base58(),
            &tx.data_root.0.to_base58()
        );

        let mempool_state = &self.mempool_state.clone();
        let mempool_state_read_guard = mempool_state.read().await;

        // Early out if we already know about this transaction
        if mempool_state_read_guard.invalid_tx.contains(&tx.id)
            || mempool_state_read_guard.recent_valid_tx.contains(&tx.id)
        {
            warn!("duplicate tx: {:?}", TxIngressError::Skipped);
            return Err(TxIngressError::Skipped);
        }
        drop(mempool_state_read_guard);

        // Validate anchor
        let hdr = match self.validate_anchor(&tx.id, &tx.anchor).await {
            Err(e) => {
                error!(
                    "Validation failed: {:?} - mapped to: {:?}",
                    e,
                    TxIngressError::DatabaseError
                );
                return Ok(());
            }
            Ok(v) => v,
        };

        let read_tx = self.read_tx().map_err(|_| TxIngressError::DatabaseError)?;

        // Update any associated ingress proofs
        if let Ok(Some(old_expiry)) = read_tx.get::<DataRootLRU>(tx.data_root) {
            let anchor_expiry_depth = self
                .config
                .node_config
                .consensus_config()
                .mempool
                .anchor_expiry_depth as u64;
            let new_expiry = hdr.height + anchor_expiry_depth;
            debug!(
                "Updating ingress proof for data root {} expiry from {} -> {}",
                &tx.data_root, &old_expiry.last_height, &new_expiry
            );

            self.irys_db
                .update(|write_tx| write_tx.put::<DataRootLRU>(tx.data_root, old_expiry))
                .map_err(|e| {
                    error!(
                        "Error updating ingress proof expiry for {} - {}",
                        &tx.data_root, &e
                    );
                    TxIngressError::DatabaseError
                })?
                .map_err(|e| {
                    error!(
                        "Error updating ingress proof expiry for {} - {}",
                        &tx.data_root, &e
                    );
                    TxIngressError::DatabaseError
                })?;
        }

        // Check account balance

        if self.reth_node_adapter.rpc.get_balance_irys(tx.signer, None) < U256::from(tx.total_fee())
        {
            error!(
                "{:?}: unfunded balance from irys_database::get_account_balance({:?})",
                TxIngressError::Unfunded,
                tx.signer
            );
            return Err(TxIngressError::Unfunded);
        }

        // Validate the transaction signature
        // check the result and error handle
        self.validate_signature(&tx).await?;

        let mut mempool_state_write_guard = mempool_state.write().await;
        mempool_state_write_guard
            .valid_submit_ledger_tx
            .insert(tx.id, tx.clone());
        mempool_state_write_guard.recent_valid_tx.insert(tx.id);
        drop(mempool_state_write_guard);

        // Cache the data_root in the database
        match self.irys_db.update_eyre(|db_tx| {
            irys_database::cache_data_root(db_tx, &tx)?;
            Ok(())
        }) {
            Ok(()) => {
                info!(
                    "Successfully cached data_root {:?} for tx {:?}",
                    tx.data_root,
                    tx.id.0.to_base58()
                );
            }
            Err(db_error) => {
                error!(
                    "Failed to cache data_root {:?} for tx {:?}: {:?}",
                    tx.data_root,
                    tx.id.0.to_base58(),
                    db_error
                );
            }
        };

        // Process any chunks that arrived before their parent transaction
        // These were temporarily stored in the pending_chunks cache
        let mut mempool_state_write_guard = mempool_state.write().await;
        let option_chunks_map = mempool_state_write_guard.pending_chunks.pop(&tx.data_root);
        drop(mempool_state_write_guard);
        if let Some(chunks_map) = option_chunks_map {
            // Extract owned chunks from the map to process them
            let chunks: Vec<_> = chunks_map.into_iter().map(|(_, chunk)| chunk).collect();
            for chunk in chunks {
                let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
                //todo check the value rather than _
                let _ = self
                    .handle_message(MempoolServiceMessage::IngestChunk(chunk, oneshot_tx))
                    .await;

                let msg_result = oneshot_rx
                    .await
                    .expect("pending chunks should be processed by the mempool");

                if let Err(err) = msg_result {
                    tracing::error!("oneshot failure: {:?}", err);
                    return Err(TxIngressError::Other("oneshot failure".to_owned()));
                }
            }
        }

        // Gossip transaction
        let gossip_broadcast_message = GossipBroadcastMessage::from(tx.clone());
        if let Err(error) = self
            .service_senders
            .gossip_broadcast
            .send(gossip_broadcast_message)
        {
            tracing::error!("Failed to send gossip data: {:?}", error);
        }

        Ok(())
    }

    pub async fn get_all_storage_tx(&self) -> HashMap<IrysTransactionId, IrysTransactionHeader> {
        let mut hash_map = HashMap::new();

        // first flat_map all the storage transactions
        let mempool_state = &self.mempool_state;
        let mempool_state_guard = mempool_state.read().await;

        // Get any IrysTransaction from the valid storage txs
        mempool_state_guard
            .valid_submit_ledger_tx
            .values()
            .for_each(|tx| {
                hash_map.insert(tx.id, tx.clone());
            });

        hash_map
    }

    /// checks mempool and mdbx
    pub async fn handle_data_tx_exists_message(&self, txid: H256) -> Result<bool, TxReadError> {
        let mempool_state = &self.mempool_state;
        let mempool_state_guard = mempool_state.read().await;

        #[expect(clippy::if_same_then_else, reason = "readability")]
        if mempool_state_guard
            .valid_submit_ledger_tx
            .contains_key(&txid)
        {
            Ok(true)
        } else if mempool_state_guard.recent_valid_tx.contains(&txid) {
            Ok(true)
        } else if mempool_state_guard.invalid_tx.contains(&txid) {
            // Still has it, just invalid
            Ok(true)
        } else {
            drop(mempool_state_guard);
            let read_tx = self.read_tx();

            if read_tx.is_err() {
                Err(TxReadError::DatabaseError)
            } else {
                Ok(
                    tx_header_by_txid(&read_tx.expect("expected valid header from tx id"), &txid)
                        .map_err(|_| TxReadError::DatabaseError)?
                        .is_some(),
                )
            }
        }
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
    /// A vector of `IrysTransactionHeader` representing Submit ledger transactions
    /// that are pending inclusion and have not been processed in recent blocks.
    ///
    /// # Notes
    /// - Only considers Submit ledger transactions (filters out Publish, etc.)
    /// - Only examines blocks within the configured `anchor_expiry_depth`
    pub async fn get_pending_submit_ledger_txs(&self) -> Vec<IrysTransactionHeader> {
        // Get the current canonical chain head to establish our starting point for block traversal
        let optimistic = get_optimistic_chain(self.block_tree_read_guard.clone())
            .await
            .unwrap();
        let (canonical, _) = self.block_tree_read_guard.read().get_canonical_chain();
        let canonical_head_entry = canonical.last().unwrap();

        // This is just here to catch any oddities in the debug log. The optimistic
        // and canonical should always have the same results from my reading of the code.
        // if the tests are stable and this hasn't come up it can be removed.
        if optimistic.last().unwrap().0 != canonical_head_entry.block_hash {
            debug!("Optimistic and Canonical have different heads");
        }

        let block_hash = canonical_head_entry.block_hash;
        let block_height = canonical_head_entry.height;

        // retrieve block from mempool or database
        // be aware that genesis starts its life immediately in the database
        let mut block = match self
            .handle_get_block_header_message(block_hash, false)
            .await
        {
            Some(b) => b,
            None => match self
                .irys_db
                .view_eyre(|tx| block_header_by_hash(tx, &block_hash, false))
            {
                Ok(Some(header)) => Ok(header),
                Ok(None) => Err(eyre!(
                    "No block header found for hash {} ({})",
                    block_hash,
                    block_height
                )),
                Err(e) => Err(eyre!(
                    "Failed to get previous block ({}) header: {}",
                    block_height,
                    e
                )),
            }
            .expect("to find the block header in the db"),
        };

        // Calculate the minimum block height we need to check for transaction conflicts
        // Only transactions anchored within this depth window are considered valid
        let anchor_expiry_depth = self.config.consensus.mempool.anchor_expiry_depth as u64;
        let min_anchor_height = block_height.saturating_sub(anchor_expiry_depth);

        // Start with all valid Submit ledger transactions - we'll filter out already-included ones
        let mut valid_submit_ledger_tx = self
            .mempool_state
            .read()
            .await
            .valid_submit_ledger_tx
            .clone();

        // Walk backwards through the canonical chain, removing Submit transactions
        // that have already been included in recent blocks within the anchor expiry window
        while block.height >= min_anchor_height {
            let block_data_tx_ids = block.get_data_ledger_tx_ids();

            // Check if this block contains any Submit ledger transactions
            if let Some(submit_txids) = block_data_tx_ids.get(&DataLedger::Submit) {
                // Remove Submit transactions that already exist in this historical block
                // This prevents double-inclusion and ensures we only return truly pending transactions
                for txid in submit_txids.iter() {
                    valid_submit_ledger_tx.remove(txid);
                }
            }

            // Stop if we've reached the genesis block
            if block.height == 0 {
                break;
            }

            // Move to the parent block and continue the traversal backwards
            let parent_block = match self
                .handle_get_block_header_message(block.previous_block_hash, false)
                .await
            {
                Some(h) => h,
                None => self
                    .irys_db
                    .view(|tx| {
                        irys_database::block_header_by_hash(tx, &block.previous_block_hash, false)
                    })
                    .unwrap()
                    .unwrap()
                    .expect("to find the parent block header in the database"),
            };

            block = parent_block;
        }

        // Return all remaining Submit transactions by consuming the map
        // These represent Submit transactions that are pending and haven't been included in any recent block
        valid_submit_ledger_tx.into_values().collect()
    }
}
