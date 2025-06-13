use std::{collections::HashMap, path::Path};

use irys_types::{CommitmentTransaction, IrysTransactionHeader, H256};

pub struct RecoveredMempoolState {
    pub commitment_txs: HashMap<H256, CommitmentTransaction>,
    pub storage_txs: HashMap<H256, IrysTransactionHeader>,
}

impl RecoveredMempoolState {
    pub async fn load_from_disk(mempool_dir: &Path, remove_files: bool) -> Self {
        let commitment_tx_path = mempool_dir.join("commitment_tx");
        let storage_tx_path = mempool_dir.join("storage_tx");
        let mut commitment_txs = HashMap::new();
        let mut storage_txs = HashMap::new();

        // if the mempool directory does not exist, then return empty struct
        if !mempool_dir.exists() {
            return Self {
                commitment_txs,
                storage_txs,
            };
        }

        if commitment_tx_path.exists() {
            if let Ok(mut entries) = tokio::fs::read_dir(&commitment_tx_path).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }

                    match tokio::fs::read_to_string(&path).await {
                        Ok(json) => match serde_json::from_str::<CommitmentTransaction>(&json) {
                            Ok(tx) => {
                                commitment_txs.insert(tx.id, tx);
                            }
                            Err(_) => tracing::warn!("Failed to parse {:?}", path),
                        },
                        Err(_) => tracing::debug!("Failed to read {:?}", path),
                    }
                }
            }
        }

        if storage_tx_path.exists() {
            if let Ok(mut entries) = tokio::fs::read_dir(&storage_tx_path).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }

                    match tokio::fs::read_to_string(&path).await {
                        Ok(json) => match serde_json::from_str::<IrysTransactionHeader>(&json) {
                            Ok(tx) => {
                                storage_txs.insert(tx.id, tx);
                            }
                            Err(_) => tracing::warn!("Failed to parse {:?}", path),
                        },
                        Err(_) => tracing::debug!("Failed to read {:?}", path),
                    }
                }
            }
        }

        // Remove the mempool directory if requested
        if remove_files {
            if let Err(e) = tokio::fs::remove_dir_all(mempool_dir).await {
                tracing::warn!(
                    "Failed to remove mempool directory {:?}: {:?}",
                    mempool_dir,
                    e
                );
            }
        }

        Self {
            commitment_txs,
            storage_txs,
        }
    }
}
