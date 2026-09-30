use std::pin::pin;

use irys_domain::BlockTreeReadGuard;
use irys_types::ingress::generate_ingress_proof_v2_from_blob;
use irys_types::irys::IrysSigner;
use irys_types::kzg::{BLOB_SIZE, KzgCommitmentBytes, zero_pad_to_chunk_size};
use irys_types::{
    Base64, DataLedger, DataTransactionHeader, H256, IrysTransactionCommon as _, SendTraced as _,
    TokioServiceHandle, Traced, generate_data_root, generate_leaves_from_chunks, resolve_proofs,
};
use reth::revm::primitives::B256;
use reth_transaction_pool::blobstore::BlobStore;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, warn};

use crate::mempool_service::MempoolServiceMessage;
use crate::mempool_service::data_txs::publish_fees_for_size;

#[derive(Debug)]
pub enum BlobExtractionMessage {
    ExtractBlobs {
        block_hash: H256,
        blob_tx_hashes: Vec<B256>,
    },
}

/// Extracts EIP-4844 blob data from the Reth blob store after block production,
/// converts each blob into a signed Publish transaction with a real chunk path,
/// and injects that transaction into the mempool.
pub struct BlobExtractionService<S: BlobStore> {
    blob_store: S,
    mempool_sender: UnboundedSender<Traced<MempoolServiceMessage>>,
    config: irys_types::Config,
    block_tree: BlockTreeReadGuard,
}

struct PreparedBlob {
    proof: irys_types::ingress::IngressProof,
    padded: Vec<u8>,
    data_path: Base64,
}

