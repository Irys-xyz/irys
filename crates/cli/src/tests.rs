use crate::cli_args::{
    Commands, IrysCli, RollbackMode, RollbackTarget, parse_rollback_target,
    timestamp_millis_to_secs,
};
use crate::snapshot_output::{
    PartitionAssignmentKindOutput, PartitionAssignmentOutput, SnapshotComparisonOutput,
    SnapshotPartitionState,
};
use clap::Parser as _;
use irys_types::H256;
use rstest::rstest;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[rstest]
#[case(&["irys-cli", "rollback-blocks", "to-block", "42"], "42")]
#[case(&["irys-cli", "rollback-blocks", "to-block", "0"], "0")]
#[case(&["irys-cli", "rollback-blocks", "to-block", "18446744073709551615"], "18446744073709551615")]
fn test_rollback_to_block_parsing(#[case] args: &[&str], #[case] expected_target: &str) {
    let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
    match cli.command {
        Commands::RollbackBlocks {
            mode: RollbackMode::ToBlock { target },
        } => {
            assert_eq!(target, expected_target);
        }
        other => panic!("expected RollbackBlocks(ToBlock), got {:?}", other),
    }
}

#[rstest]
#[case(&["irys-cli", "rollback-blocks", "count", "5"], 5)]
#[case(&["irys-cli", "rollback-blocks", "count", "0"], 0)]
#[case(&["irys-cli", "rollback-blocks", "count", "1000000"], 1_000_000)]
fn test_rollback_count_parsing(#[case] args: &[&str], #[case] expected_count: u64) {
    let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
    match cli.command {
        Commands::RollbackBlocks {
            mode: RollbackMode::Count { count },
        } => {
            assert_eq!(count, expected_count);
        }
        other => panic!("expected RollbackBlocks(Count), got {:?}", other),
    }
}

#[rstest]
#[case(&["irys-cli", "dump-state"])]
fn test_dump_state_parsing(#[case] args: &[&str]) {
    let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
    assert!(
        matches!(cli.command, Commands::DumpState { .. }),
        "expected DumpState, got {:?}",
        cli.command
    );
}

#[rstest]
#[case(&["irys-cli", "init-state", "/tmp/state.json"], "/tmp/state.json")]
#[case(&["irys-cli", "init-state", "relative/path"], "relative/path")]
fn test_init_state_parsing(#[case] args: &[&str], #[case] expected_path: &str) {
    let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
    match cli.command {
        Commands::InitState { state_path } => {
            assert_eq!(state_path, PathBuf::from(expected_path));
        }
        other => panic!("expected InitState, got {:?}", other),
    }
}

#[rstest]
#[case(
        &["irys-cli", "tui", "http://localhost:19080"],
        vec!["http://localhost:19080"],
        None,
        false
    )]
#[case(
        &["irys-cli", "tui", "http://a:1", "http://b:2"],
        vec!["http://a:1", "http://b:2"],
        None,
        false
    )]
#[case(
        &["irys-cli", "tui", "--config", "tui.toml"],
        vec![],
        Some("tui.toml"),
        false
    )]
#[case(
        &["irys-cli", "tui", "--record", "http://localhost:19080"],
        vec!["http://localhost:19080"],
        None,
        true
    )]
fn test_tui_parsing(
    #[case] args: &[&str],
    #[case] expected_urls: Vec<&str>,
    #[case] expected_config: Option<&str>,
    #[case] expected_record: bool,
) {
    let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
    match cli.command {
        Commands::Tui {
            node_urls,
            config,
            record,
        } => {
            let expected: Vec<String> = expected_urls.into_iter().map(String::from).collect();
            assert_eq!(node_urls, expected);
            assert_eq!(config.as_deref(), expected_config);
            assert_eq!(record, expected_record);
        }
        other => panic!("expected Tui, got {:?}", other),
    }
}

