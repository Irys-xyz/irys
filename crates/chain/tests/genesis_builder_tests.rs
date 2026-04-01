use std::path::PathBuf;

use irys_chain::genesis_builder::{
    GenesisMinerEntry, GenesisMinerManifest, GenesisMinerManifestEntry, build_signed_genesis_block,
};
use irys_config::StorageSubmodulesConfig;
use irys_domain::EpochSnapshot;
use irys_types::{Config, IrysAddress, NodeConfig};
use k256::ecdsa::SigningKey;

const KEY_A: &str = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
const KEY_B: &str = "f57554aff54acd4cfaa084f45a7062d5869c8dbb789f7d6a883fade660960303";

fn make_manifest(entries: Vec<(&str, u64)>) -> GenesisMinerManifest {
    GenesisMinerManifest {
        miners: entries
            .into_iter()
            .map(|(key, pledge_count)| GenesisMinerManifestEntry {
                mining_key: key.to_string(),
                pledge_count,
            })
            .collect(),
    }
}

#[test]
fn into_entries_rejects_duplicate_mining_keys() {
    let manifest = make_manifest(vec![(KEY_A, 5), (KEY_A, 3)]);
    let result = manifest.into_entries();
    match result {
        Ok(_) => panic!("should reject duplicate mining keys"),
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            assert!(
                msg.contains("duplicate"),
                "error should mention 'duplicate', got: {msg}"
            );
        }
    }
}

#[test]
fn into_entries_rejects_zero_pledge_count() {
    let manifest = make_manifest(vec![(KEY_A, 0)]);
    let result = manifest.into_entries();
    match result {
        Ok(_) => panic!("should reject zero pledge_count"),
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            assert!(
                msg.contains("pledge_count"),
                "error should mention 'pledge_count', got: {msg}"
            );
        }
    }
}

#[test]
fn into_entries_accepts_valid_manifest() {
    let manifest = make_manifest(vec![(KEY_A, 5), (KEY_B, 3)]);
    let entries = manifest
        .into_entries()
        .expect("valid manifest should be accepted");
    assert_eq!(entries.len(), 2);
    // Both pledge counts should be present (order may vary due to canonical sorting)
    let mut counts: Vec<u64> = entries.iter().map(|e| e.pledge_count).collect();
    counts.sort();
    assert_eq!(counts, vec![3, 5]);
}

#[test]
fn into_entries_canonicalizes_order() {
    // Manifest with miners in A, B order
    let manifest_ab = make_manifest(vec![(KEY_A, 3), (KEY_B, 5)]);
    let entries_ab = manifest_ab.into_entries().expect("valid");

    // Manifest with miners in B, A order
    let manifest_ba = make_manifest(vec![(KEY_B, 5), (KEY_A, 3)]);
    let entries_ba = manifest_ba.into_entries().expect("valid");

    // Both should produce the same canonical order
    assert_eq!(entries_ab.len(), entries_ba.len());
    for (a, b) in entries_ab.iter().zip(entries_ba.iter()) {
        assert_eq!(
            a.signing_key.to_bytes(),
            b.signing_key.to_bytes(),
            "canonical sort should produce identical key order"
        );
        assert_eq!(a.pledge_count, b.pledge_count);
    }
}

fn test_config() -> Config {
    let node_config = NodeConfig::testing().with_consensus(|c| {
        // Ensure deterministic timestamp — testing() defaults to 0 which means "use now()"
        if c.genesis.timestamp_millis == 0 {
            c.genesis.timestamp_millis = 1_700_000_000_000;
        }
    });
    Config::new_with_random_peer_id(node_config)
}

fn test_miners() -> Vec<GenesisMinerEntry> {
    let key_a = SigningKey::from_slice(&hex::decode(KEY_A).unwrap()).unwrap();
    let key_b = SigningKey::from_slice(&hex::decode(KEY_B).unwrap()).unwrap();

    let mut entries = vec![
        GenesisMinerEntry {
            signing_key: key_a,
            pledge_count: 3,
        },
        GenesisMinerEntry {
            signing_key: key_b,
            pledge_count: 2,
        },
    ];
    // Sort by IrysAddress to match canonical order from into_entries()
    entries.sort_by_cached_key(|e| {
        use alloy_signer::utils::secret_key_to_address;
        IrysAddress::from(secret_key_to_address(&e.signing_key))
    });
    entries
}