impl<S: BlobStore> BlobExtractionService<S> {
    pub fn spawn_service(
        blob_store: S,
        mempool_sender: UnboundedSender<Traced<MempoolServiceMessage>>,
        config: irys_types::Config,
        block_tree: BlockTreeReadGuard,
        rx: UnboundedReceiver<BlobExtractionMessage>,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
        let service = Self {
            blob_store,
            mempool_sender,
            config,
            block_tree,
        };
        let handle = runtime_handle.spawn(service.start(rx, shutdown_rx));
        TokioServiceHandle {
            name: "blob_extraction_service".to_owned(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    async fn start(
        self,
        mut rx: UnboundedReceiver<BlobExtractionMessage>,
        shutdown: reth::tasks::shutdown::Shutdown,
    ) {
        debug!("Blob extraction service started");
        let mut shutdown_future = pin!(shutdown);
        loop {
            tokio::select! {
                biased;
                () = &mut shutdown_future => {
                    debug!("Blob extraction service shutting down");
                    break;
                }
                msg = rx.recv() => {
                    match msg {
                        Some(BlobExtractionMessage::ExtractBlobs {
                            block_hash,
                            blob_tx_hashes,
                        }) => {
                            if let Err(e) = self.handle_extract_blobs(block_hash, &blob_tx_hashes).await {
                                warn!(
                                    block.hash = %block_hash,
                                    error = %e,
                                    "Failed to extract blobs from block",
                                );
                            }
                        }
                        None => break,
                    }
                }
            }
        }
        debug!("Blob extraction service stopped");
    }

    async fn handle_extract_blobs(
        &self,
        block_hash: H256,
        blob_tx_hashes: &[B256],
    ) -> eyre::Result<()> {
        if !self.config.consensus.enable_blobs {
            warn!("Received blob extraction request but blobs are disabled");
            return Ok(());
        }

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

            if sidecar.commitments.len() != sidecar.blobs.len() {
                warn!(
                    tx.hash = %tx_hash,
                    commitments = sidecar.commitments.len(),
                    blobs = sidecar.blobs.len(),
                    "Skipping blob sidecar whose commitment count does not match its blobs"
                );
                continue;
            }
            for (blob, commitment) in sidecar.blobs.iter().zip(sidecar.commitments.iter()) {
                if let Err(e) = self
                    .process_single_blob(
                        &signer,
                        blob.as_ref(),
                        commitment.as_ref(),
                        chain_id,
                        anchor,
                    )
                    .await
                {
                    warn!(
                        tx.hash = %tx_hash,
                        error = %e,
                        "Skipping blob that could not be turned into a Publish transaction"
                    );
                    continue;
                }
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

    async fn process_single_blob(
        &self,
        signer: &IrysSigner,
        blob_data: &[u8],
        commitment_bytes: &[u8; 48],
        chain_id: u64,
        anchor: H256,
    ) -> eyre::Result<()> {
        // clone: the blocking proof task needs an owned signer; this task keeps the original
        let signer_for_proof = signer.clone();
        // clone: spawn_blocking requires owned blob bytes
        let blob_owned = blob_data.to_vec();
        let commitment = *commitment_bytes;
        let prepared = tokio::task::spawn_blocking(move || {
            prepare_blob_ingress(signer_for_proof, blob_owned, commitment, chain_id, anchor)
        })
        .await
        .map_err(|e| eyre::eyre!("blob proof task failed: {e}"))??;

        let data_size = u64::try_from(prepared.padded.len())
            .map_err(|_| eyre::eyre!("chunk size overflows u64"))?;
        let (term_fee, perm_fee) = publish_fees_for_size(&self.config, &self.block_tree, data_size)
            .map_err(|e| eyre::eyre!("blob publish fees: {e}"))?;
        let data_root = prepared.proof.data_root();
        let unsigned = DataTransactionHeader::V1(
            irys_types::transaction::DataTransactionHeaderV1WithMetadata {
                tx: irys_types::transaction::DataTransactionHeaderV1 {
                    id: H256::zero(),
                    anchor,
                    signer: signer.address(),
                    data_root,
                    data_size,
                    prefix_size: 0,
                    prefix_hash: H256::zero(),
                    term_fee,
                    perm_fee: Some(perm_fee),
                    ledger_id: u32::from(DataLedger::Publish),
                    chain_id,
                    signature: Default::default(),
                    metadata_format: 0,
                },
                metadata: irys_types::transaction::DataTransactionMetadata::new(),
            },
        );
        let tx_header = unsigned.sign(signer)?;

        let per_chunk_commitments = vec![(0_u32, KzgCommitmentBytes::from(commitment))];
        if let Err(e) =
            self.mempool_sender
                .send_traced(MempoolServiceMessage::IngestBlobDerivedTx {
                    tx_header,
                    ingress_proof: prepared.proof,
                    chunk_data: prepared.padded,
                    data_path: prepared.data_path,
                    per_chunk_commitments,
                })
        {
            warn!(data_root = %data_root, error = %e, "Failed to send blob-derived tx to mempool");
        }

        Ok(())
    }
}

fn prepare_blob_ingress(
    signer: IrysSigner,
    blob_data: Vec<u8>,
    commitment: [u8; 48],
    chain_id: u64,
    anchor: H256,
) -> eyre::Result<PreparedBlob> {
    let proof =
        generate_ingress_proof_v2_from_blob(&signer, &blob_data, &commitment, chain_id, anchor)?;
    if blob_data.len() > BLOB_SIZE {
        return Err(eyre::eyre!(
            "blob data exceeds one EIP-4844 blob: {} > {BLOB_SIZE}",
            blob_data.len()
        ));
    }
    let mut blob = vec![0_u8; BLOB_SIZE];
    blob[..blob_data.len()].copy_from_slice(&blob_data);
    let padded = zero_pad_to_chunk_size(&blob)?;
    // clone: the merkle builder takes ownership of the chunk and the mempool stores the same bytes
    let leaves = generate_leaves_from_chunks(std::iter::once(Ok(padded.clone())))?;
    let root = generate_data_root(leaves)?;
    if H256(root.id) != proof.data_root() {
        return Err(eyre::eyre!(
            "padded blob does not hash to the ingress proof data root"
        ));
    }
    let mut proofs = resolve_proofs(root, None)?;
    if proofs.len() != 1 {
        return Err(eyre::eyre!(
            "blob transaction must have exactly one chunk path, got {}",
            proofs.len()
        ));
    }
    let chunk_proof = proofs
        .pop()
        .ok_or_else(|| eyre::eyre!("blob merkle path is empty"))?;
    Ok(PreparedBlob {
        proof,
        padded,
        data_path: Base64(chunk_proof.proof),
    })
}

#[cfg(test)]
mod tests {
    use super::prepare_blob_ingress;
    use crate::data_tx_validation::data_tx_structural_defect;
    use crate::mempool_service::validate_tx_signature;
    use irys_types::kzg::{
        BLOB_SIZE, commitment_to_bytes, compute_blob_commitment, default_kzg_settings,
    };
    use irys_types::{
        BoundedFee, ConsensusConfig, DataLedger, DataTransactionHeader, H256,
        IrysTransactionCommon as _, TxChunkOffset, expected_chunk_byte_range, irys::IrysSigner,
        validate_path, validate_path_byte_range,
    };

    #[test]
    fn prepared_blob_publish_tx_passes_ingress_precheck_and_chunk_path() {
        let consensus = ConsensusConfig::testing();
        let signer = IrysSigner::random_signer(&consensus);
        let blob_data = b"blob-sidecar".to_vec();
        let mut blob = [0_u8; BLOB_SIZE];
        blob[..blob_data.len()].copy_from_slice(&blob_data);
        let commitment = commitment_to_bytes(
            &compute_blob_commitment(&blob, default_kzg_settings()).expect("blob commitment"),
        )
        .expect("commitment bytes");
        let anchor = H256::repeat_byte(9);
        // clone: the proof signer is consumed; the publish header is signed separately
        let prepared = prepare_blob_ingress(
            signer.clone(),
            blob_data,
            commitment,
            consensus.chain_id,
            anchor,
        )
        .expect("blob ingress prepares");

        assert!(!prepared.data_path.0.is_empty());
        let data_size = u64::try_from(prepared.padded.len()).expect("padded length fits u64");
        assert_eq!(data_size, ConsensusConfig::CHUNK_SIZE);

        let data_root = prepared.proof.data_root();
        let unsigned = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: irys_types::DataTransactionHeaderV1 {
                id: H256::zero(),
                anchor,
                signer: signer.address(),
                data_root,
                data_size,
                prefix_size: 0,
                prefix_hash: H256::zero(),
                term_fee: BoundedFee::zero(),
                perm_fee: Some(BoundedFee::zero()),
                ledger_id: u32::from(DataLedger::Publish),
                chain_id: consensus.chain_id,
                signature: Default::default(),
                metadata_format: 0,
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });
        let tx_header = unsigned.sign(&signer).expect("publish header signs");

        assert_eq!(
            DataLedger::try_from(tx_header.ledger_id).expect("known ledger"),
            DataLedger::Publish
        );
        assert!(
            DataLedger::try_from(tx_header.ledger_id)
                .expect("known ledger")
                .is_user_targetable()
        );
        assert_eq!(tx_header.data_root, data_root);
        assert_eq!(tx_header.data_size, data_size);
        assert!(
            data_tx_structural_defect(
                &tx_header,
                consensus.chain_id,
                ConsensusConfig::CHUNK_SIZE,
                consensus.mempool.max_data_tx_chunks,
            )
            .is_none()
        );
        validate_tx_signature(&tx_header).expect("precheck accepts the signature");

        let (min_byte, max_byte) =
            expected_chunk_byte_range(TxChunkOffset(0), data_size, ConsensusConfig::CHUNK_SIZE)
                .expect("chunk range");
        let target_offset = u128::from(max_byte - 1);
        let path = validate_path(data_root.0, &prepared.data_path, target_offset)
            .expect("chunk path validates");
        validate_path_byte_range(&path, (min_byte, max_byte)).expect("path covers the chunk");
    }
}
