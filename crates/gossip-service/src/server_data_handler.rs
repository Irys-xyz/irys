use crate::types::{tx_ingress_error_to_gossip_error, InternalGossipError, InvalidDataError};
use crate::{GossipCache, GossipError, GossipResult};
use actix::{Actor, Addr, Context, Handler};
use core::net::SocketAddr;
use irys_actors::mempool_service::{
    ChunkIngressError, ChunkIngressMessage, TxExistenceQuery, TxIngressError, TxIngressMessage,
};
use irys_api_client::ApiClient;
use irys_types::{GossipData, IrysBlockHeader, IrysTransactionHeader, UnpackedChunk, H256};
use std::sync::Arc;

/// Handles data received by the `GossipServer`
#[derive(Debug)]
pub struct GossipServerDataHandler<M, A>
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    A: ApiClient + 'static,
{
    pub mempool: Addr<M>,
    pub cache: Arc<GossipCache>,
    pub api_client: A,
}

impl<M, A> GossipServerDataHandler<M, A>
where
    M: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<M>>,
    A: ApiClient,
{
    pub(crate) async fn handle_chunk(
        &self,
        chunk: UnpackedChunk,
        source_address: SocketAddr,
    ) -> GossipResult<()> {
        match self.mempool.send(ChunkIngressMessage(chunk.clone())).await {
            Ok(message_result) => {
                match message_result {
                    Ok(()) => {
                        // Success. Mempool will send the tx data to the internal mempool,
                        //  but we still need to update the cache with the source address.
                        self.cache
                            .record_seen(source_address, &GossipData::Chunk(chunk))
                    }
                    Err(error) => {
                        match error {
                            ChunkIngressError::UnknownTransaction => {
                                // TODO:
                                //  I suppose we have to ask the peer for transaction,
                                //  but what if it doesn't have one?
                                Ok(())
                            }
                            // ===== External invalid data errors
                            ChunkIngressError::InvalidProof => Err(GossipError::InvalidData(
                                InvalidDataError::ChunkInvalidProof,
                            )),
                            ChunkIngressError::InvalidDataHash => Err(GossipError::InvalidData(
                                InvalidDataError::ChinkInvalidDataHash,
                            )),
                            ChunkIngressError::InvalidChunkSize => Err(GossipError::InvalidData(
                                InvalidDataError::ChunkInvalidChunkSize,
                            )),
                            // ===== Internal errors
                            ChunkIngressError::DatabaseError => {
                                Err(GossipError::Internal(InternalGossipError::Database))
                            }
                            ChunkIngressError::ServiceUninitialized => Err(GossipError::Internal(
                                InternalGossipError::ServiceUninitialized,
                            )),
                            ChunkIngressError::Other(other) => {
                                Err(GossipError::Internal(InternalGossipError::Unknown(other)))
                            }
                        }
                    }
                }
            }
            Err(error) => {
                tracing::error!("Failed to send transaction to mempool: {}", error);
                Err(GossipError::Internal(InternalGossipError::Unknown(
                    error.to_string(),
                )))
            }
        }
    }

    pub(crate) async fn handle_transaction(
        &self,
        tx: IrysTransactionHeader,
        source_address: SocketAddr,
    ) -> GossipResult<()> {
        match self.mempool.send(TxIngressMessage(tx.clone())).await {
            Ok(message_result) => {
                match message_result {
                    Ok(()) => {
                        // Success. Mempool will send the tx data to the internal mempool,
                        //  but we still need to update the cache with the source address.
                        self.cache
                            .record_seen(source_address, &GossipData::Transaction(tx))
                    }
                    Err(error) => {
                        match error {
                            // ==== Not really errors
                            TxIngressError::Skipped => {
                                // Not an invalid transaction - just skipped
                                self.cache
                                    .record_seen(source_address, &GossipData::Transaction(tx))
                            }
                            // ==== External errors
                            TxIngressError::InvalidSignature => {
                                // Invalid signature, decrease source reputation
                                Err(GossipError::InvalidData(
                                    InvalidDataError::TransactionSignature,
                                ))
                            }
                            TxIngressError::Unfunded => {
                                // Unfunded transaction, decrease source reputation
                                Err(GossipError::InvalidData(
                                    InvalidDataError::TransactionUnfunded,
                                ))
                            }
                            TxIngressError::InvalidAnchor => {
                                // Invalid anchor, decrease source reputation
                                Err(GossipError::InvalidData(
                                    InvalidDataError::TransactionAnchor,
                                ))
                            }
                            // ==== Internal errors - shouldn't be communicated to outside
                            TxIngressError::DatabaseError => {
                                Err(GossipError::Internal(InternalGossipError::Database))
                            }
                            TxIngressError::ServiceUninitialized => Err(GossipError::Internal(
                                InternalGossipError::ServiceUninitialized,
                            )),
                            TxIngressError::Other(error) => {
                                Err(GossipError::Internal(InternalGossipError::Unknown(error)))
                            }
                        }
                    }
                }
            }
            Err(error) => {
                tracing::error!("Failed to send transaction to mempool: {}", error);
                Err(GossipError::Internal(InternalGossipError::Unknown(
                    error.to_string(),
                )))
            }
        }
    }

    pub(crate) async fn handle_block_header(
        &self,
        irys_block_header: IrysBlockHeader,
        source_address: SocketAddr,
        source_api_address: SocketAddr,
    ) -> GossipResult<()> where {
        tracing::debug!(
            "Gossip block received from peer {}: {:?}",
            source_address,
            irys_block_header.block_hash
        );

        // Get all transaction IDs from the block
        let data_tx_ids = irys_block_header
            .data_ledgers
            .iter()
            .flat_map(|ledger| ledger.tx_ids.0.clone())
            .collect::<Vec<H256>>();

        let mut missing_tx_ids = Vec::new();

        for tx_id in irys_block_header
            .data_ledgers
            .iter()
            .flat_map(|ledger| ledger.tx_ids.0.clone())
        {
            if !self.is_known_tx(tx_id).await? {
                missing_tx_ids.push(tx_id);
            }
        }

        for system_tx_id in irys_block_header
            .system_ledgers
            .iter()
            .flat_map(|ledger| ledger.tx_ids.0.clone())
        {
            if !self.is_known_tx(system_tx_id).await? {
                missing_tx_ids.push(system_tx_id);
            }
        }

        // Fetch missing transactions from the source peer
        let missing_txs = self
            .api_client
            .get_transactions(source_api_address, &missing_tx_ids)
            .await
            .map_err(|error| {
                tracing::error!(
                    "Failed to fetch transactions from peer {}: {}",
                    source_api_address,
                    error
                );
                GossipError::unknown(&error)
            })?;

        // Process each transaction
        for (tx_id, tx) in data_tx_ids.iter().zip(missing_txs.iter()) {
            if let Some(tx) = tx {
                // Send transaction to mempool
                match self.mempool.send(TxIngressMessage(tx.clone())).await {
                    Ok(message_result) => {
                        match message_result {
                            Ok(()) => {
                                // Success. Record in cache
                                self.cache.record_seen(
                                    source_address,
                                    &GossipData::Transaction(tx.clone()),
                                )?;
                            }
                            Err(error) => {
                                match tx_ingress_error_to_gossip_error(error) {
                                    Some(GossipError::InvalidData(error)) => {
                                        // Invalid transaction, decrease source reputation
                                        return Err(GossipError::InvalidData(error));
                                    }
                                    Some(GossipError::Internal(error)) => {
                                        // Internal error - log it
                                        tracing::error!("Internal error: {:?}", error);
                                        return Err(GossipError::Internal(error));
                                    }
                                    Some(error) => {
                                        // Other error - log it
                                        tracing::error!("Unexpected error when handling gossip transaction: {:?}", error);
                                        return Err(error);
                                    }
                                    None => {
                                        // Not an invalid transaction - just skipped
                                        self.cache.record_seen(
                                            source_address,
                                            &GossipData::Transaction(tx.clone()),
                                        )?;
                                    }
                                }
                            }
                        }
                    }
                    Err(error) => {
                        tracing::error!("Failed to send transaction to mempool: {}", error);
                        return Err(GossipError::Internal(InternalGossipError::Unknown(
                            error.to_string(),
                        )));
                    }
                }
            } else {
                return Err(GossipError::InvalidData(InvalidDataError::InvalidBlock(
                    format!("Missing transaction {tx_id} in block from peer {source_address}"),
                )));
            }
        }

        // Record block in cache
        self.cache
            .record_seen(source_address, &GossipData::Block(irys_block_header))?;
        Ok(())
    }

    async fn is_known_tx(&self, tx_id: H256) -> Result<bool, GossipError> {
        self.mempool
            .send(TxExistenceQuery(tx_id))
            .await
            .map_err(|error| GossipError::unknown(&error))?
            .map_err(|error| {
                tx_ingress_error_to_gossip_error(error).unwrap_or_else(|| {
                    GossipError::unknown(
                        "Did not receive an error from mempool where an error was expected",
                    )
                })
            })
    }
}