/// Build a StorageSubmodulesConfig with enough paths for the total pledge count.
fn test_storage_submodules(total_pledges: usize) -> StorageSubmodulesConfig {
    StorageSubmodulesConfig {
        is_using_hardcoded_paths: true,
        submodule_paths: (0..total_pledges)
            .map(|i| PathBuf::from(format!("/tmp/test-sm-{i}")))
            .collect(),
    }
}

#[tokio::test]
async fn build_signed_genesis_block_is_deterministic() {
    let config = test_config();
    let miners = test_miners();

    let output_1 = build_signed_genesis_block(&config, &miners).await.unwrap();
    let output_2 = build_signed_genesis_block(&config, &miners).await.unwrap();

    // Block hashes must match
    assert_eq!(
        output_1.block.block_hash, output_2.block.block_hash,
        "genesis block hash must be deterministic"
    );

    // Commitment count must match
    assert_eq!(output_1.commitments.len(), output_2.commitments.len());

    // Every commitment ID must match in order
    for (c1, c2) in output_1.commitments.iter().zip(output_2.commitments.iter()) {
        assert_eq!(
            c1.id(),
            c2.id(),
            "commitment IDs must be identical and in the same order"
        );
    }
}

#[tokio::test]
async fn partition_assignments_are_deterministic() {
    let config = test_config();
    let miners = test_miners();
    let total_pledges: usize = miners.iter().map(|m| m.pledge_count as usize).sum();

    let output = build_signed_genesis_block(&config, &miners).await.unwrap();

    let submodules = test_storage_submodules(total_pledges);

    // Create two EpochSnapshots from the same genesis data
    let snap_1 = EpochSnapshot::new(
        &submodules,
        output.block.clone(),
        output.commitments.clone(),
        &config,
    );
    let snap_2 = EpochSnapshot::new(
        &submodules,
        output.block.clone(),
        output.commitments.clone(),
        &config,
    );

    // Extract partition assignments from both snapshots.
    // After genesis init, some pledged capacity partitions are moved to data partitions
    // via backfill_missing_partitions, so we must check both maps.
    let cap_1 = &snap_1.partition_assignments.capacity_partitions;
    let cap_2 = &snap_2.partition_assignments.capacity_partitions;
    let data_1 = &snap_1.partition_assignments.data_partitions;
    let data_2 = &snap_2.partition_assignments.data_partitions;

    // Capacity partition assignments must match
    assert_eq!(
        cap_1.len(),
        cap_2.len(),
        "capacity partition assignment count must match"
    );
    for ((hash_1, assign_1), (hash_2, assign_2)) in cap_1.iter().zip(cap_2.iter()) {
        assert_eq!(hash_1, hash_2, "capacity partition hashes must match");
        assert_eq!(
            assign_1.miner_address, assign_2.miner_address,
            "capacity miner assignments must match for partition {hash_1}"
        );
    }

    // Data partition assignments must match
    assert_eq!(
        data_1.len(),
        data_2.len(),
        "data partition assignment count must match"
    );
    for ((hash_1, assign_1), (hash_2, assign_2)) in data_1.iter().zip(data_2.iter()) {
        assert_eq!(hash_1, hash_2, "data partition hashes must match");
        assert_eq!(
            assign_1.miner_address, assign_2.miner_address,
            "data miner assignments must match for partition {hash_1}"
        );
    }

    // Verify we actually assigned partitions (not a vacuous pass).
    // Total assigned (capacity + data) must equal the total pledge count.
    let total_assigned = cap_1.len() + data_1.len();
    assert_eq!(
        total_assigned, total_pledges,
        "every pledge should have a partition assignment (capacity + data)"
    );
}
