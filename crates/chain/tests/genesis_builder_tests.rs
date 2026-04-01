use irys_chain::genesis_builder::{GenesisMinerManifest, GenesisMinerManifestEntry};

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
