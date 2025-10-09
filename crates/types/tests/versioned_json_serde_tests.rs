use irys_types::{
    CommitmentTransaction, ConsensusConfig, DataTransactionHeader, IrysBlockHeader,
    VersionDiscriminant as _, H256,
};

#[test]
fn test_versioned_commitment_transaction_json_no_duplicate_type_field() {
    let config = ConsensusConfig::testing();
    let mut commitment_tx = CommitmentTransaction::new_stake(&config, H256::zero());
    commitment_tx.id = H256::from([1_u8; 32]);

    // Serialize to JSON
    let json = serde_json::to_string(&commitment_tx).unwrap();

    // Check that there's no duplicate 'type' field
    // The JSON should only have one "type" field (from commitmentType)
    let type_count = json.matches("\"type\"").count();
    assert_eq!(
        type_count, 1,
        "Should have exactly one 'type' field (from commitmentType), found {} in: {}",
        type_count, json
    );

    // Verify it deserializes correctly
    let deserialized: CommitmentTransaction =
        serde_json::from_str(&json).expect("Failed to deserialize commitment transaction");
    assert_eq!(commitment_tx.id, deserialized.id);
    assert_eq!(commitment_tx.discriminant(), deserialized.discriminant());
}

#[test]
fn test_versioned_data_transaction_header_json_no_duplicate_type_field() {
    let config = ConsensusConfig::testing();
    let mut data_tx_header = DataTransactionHeader::new(&config);
    data_tx_header.id = H256::from([2_u8; 32]);

    // Serialize to JSON
    let json = serde_json::to_string(&data_tx_header).unwrap();

    // Check that there's no 'type' field at all (data transactions don't have one)
    let type_count = json.matches("\"type\"").count();
    assert_eq!(
        type_count, 0,
        "Should have no 'type' field in data transaction header, found {} in: {}",
        type_count, json
    );

    // Verify it deserializes correctly
    let deserialized: DataTransactionHeader =
        serde_json::from_str(&json).expect("Failed to deserialize data transaction header");
    assert_eq!(data_tx_header.id, deserialized.id);
    assert_eq!(data_tx_header.discriminant(), deserialized.discriminant());
}

#[test]
fn test_versioned_block_header_json_roundtrip() {
    let versioned = IrysBlockHeader::new_mock_header();

    // Serialize to JSON
    let json = serde_json::to_string(&versioned).unwrap();

    // Verify it deserializes correctly
    let deserialized: IrysBlockHeader =
        serde_json::from_str(&json).expect("Failed to deserialize block header");
    assert_eq!(versioned.version, deserialized.version);
    assert_eq!(versioned.block_hash, deserialized.block_hash);
}

#[test]
fn test_commitment_transaction_json_structure() {
    let config = ConsensusConfig::testing();
    let mut commitment_tx = CommitmentTransaction::new_stake(&config, H256::zero());
    commitment_tx.id = H256::from([1_u8; 32]);

    // Serialize to pretty JSON for inspection
    // note: we use `NumericVersionWrapper` so version is serialized as a number, not a string.
    let json = serde_json::to_string_pretty(&irys_types::NumericVersionWrapper::new(
        commitment_tx.clone(),
    ))
    .unwrap();
    println!("CommitmentTransaction JSON:\n{}", json);

    // Parse as a generic JSON value
    let json_value: serde_json::Value =
        serde_json::from_str(&json).expect("Failed to parse as JSON");

    // Verify structure
    assert!(json_value.is_object(), "Should be a JSON object");

    let obj = json_value.as_object().unwrap();

    // Should have version field from the inner struct
    assert!(obj.contains_key("version"), "Should have 'version' field");

    // Should have commitmentType with nested type
    assert!(
        obj.contains_key("commitmentType"),
        "Should have 'commitmentType' field"
    );

    // Version should be 1
    assert_eq!(
        obj.get("version").and_then(serde_json::Value::as_u64),
        Some(1),
        "Version should be 1"
    );
}