#[rstest]
#[case(&["irys-cli"])]
#[case(&["irys-cli", "nonexistent-command"])]
#[case(&["irys-cli", "rollback-blocks"])]
#[case(&["irys-cli", "rollback-blocks", "count"])]
#[case(&["irys-cli", "init-state"])]
fn test_cli_rejects_invalid_args(#[case] args: &[&str]) {
    assert!(
        IrysCli::try_parse_from(args).is_err(),
        "expected parse failure for {:?}",
        args
    );
}

#[rstest]
#[case("0", RollbackTarget::Height(0))]
#[case("42", RollbackTarget::Height(42))]
#[case("18446744073709551615", RollbackTarget::Height(u64::MAX))]
fn test_parse_rollback_target_height(#[case] input: &str, #[case] expected: RollbackTarget) {
    let result = parse_rollback_target(input).expect("should parse as height");
    assert_eq!(result, expected);
}

#[test]
fn test_parse_rollback_target_hash() {
    let hash = H256::random();
    let encoded = hash.to_string();
    let result = parse_rollback_target(&encoded).expect("should parse as hash");
    assert_eq!(result, RollbackTarget::Hash(hash));
}

#[rstest]
#[case("not-a-number-or-hash")]
#[case("0x1234")]
#[case("-1")]
#[case("hello world with spaces")]
fn test_parse_rollback_target_invalid(#[case] input: &str) {
    assert!(
        parse_rollback_target(input).is_err(),
        "expected error for {:?}",
        input
    );
}

#[rstest]
#[case(0_u128, 0)]
#[case(1000, 1)]
#[case(1500, 1)]
#[case(999, 0)]
#[case(60_000, 60)]
#[case(1_763_749_823_171, 1_763_749_823)]
fn test_timestamp_millis_to_secs(#[case] millis: u128, #[case] expected_secs: u64) {
    let result = timestamp_millis_to_secs(millis).expect("should convert");
    assert_eq!(result, expected_secs);
}

#[test]
fn test_timestamp_millis_to_secs_overflow() {
    let overflow_value: u128 = (u128::from(u64::MAX) + 1) * 1000;
    assert!(
        timestamp_millis_to_secs(overflow_value).is_err(),
        "should reject values whose seconds overflow u64"
    );
}

#[test]
fn test_timestamp_millis_to_secs_max_valid_millis() {
    let result = timestamp_millis_to_secs(u128::from(u64::MAX) * 1000 + 999)
        .expect("max valid millis should convert");
    assert_eq!(result, u64::MAX);
}

#[rstest]
#[case(
        &["irys-cli", "generate-miner-info", "--key", "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0"],
        "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0"
    )]
fn test_generate_miner_info_parsing(#[case] args: &[&str], #[case] expected_key: &str) {
    let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
    match cli.command {
        Commands::GenerateMinerInfo { key, key_file } => {
            assert_eq!(key.as_deref(), Some(expected_key));
            assert!(key_file.is_none());
        }
        other => panic!("expected GenerateMinerInfo, got {:?}", other),
    }
}

#[test]
fn test_build_genesis_rejects_conflicting_args() {
    // --miners and --commitments are mutually exclusive
    let args = &[
        "irys-cli",
        "build-genesis",
        "--miners",
        "manifest.toml",
        "--commitments",
        "commitments.json",
    ];
    assert!(
        IrysCli::try_parse_from(args).is_err(),
        "should reject --miners and --commitments together"
    );
}

#[test]
fn test_build_genesis_no_input_is_rejected() {
    // build-genesis with no --miners or --commitments is now rejected at parse time
    // by the required ArgGroup, so no runtime fallback is needed.
    let args = &["irys-cli", "build-genesis"];
    assert!(
        IrysCli::try_parse_from(args).is_err(),
        "should reject build-genesis with neither --miners nor --commitments"
    );
}

