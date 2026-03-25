use std::{collections::HashMap, path::Path};

use irys_types::{CommitmentTransaction, DataTransactionHeader, H256};

pub struct RecoveredMempoolState {
    pub commitment_txs: HashMap<H256, CommitmentTransaction>,
    pub storage_txs: HashMap<H256, DataTransactionHeader>,
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

        if commitment_tx_path.exists()
            && let Ok(mut entries) = tokio::fs::read_dir(&commitment_tx_path).await
        {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }

                match tokio::fs::read_to_string(&path).await {
                    Ok(json) => match serde_json::from_str::<CommitmentTransaction>(&json) {
                        Ok(tx) => {
                            commitment_txs.insert(tx.id(), tx);
                        }
                        Err(_) => tracing::warn!("Failed to parse {:?}", path),
                    },
                    Err(_) => tracing::debug!("Failed to read {:?}", path),
                }
            }
        }

        if storage_tx_path.exists()
            && let Ok(mut entries) = tokio::fs::read_dir(&storage_tx_path).await
        {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }

                match tokio::fs::read_to_string(&path).await {
                    Ok(json) => match serde_json::from_str::<DataTransactionHeader>(&json) {
                        Ok(tx) => {
                            storage_txs.insert(tx.id, tx);
                        }
                        Err(_) => tracing::warn!("Failed to parse {:?}", path),
                    },
                    Err(_) => tracing::debug!("Failed to read {:?}", path),
                }
            }
        }

        // Remove the mempool directory if requested
        if remove_files && let Err(e) = tokio::fs::remove_dir_all(mempool_dir).await {
            tracing::warn!(
                "Failed to remove mempool directory {:?}: {:?}",
                mempool_dir,
                e
            );
        }

        Self {
            commitment_txs,
            storage_txs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_testing_utils::utils::TempDirBuilder;

    #[tokio::test]
    async fn test_load_from_nonexistent_dir() {
        let dir = TempDirBuilder::new().build();
        let nonexistent = dir.path().join("does_not_exist");

        let state = RecoveredMempoolState::load_from_disk(&nonexistent, false).await;

        assert!(state.commitment_txs.is_empty());
        assert!(state.storage_txs.is_empty());
    }

    #[tokio::test]
    async fn test_load_valid_transactions() {
        let dir = TempDirBuilder::new().build();
        let mempool_dir = dir.path().join("mempool");

        let commitment_dir = mempool_dir.join("commitment_tx");
        let storage_dir = mempool_dir.join("storage_tx");
        tokio::fs::create_dir_all(&commitment_dir).await.unwrap();
        tokio::fs::create_dir_all(&storage_dir).await.unwrap();

        let mut commitment_tx = CommitmentTransaction::default();
        let commitment_id = H256::from_low_u64_be(42);
        commitment_tx.set_id(commitment_id);
        let commitment_json = serde_json::to_string(&commitment_tx).unwrap();
        tokio::fs::write(commitment_dir.join("tx1.json"), &commitment_json)
            .await
            .unwrap();

        let mut storage_tx = DataTransactionHeader::default();
        let storage_id = H256::from_low_u64_be(99);
        storage_tx.id = storage_id;
        let storage_json = serde_json::to_string(&storage_tx).unwrap();
        tokio::fs::write(storage_dir.join("tx2.json"), &storage_json)
            .await
            .unwrap();

        let state = RecoveredMempoolState::load_from_disk(&mempool_dir, false).await;

        assert_eq!(state.commitment_txs.len(), 1);
        assert!(state.commitment_txs.contains_key(&commitment_id));
        assert_eq!(state.commitment_txs[&commitment_id].id(), commitment_id);

        assert_eq!(state.storage_txs.len(), 1);
        assert!(state.storage_txs.contains_key(&storage_id));
        assert_eq!(state.storage_txs[&storage_id].id, storage_id);
    }

    #[tokio::test]
    async fn test_skips_non_json_files() {
        let dir = TempDirBuilder::new().build();
        let mempool_dir = dir.path().join("mempool");

        let commitment_dir = mempool_dir.join("commitment_tx");
        let storage_dir = mempool_dir.join("storage_tx");
        tokio::fs::create_dir_all(&commitment_dir).await.unwrap();
        tokio::fs::create_dir_all(&storage_dir).await.unwrap();

        let mut commitment_tx = CommitmentTransaction::default();
        let commitment_id = H256::from_low_u64_be(7);
        commitment_tx.set_id(commitment_id);
        let commitment_json = serde_json::to_string(&commitment_tx).unwrap();

        tokio::fs::write(commitment_dir.join("valid.json"), &commitment_json)
            .await
            .unwrap();
        tokio::fs::write(commitment_dir.join("ignored.txt"), &commitment_json)
            .await
            .unwrap();
        tokio::fs::write(commitment_dir.join("ignored.log"), &commitment_json)
            .await
            .unwrap();
        tokio::fs::write(commitment_dir.join("no_extension"), &commitment_json)
            .await
            .unwrap();

        let mut storage_tx = DataTransactionHeader::default();
        let storage_id = H256::from_low_u64_be(8);
        storage_tx.id = storage_id;
        let storage_json = serde_json::to_string(&storage_tx).unwrap();

        tokio::fs::write(storage_dir.join("valid.json"), &storage_json)
            .await
            .unwrap();
        tokio::fs::write(storage_dir.join("readme.md"), &storage_json)
            .await
            .unwrap();

        let state = RecoveredMempoolState::load_from_disk(&mempool_dir, false).await;

        assert_eq!(state.commitment_txs.len(), 1);
        assert!(state.commitment_txs.contains_key(&commitment_id));

        assert_eq!(state.storage_txs.len(), 1);
        assert!(state.storage_txs.contains_key(&storage_id));
    }

    #[test_log::test(tokio::test)]
    async fn test_handles_malformed_json() {
        let dir = TempDirBuilder::new().build();
        let mempool_dir = dir.path().join("mempool");

        let commitment_dir = mempool_dir.join("commitment_tx");
        let storage_dir = mempool_dir.join("storage_tx");
        tokio::fs::create_dir_all(&commitment_dir).await.unwrap();
        tokio::fs::create_dir_all(&storage_dir).await.unwrap();

        tokio::fs::write(commitment_dir.join("bad1.json"), "not valid json at all")
            .await
            .unwrap();
        tokio::fs::write(commitment_dir.join("bad2.json"), "{}")
            .await
            .unwrap();
        tokio::fs::write(commitment_dir.join("bad3.json"), "[]")
            .await
            .unwrap();

        tokio::fs::write(storage_dir.join("bad1.json"), "{\"wrong\": \"schema\"}")
            .await
            .unwrap();
        tokio::fs::write(storage_dir.join("bad2.json"), "")
            .await
            .unwrap();

        let state = RecoveredMempoolState::load_from_disk(&mempool_dir, false).await;

        assert!(state.commitment_txs.is_empty());
        assert!(state.storage_txs.is_empty());
    }

    #[tokio::test]
    async fn test_empty_subdirs_returns_empty() {
        let dir = TempDirBuilder::new().build();
        let mempool_dir = dir.path().join("mempool");

        tokio::fs::create_dir_all(mempool_dir.join("commitment_tx"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(mempool_dir.join("storage_tx"))
            .await
            .unwrap();

        let state = RecoveredMempoolState::load_from_disk(&mempool_dir, false).await;

        assert!(state.commitment_txs.is_empty());
        assert!(state.storage_txs.is_empty());
    }

    #[test_log::test(tokio::test)]
    async fn test_remove_files_true_removes_mempool_dir() {
        let dir = TempDirBuilder::new().build();
        let mempool_dir = dir.path().join("mempool");
        tokio::fs::create_dir_all(mempool_dir.join("commitment_tx"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(mempool_dir.join("storage_tx"))
            .await
            .unwrap();

        let _state = RecoveredMempoolState::load_from_disk(&mempool_dir, true).await;
        assert!(!mempool_dir.exists());
    }

    #[test_log::test(tokio::test)]
    async fn test_remove_files_loads_data_then_removes() {
        let dir = TempDirBuilder::new().build();
        let mempool_dir = dir.path().join("mempool");

        let commitment_dir = mempool_dir.join("commitment_tx");
        tokio::fs::create_dir_all(&commitment_dir).await.unwrap();
        tokio::fs::create_dir_all(mempool_dir.join("storage_tx"))
            .await
            .unwrap();

        let mut tx = CommitmentTransaction::default();
        let tx_id = H256::from_low_u64_be(42);
        tx.set_id(tx_id);
        tokio::fs::write(
            commitment_dir.join("tx.json"),
            serde_json::to_string(&tx).unwrap(),
        )
        .await
        .unwrap();

        let state = RecoveredMempoolState::load_from_disk(&mempool_dir, true).await;

        assert_eq!(state.commitment_txs.len(), 1);
        assert!(state.commitment_txs.contains_key(&tx_id));
        assert!(!mempool_dir.exists());
    }
}
