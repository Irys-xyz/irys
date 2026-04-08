use std::path::PathBuf;

use irys_chain::genesis_builder::{
    GenesisMinerEntry, GenesisMinerManifest, GenesisMinerManifestEntry,
    build_genesis_block_from_commitments, build_signed_genesis_block,
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

    // Manifest with miners in reversed (B, A) order
    let manifest_rev = make_manifest(vec![(KEY_B, 5), (KEY_A, 3)]);
    let entries_rev = manifest_rev.into_entries().expect("valid");

    // Both should produce the same canonical order
    assert_eq!(entries_ab.len(), entries_rev.len());
    for (a, b) in entries_ab.iter().zip(entries_rev.iter()) {
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
        // Difficulty source for genesis builder is config-driven.
        c.genesis.initial_packed_partitions = Some(5.0);
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
    let snap_2 = EpochSnapshot::new(&submodules, output.block, output.commitments, &config);

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
            assign_1, assign_2,
            "capacity partition assignments must match for partition {hash_1}"
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
            assign_1, assign_2,
            "data partition assignments must match for partition {hash_1}"
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

#[tokio::test]
async fn build_genesis_from_existing_commitments_matches_generated() {
    let config = test_config();
    let miners = test_miners();

    // First, generate commitments the normal way
    let generated = build_signed_genesis_block(&config, &miners).await.unwrap();

    // Now use those commitments to build a genesis block from existing commitments.
    // Use the first miner's key as the block signer (same as build_signed_genesis_block).
    let from_existing = build_genesis_block_from_commitments(
        &config,
        generated.commitments.clone(),
        &miners[0].signing_key,
    )
    .unwrap();

    // Both should produce the same block hash (same commitments, same config, same signer)
    assert_eq!(
        generated.block.block_hash, from_existing.block.block_hash,
        "genesis from existing commitments should match generated genesis"
    );
    assert_eq!(generated.commitments.len(), from_existing.commitments.len());
}

#[test]
fn build_genesis_from_commitments_rejects_duplicate_txids() {
    let config = test_config();

    // Create a valid commitment, then duplicate it
    let rt = tokio::runtime::Runtime::new().unwrap();
    let generated = rt
        .block_on(build_signed_genesis_block(&config, &test_miners()))
        .unwrap();

    let mut duped = generated.commitments;
    // Duplicate the first commitment
    duped.push(duped[0].clone());

    let result =
        build_genesis_block_from_commitments(&config, duped, &test_miners()[0].signing_key);
    assert!(result.is_err(), "should reject duplicate commitment txids");
    let msg = result
        .err()
        .expect("expected Err")
        .to_string()
        .to_lowercase();
    assert!(
        msg.contains("duplicate"),
        "error should mention 'duplicate', got: {msg}"
    );
}

#[tokio::test]
async fn build_signed_genesis_rejects_non_canonical_miner_order() {
    let config = test_config();
    let mut miners = test_miners();

    // Reverse the canonical order
    miners.reverse();

    let result = build_signed_genesis_block(&config, &miners).await;
    assert!(
        result.is_err(),
        "should reject non-canonical miner ordering"
    );
    let msg = result
        .err()
        .expect("expected Err")
        .to_string()
        .to_lowercase();
    assert!(
        msg.contains("sorted"),
        "error should mention sorting, got: {msg}"
    );
}

#[test]
fn build_genesis_from_commitments_rejects_missing_stake() {
    let config = test_config();

    // Create valid commitments, then remove all stakes
    let rt = tokio::runtime::Runtime::new().unwrap();
    let generated = rt
        .block_on(build_signed_genesis_block(&config, &test_miners()))
        .unwrap();

    let pledges_only: Vec<_> = generated
        .commitments
        .into_iter()
        .filter(|c| {
            matches!(
                c.commitment_type(),
                irys_types::CommitmentTypeV2::Pledge { .. }
            )
        })
        .collect();

    let result =
        build_genesis_block_from_commitments(&config, pledges_only, &test_miners()[0].signing_key);
    assert!(result.is_err(), "should reject commitments without a stake");
    let msg = result
        .err()
        .expect("expected Err")
        .to_string()
        .to_lowercase();
    assert!(
        msg.contains("stake"),
        "error should mention 'stake', got: {msg}"
    );
}