#[test]
fn test_generate_miner_info_address_derivation() {
    use alloy_signer::utils::secret_key_to_address;
    use irys_types::IrysAddress;
    use k256::ecdsa::SigningKey;

    let key_hex = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
    let key_bytes = hex::decode(key_hex).unwrap();
    let signing_key = SigningKey::from_slice(&key_bytes).unwrap();

    let evm_address = secret_key_to_address(&signing_key);
    let irys_address = IrysAddress::from(evm_address);

    // Pin the exact EVM checksum address for this key.
    let evm_str = format!("{evm_address}");
    assert_eq!(evm_str, "0x64f1a2829e0E698c18E7792D6E74f67d89AA0a32");

    // Pin the exact Irys (base58) address for this key.
    let irys_str = format!("{irys_address}");
    assert_eq!(irys_str, "2QZrWyPPi4XukwiJQrVmUvuPQ57F");
}

#[rstest]
#[case(&["irys-cli", "configured-miner-info"])]
fn test_configured_miner_info_parsing(#[case] args: &[&str]) {
    let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
    assert!(
        matches!(cli.command, Commands::ConfiguredMinerInfo { .. }),
        "expected ConfiguredMinerInfo, got {:?}",
        cli.command
    );
}

#[rstest]
#[case(&["irys-cli", "inspect-genesis", "--genesis-dir", "/tmp/genesis"])]
fn test_inspect_genesis_parsing(#[case] args: &[&str]) {
    let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
    match cli.command {
        Commands::InspectGenesis { genesis_dir } => {
            assert_eq!(genesis_dir, PathBuf::from("/tmp/genesis"));
        }
        other => panic!("expected InspectGenesis, got {:?}", other),
    }
}

#[rstest]
#[case(&["irys-cli", "import-genesis", "--genesis-dir", "/tmp/genesis"])]
fn test_import_genesis_parsing(#[case] args: &[&str]) {
    let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
    match cli.command {
        Commands::ImportGenesis { genesis_dir } => {
            assert_eq!(genesis_dir, PathBuf::from("/tmp/genesis"));
        }
        other => panic!("expected ImportGenesis, got {:?}", other),
    }
}

#[test]
fn test_import_genesis_default_dir() {
    let args = &["irys-cli", "import-genesis"];
    let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
    match cli.command {
        Commands::ImportGenesis { genesis_dir } => {
            assert_eq!(genesis_dir, PathBuf::from("."));
        }
        other => panic!("expected ImportGenesis, got {:?}", other),
    }
}

#[test]
fn test_inspect_genesis_default_dir() {
    let args = &["irys-cli", "inspect-genesis"];
    let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
    match cli.command {
        Commands::InspectGenesis { genesis_dir } => {
            assert_eq!(genesis_dir, PathBuf::from("."));
        }
        other => panic!("expected InspectGenesis, got {:?}", other),
    }
}

#[rstest]
#[case(&["irys-cli", "dump-commitments", "--output", "/tmp/out.json"])]
fn test_dump_commitments_parsing(#[case] args: &[&str]) {
    let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
    match cli.command {
        Commands::DumpCommitments { output } => {
            assert_eq!(output, PathBuf::from("/tmp/out.json"));
        }
        other => panic!("expected DumpCommitments, got {:?}", other),
    }
}

#[test]
fn test_dump_commitments_default_output() {
    let args = &["irys-cli", "dump-commitments"];
    let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
    match cli.command {
        Commands::DumpCommitments { output } => {
            assert_eq!(output, PathBuf::from(".irys_genesis_commitments.json"));
        }
        other => panic!("expected DumpCommitments, got {:?}", other),
    }
}

#[rstest]
#[case(&["irys-cli", "compare-genesis", "--genesis-dir", "/tmp/genesis"])]
fn test_compare_genesis_parsing(#[case] args: &[&str]) {
    let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
    match cli.command {
        Commands::CompareGenesis {
            genesis_dir,
            list_retained_partition_hashes,
        } => {
            assert_eq!(genesis_dir, PathBuf::from("/tmp/genesis"));
            assert!(!list_retained_partition_hashes);
        }
        other => panic!("expected CompareGenesis, got {:?}", other),
    }
}

