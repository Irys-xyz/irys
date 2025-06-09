use std::{collections::HashMap, path::Path};

use irys_types::{CommitmentTransaction, IrysTransactionHeader, H256};
use tracing::debug;

pub struct RecoveredMempoolState {
    pub commitment_txs: HashMap<H256, CommitmentTransaction>,
    pub storage_txs: HashMap<H256, IrysTransactionHeader>,
}

impl RecoveredMempoolState {
    pub async fn load_from_disk(mempool_dir: &Path) -> Self {
        let commitment_tx_path = mempool_dir.join("commitment_tx");
        let storage_tx_path = mempool_dir.join("storage_tx");
        let mut commitment_txs = HashMap::new();
        let mut storage_txs = HashMap::new();

        if commitment_tx_path.exists() {
            if let Ok(mut entries) = tokio::fs::read_dir(&commitment_tx_path).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }

                    let Ok(json) = tokio::fs::read_to_string(&path).await else {
                        debug!("Failed to read {:?}", path);
                        continue;
                    };

                    let Ok(tx) = serde_json::from_str::<CommitmentTransaction>(&json) else {
                        debug!("Failed to parse {:?}", path);
                        continue;
                    };

                    commitment_txs.insert(tx.id, tx);
                    let _ = tokio::fs::remove_file(&path).await;
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

                    let Ok(json) = tokio::fs::read_to_string(&path).await else {
                        debug!("Failed to read {:?}", path);
                        continue;
                    };

                    let Ok(tx) = serde_json::from_str::<IrysTransactionHeader>(&json) else {
                        debug!("Failed to parse {:?}", path);
                        continue;
                    };

                    storage_txs.insert(tx.id, tx);
                    let _ = tokio::fs::remove_file(&path).await;
                }
            }
        }

        Self {
            commitment_txs,
            storage_txs,
        }
    }
}
