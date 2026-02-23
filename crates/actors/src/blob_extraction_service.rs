use irys_types::H256;
use reth::revm::primitives::B256;
use reth_transaction_pool::blobstore::{BlobStore, BlobStoreError};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, warn};

use crate::mempool_service::MempoolServiceMessage;

/// Messages sent to the blob extraction service.
#[derive(Debug)]
pub enum BlobExtractionMessage {
    /// Extract blobs from a locally-produced block containing EIP-4844 transactions.
    ExtractBlobs {
        block_hash: H256,
        blob_tx_hashes: Vec<B256>,
    },
}

/// Extracts EIP-4844 blob data from the Reth blob store after block production,
/// converts blobs into Irys chunks with IngressProofV2, and injects synthetic
/// data transactions into the mempool.
pub struct BlobExtractionService<S: BlobStore> {
    blob_store: S,
    mempool_sender: tokio::sync::mpsc::UnboundedSender<MempoolServiceMessage>,
    config: Arc<irys_types::Config>,
}

impl<S: BlobStore> BlobExtractionService<S> {
    pub fn spawn_service(
        blob_store: S,
        mempool_sender: tokio::sync::mpsc::UnboundedSender<MempoolServiceMessage>,
        config: Arc<irys_types::Config>,
        rx: UnboundedReceiver<BlobExtractionMessage>,
        runtime_handle: tokio::runtime::Handle,
    ) {
        let service = Self {
            blob_store,
            mempool_sender,
            config,
        };

        runtime_handle.spawn(service.start(rx));
    }

    async fn start(self, mut rx: UnboundedReceiver<BlobExtractionMessage>) {
        debug!("Blob extraction service started");
        while let Some(msg) = rx.recv().await {
            match msg {
                BlobExtractionMessage::ExtractBlobs {
                    block_hash,
                    blob_tx_hashes,
                } => {
                    if let Err(e) = self.handle_extract_blobs(block_hash, &blob_tx_hashes) {
                        warn!(
                            block.hash = %block_hash,
                            error = %e,
                            "Failed to extract blobs from block",
                        );
                    }
                }
            }
        }
        debug!("Blob extraction service stopped");
    }

    fn handle_extract_blobs(&self, block_hash: H256, blob_tx_hashes: &[B256]) -> eyre::Result<()> {
        use irys_types::ingress::generate_ingress_proof_v2_from_blob;

        let signer = self.config.irys_signer();
        let chain_id = self.config.consensus.chain_id;
        let anchor: H256 = block_hash;

        let mut total_blobs = 0_u64;

        for tx_hash in blob_tx_hashes {
            let sidecar_variant = match self.blob_store.get(*tx_hash) {
                Ok(Some(s)) => s,
                Ok(None) => {
                    warn!(tx.hash = %tx_hash, "Blob sidecar not found in store (may be pruned)");
                    continue;
                }
                Err(BlobStoreError::Other(e)) => {
                    warn!(tx.hash = %tx_hash, error = %e, "Blob store error");
                    continue;
                }
                Err(e) => {
                    warn!(tx.hash = %tx_hash, error = ?e, "Blob store error");
                    continue;
                }
            };

            let sidecar = match sidecar_variant.as_eip4844() {
                Some(s) => s,
                None => {
                    warn!(tx.hash = %tx_hash, "Sidecar is not EIP-4844 format, skipping");
                    continue;
                }
            };

            for (blob_idx, blob) in sidecar.blobs.iter().enumerate() {
                let commitment_bytes: &[u8; 48] = sidecar.commitments[blob_idx].as_ref();

                let proof = generate_ingress_proof_v2_from_blob(
                    &signer,
                    blob.as_ref(),
                    commitment_bytes,
                    chain_id,
                    anchor,
                )?;

                let data_root = proof.data_root();

                let tx_header = irys_types::transaction::DataTransactionHeader::V1(
                    irys_types::transaction::DataTransactionHeaderV1WithMetadata {
                        tx: irys_types::transaction::DataTransactionHeaderV1 {
                            id: H256::zero(),
                            anchor,
                            signer: signer.address(),
                            data_root,
                            data_size: blob.len() as u64,
                            header_size: 0,
                            term_fee: Default::default(),
                            perm_fee: None,
                            ledger_id: irys_types::block::DataLedger::Submit as u32,
                            chain_id,
                            signature: Default::default(),
                            bundle_format: None,
                        },
                        metadata: irys_types::transaction::DataTransactionMetadata::new(),
                    },
                );

                // Zero-pad blob to 256KB Irys chunk
                let mut chunk_data = vec![0_u8; irys_types::kzg::CHUNK_SIZE_FOR_KZG];
                chunk_data[..blob.len()].copy_from_slice(blob.as_ref());

                let _ = self
                    .mempool_sender
                    .send(MempoolServiceMessage::IngestBlobDerivedTx {
                        tx_header,
                        ingress_proof: proof,
                        chunk_data,
                    });

                total_blobs += 1;
            }
        }

        if total_blobs > 0 {
            debug!(
                block.hash = %block_hash,
                blobs.count = total_blobs,
                txs.count = blob_tx_hashes.len(),
                "Extracted blobs from block",
            );
        }

        Ok(())
    }
}