#[test]
fn test_compare_genesis_default_dir() {
    let args = &["irys-cli", "compare-genesis"];
    let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
    match cli.command {
        Commands::CompareGenesis {
            genesis_dir,
            list_retained_partition_hashes,
        } => {
            assert_eq!(genesis_dir, PathBuf::from("."));
            assert!(!list_retained_partition_hashes);
        }
        other => panic!("expected CompareGenesis, got {:?}", other),
    }
}

#[test]
fn test_compare_genesis_list_retained_partition_hashes_flag() {
    let args = &[
        "irys-cli",
        "compare-genesis",
        "--genesis-dir",
        "/tmp/genesis",
        "--list-retained-partition-hashes",
    ];
    let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
    match cli.command {
        Commands::CompareGenesis {
            genesis_dir,
            list_retained_partition_hashes,
        } => {
            assert_eq!(genesis_dir, PathBuf::from("/tmp/genesis"));
            assert!(list_retained_partition_hashes);
        }
        other => panic!("expected CompareGenesis, got {:?}", other),
    }
}

#[test]
fn test_compare_genesis_diff_tracks_wipe_add_and_role_change() {
    fn assignment(hash_byte: u8, kind: PartitionAssignmentKindOutput) -> PartitionAssignmentOutput {
        PartitionAssignmentOutput {
            partition_hash: H256::from([hash_byte; 32]),
            kind,
        }
    }

    let miner_a = irys_types::IrysAddress::from([1_u8; 20]);
    let miner_b = irys_types::IrysAddress::from([2_u8; 20]);
    let miner_c = irys_types::IrysAddress::from([3_u8; 20]);

    let current = SnapshotPartitionState {
        epoch_block: irys_types::IrysBlockHeader::default(),
        total_commitments: 0,
        stake_count: 0,
        pledge_count: 0,
        by_miner: BTreeMap::from([
            (
                miner_a,
                vec![
                    assignment(0x11, PartitionAssignmentKindOutput::Capacity),
                    assignment(
                        0x22,
                        PartitionAssignmentKindOutput::Data {
                            ledger_id: 0,
                            slot_index: 0,
                        },
                    ),
                ],
            ),
            (
                miner_b,
                vec![assignment(0x33, PartitionAssignmentKindOutput::Capacity)],
            ),
        ]),
    };

    let target = SnapshotPartitionState {
        epoch_block: irys_types::IrysBlockHeader::default(),
        total_commitments: 0,
        stake_count: 0,
        pledge_count: 0,
        by_miner: BTreeMap::from([
            (
                miner_a,
                vec![
                    assignment(
                        0x11,
                        PartitionAssignmentKindOutput::Data {
                            ledger_id: 1,
                            slot_index: 0,
                        },
                    ),
                    assignment(
                        0x22,
                        PartitionAssignmentKindOutput::Data {
                            ledger_id: 0,
                            slot_index: 0,
                        },
                    ),
                ],
            ),
            (
                miner_c,
                vec![assignment(0x33, PartitionAssignmentKindOutput::Capacity)],
            ),
        ]),
    };

    let comparison = SnapshotComparisonOutput::from_states("Current", current, "Target", target);

    assert_eq!(comparison.affected_miners(), 3);
    assert_eq!(comparison.wipe_count(), 1);
    assert_eq!(comparison.add_count(), 1);
    assert_eq!(comparison.role_change_count(), 1);
    assert_eq!(comparison.unchanged_count(), 1);
}

#[test]
fn test_compare_genesis_retained_partition_hashes_across_all_nodes() {
    fn assignment(hash_byte: u8, kind: PartitionAssignmentKindOutput) -> PartitionAssignmentOutput {
        PartitionAssignmentOutput {
            partition_hash: H256::from([hash_byte; 32]),
            kind,
        }
    }

    let miner_a = irys_types::IrysAddress::from([1_u8; 20]);
    let miner_b = irys_types::IrysAddress::from([2_u8; 20]);
    let miner_c = irys_types::IrysAddress::from([3_u8; 20]);

    let current = SnapshotPartitionState {
        epoch_block: irys_types::IrysBlockHeader::default(),
        total_commitments: 0,
        stake_count: 0,
        pledge_count: 0,
        by_miner: BTreeMap::from([
            (
                miner_a,
                vec![
                    assignment(0x11, PartitionAssignmentKindOutput::Capacity),
                    assignment(
                        0x22,
                        PartitionAssignmentKindOutput::Data {
                            ledger_id: 0,
                            slot_index: 0,
                        },
                    ),
                ],
            ),
            (
                miner_b,
                vec![assignment(0x33, PartitionAssignmentKindOutput::Capacity)],
            ),
        ]),
    };

    let target = SnapshotPartitionState {
        epoch_block: irys_types::IrysBlockHeader::default(),
        total_commitments: 0,
        stake_count: 0,
        pledge_count: 0,
        by_miner: BTreeMap::from([
            (
                miner_a,
                vec![
                    assignment(
                        0x11,
                        PartitionAssignmentKindOutput::Data {
                            ledger_id: 1,
                            slot_index: 0,
                        },
                    ),
                    assignment(
                        0x22,
                        PartitionAssignmentKindOutput::Data {
                            ledger_id: 0,
                            slot_index: 0,
                        },
                    ),
                ],
            ),
            (
                miner_c,
                vec![assignment(0x33, PartitionAssignmentKindOutput::Capacity)],
            ),
        ]),
    };

    let comparison = SnapshotComparisonOutput::from_states("Current", current, "Target", target);
    let retained = comparison.retained_partition_hashes();

    assert_eq!(retained.len(), 3);
    assert!(retained.contains(&H256::from([0x11; 32])));
    assert!(retained.contains(&H256::from([0x22; 32])));
    assert!(retained.contains(&H256::from([0x33; 32])));
}

#[test]
fn test_compare_genesis_output_lists_retained_partitions() {
    fn assignment(hash_byte: u8, kind: PartitionAssignmentKindOutput) -> PartitionAssignmentOutput {
        PartitionAssignmentOutput {
            partition_hash: H256::from([hash_byte; 32]),
            kind,
        }
    }

    let miner = irys_types::IrysAddress::from([9_u8; 20]);
    let retained = assignment(
        0x42,
        PartitionAssignmentKindOutput::Data {
            ledger_id: 3,
            slot_index: 1,
        },
    );
    let removed = assignment(0x99, PartitionAssignmentKindOutput::Capacity);

    let current = SnapshotPartitionState {
        epoch_block: irys_types::IrysBlockHeader::default(),
        total_commitments: 0,
        stake_count: 0,
        pledge_count: 0,
        by_miner: BTreeMap::from([(miner, vec![retained, removed])]),
    };

    let target = SnapshotPartitionState {
        epoch_block: irys_types::IrysBlockHeader::default(),
        total_commitments: 0,
        stake_count: 0,
        pledge_count: 0,
        by_miner: BTreeMap::from([(miner, vec![retained])]),
    };

    let comparison = SnapshotComparisonOutput::from_states("Current", current, "Target", target);
    let rendered = comparison.to_string();

    assert!(rendered.contains("Per-Miner Actions:"));
    assert!(rendered.contains("Keep 1 unchanged partition(s):"));
    assert!(rendered.contains(&retained.to_string()));
}

mod proptest_fuzz {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn parse_rollback_target_never_panics(s in "\\PC{0,100}") {
            let _ = parse_rollback_target(&s);
        }

        #[test]
        fn timestamp_millis_to_secs_valid_range(millis in 0_u128..=(u128::from(u64::MAX) * 1000 + 999)) {
            let result = timestamp_millis_to_secs(millis);
            prop_assert!(result.is_ok());
            let secs = result.unwrap();
            prop_assert_eq!(secs, u64::try_from(millis / 1000).unwrap());
        }

        #[test]
        fn timestamp_millis_to_secs_overflow_is_error(offset in 1_u128..=10_000) {
            let millis = u128::from(u64::MAX) * 1000 + 999 + offset;
            prop_assert!(timestamp_millis_to_secs(millis).is_err());
        }
    }
}
