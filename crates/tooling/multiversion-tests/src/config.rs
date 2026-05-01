use std::io::Write as _;
use std::path::Path;
use thiserror::Error;
use toml::Value;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TOML parse error: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("TOML serialize error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
    #[error("base template is not a TOML table")]
    InvalidTemplate,
    #[error("missing section '{0}' in config template")]
    MissingSection(String),
    #[error("consensus config is not a JSON object")]
    InvalidConsensusJson,
}

#[derive(Debug, Clone, Copy)]
pub enum NodeRole {
    Genesis,
    Peer,
}

#[derive(Debug, Clone)]
pub struct PeerEndpoint {
    pub gossip_addr: String,
    pub api_addr: String,
    pub reth_addr: String,
}

pub struct ConfigParams<'a> {
    pub base_template: &'a str,
    pub role: NodeRole,
    pub api_port: u16,
    pub gossip_port: u16,
    pub reth_port: u16,
    pub data_dir: &'a Path,
    pub mining_key: &'a str,
    pub reward_address: &'a str,
    pub peer_endpoints: &'a [PeerEndpoint],
    pub output_path: &'a Path,
}

pub fn generate_config(params: &ConfigParams<'_>) -> Result<(), ConfigError> {
    let mut config: Value = params.base_template.parse()?;
    let table = config.as_table_mut().ok_or(ConfigError::InvalidTemplate)?;

    let node_mode = match params.role {
        NodeRole::Genesis => "Genesis",
        NodeRole::Peer => "Peer",
    };
    table.insert("node_mode".into(), Value::String(node_mode.into()));
    table.insert("mining_key".into(), Value::String(params.mining_key.into()));
    table.insert(
        "reward_address".into(),
        Value::String(params.reward_address.into()),
    );
    table.insert(
        "base_directory".into(),
        Value::String(params.data_dir.display().to_string()),
    );

    let trusted_peers = build_trusted_peers(params.peer_endpoints);
    table.insert("trusted_peers".into(), trusted_peers);

    patch_section(table, "http", params.api_port)?;
    patch_section(table, "gossip", params.gossip_port)?;
    patch_reth_network(table, params.reth_port)?;

    let content = toml::to_string_pretty(&config)?;
    let tmp_path = params.output_path.with_extension("toml.tmp");
    let mut file = std::fs::File::create(&tmp_path)?;
    file.write_all(content.as_bytes())?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp_path, params.output_path)?;
    Ok(())
}

fn patch_section(
    table: &mut toml::map::Map<String, Value>,
    section: &str,
    port: u16,
) -> Result<(), ConfigError> {
    let section_table = table
        .entry(section)
        .or_insert_with(|| Value::Table(Default::default()))
        .as_table_mut()
        .ok_or_else(|| ConfigError::MissingSection(section.into()))?;

    section_table.insert("bind_port".into(), i64::from(port).into());
    section_table.insert("public_port".into(), i64::from(port).into());
    Ok(())
}

fn patch_reth_network(
    table: &mut toml::map::Map<String, Value>,
    port: u16,
) -> Result<(), ConfigError> {
    let reth = table
        .entry("reth")
        .or_insert_with(|| Value::Table(Default::default()))
        .as_table_mut()
        .ok_or_else(|| ConfigError::MissingSection("reth".into()))?;

    let network = reth
        .entry("network")
        .or_insert_with(|| Value::Table(Default::default()))
        .as_table_mut()
        .ok_or_else(|| ConfigError::MissingSection("reth.network".into()))?;

    network.insert("bind_port".into(), i64::from(port).into());
    network.insert("public_port".into(), i64::from(port).into());
    network.insert("use_random_ports".into(), Value::Boolean(true));
    Ok(())
}

/// Rewrites a peer node's config file, replacing `consensus = "Testing"` with
/// `[consensus.Custom]` populated from the genesis node's `/v1/network/config`
/// response plus the actual genesis hash.
///
/// `template_overlay` is the cross-version escape hatch: any fields the user
/// authored under `[consensus.Custom]` in `--base-config-new` win over the
/// genesis-served values, and any fields the genesis doesn't serve at all
/// (because they're new in the upgraded binary) get backfilled from the
/// template. The genesis values still win for any field the template didn't
/// specify, so the upgraded peer stays coherent with the running chain.
pub fn patch_peer_consensus(
    config_path: &Path,
    consensus_json: &serde_json::Value,
    genesis_hash: &str,
    template_overlay: Option<&toml::map::Map<String, Value>>,
) -> Result<(), ConfigError> {
    let content = std::fs::read_to_string(config_path)?;
    let mut config: Value = content.parse()?;
    let table = config.as_table_mut().ok_or(ConfigError::InvalidTemplate)?;

    let mut consensus_toml = json_to_toml(consensus_json)
        .and_then(|v| match v {
            Value::Table(t) => Some(t),
            _ => None,
        })
        .ok_or(ConfigError::InvalidConsensusJson)?;

    if let Some(overlay) = template_overlay {
        overlay_template_onto_consensus(&mut consensus_toml, overlay);
    }

    consensus_toml.insert(
        "expected_genesis_hash".into(),
        Value::String(genesis_hash.to_owned()),
    );

    let mut consensus_wrapper = toml::map::Map::new();
    consensus_wrapper.insert("Custom".into(), Value::Table(consensus_toml));
    table.insert("consensus".into(), Value::Table(consensus_wrapper));

    let output = toml::to_string_pretty(&config)?;
    let tmp_path = config_path.with_extension("toml.tmp");
    let mut file = std::fs::File::create(&tmp_path)?;
    file.write_all(output.as_bytes())?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp_path, config_path)?;
    Ok(())
}

/// Returns the `[consensus.Custom]` sub-table from a base-config template, or
/// `None` if the template uses bare `consensus = "Testing"` / `"Mainnet"` /
/// has no `consensus` key. Used by [`patch_peer_consensus`] as the
/// cross-version overlay source.
pub fn extract_consensus_custom_from_template(
    template: &str,
) -> Option<toml::map::Map<String, Value>> {
    let parsed: Value = template.parse().ok()?;
    let consensus = parsed.as_table()?.get("consensus")?;
    let custom = consensus.as_table()?.get("Custom")?;
    custom.as_table().cloned()
}

/// Recursively merges `overlay` into `target`, with overlay values winning
/// for shared keys. Tables on both sides are merged in-place; any other value
/// (or table-vs-non-table mismatch) is replaced by the overlay value.
fn overlay_template_onto_consensus(
    target: &mut toml::map::Map<String, Value>,
    overlay: &toml::map::Map<String, Value>,
) {
    for (key, overlay_value) in overlay {
        match (target.get_mut(key), overlay_value) {
            (Some(Value::Table(target_table)), Value::Table(overlay_table)) => {
                overlay_template_onto_consensus(target_table, overlay_table);
            }
            (_, _) => {
                target.insert(key.clone(), overlay_value.clone());
            }
        }
    }
}

fn json_to_toml(json: &serde_json::Value) -> Option<Value> {
    match json {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(b) => Some(Value::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(Value::Integer(i))
            } else {
                n.as_f64().map(Value::Float)
            }
        }
        serde_json::Value::String(s) => {
            // Canonical serializes u64/i64 as JSON strings; recover integer type
            if let Ok(i) = s.parse::<i64>() {
                Some(Value::Integer(i))
            } else {
                Some(Value::String(s.clone())) // clone: building owned toml::Value from &String
            }
        }
        serde_json::Value::Array(arr) => {
            let items: Vec<Value> = arr.iter().filter_map(json_to_toml).collect();
            Some(Value::Array(items))
        }
        serde_json::Value::Object(obj) => {
            let mut table = toml::map::Map::new();
            for (k, v) in obj {
                if let Some(tv) = json_to_toml(v) {
                    table.insert(camel_to_snake(k), tv);
                }
            }
            Some(Value::Table(table))
        }
    }
}

/// Converts camelCase keys from the Canonical JSON serializer back to snake_case.
/// Mirrors the `to_snake_case` in `irys_types::canonical` so the roundtrip is exact.
///
/// NOTE: this intentionally matches the canonical implementation's handling of
/// digit-then-uppercase sequences (e.g. `sha1SDifficulty` → `sha_1s_difficulty`).
/// The two must stay in sync — do not "fix" edge cases here without updating
/// the canonical module.
fn camel_to_snake(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut result = String::with_capacity(s.len() + 4);

    for i in 0..chars.len() {
        let c = chars[i];

        if c.is_ascii_uppercase() {
            if i > 0 && !chars[i - 1].is_ascii_digit() {
                result.push('_');
            }
            result.push(c.to_ascii_lowercase());
        } else if c.is_ascii_digit() && i > 0 {
            if i + 1 < chars.len() && chars[i + 1].is_ascii_uppercase() {
                result.push('_');
            }
            result.push(c);
        } else {
            result.push(c.to_ascii_lowercase());
        }
    }
    result
}

fn build_trusted_peers(peers: &[PeerEndpoint]) -> Value {
    let entries: Vec<Value> = peers
        .iter()
        .map(|peer| {
            let mut entry = toml::map::Map::new();
            entry.insert("gossip".into(), Value::String(peer.gossip_addr.clone())); // clone: building owned Value from &String
            entry.insert("api".into(), Value::String(peer.api_addr.clone())); // clone: building owned Value from &String

            let mut execution = toml::map::Map::new();
            execution.insert(
                "peering_tcp_addr".into(),
                Value::String(peer.reth_addr.clone()), // clone: building owned Value from &String
            );
            entry.insert("execution".into(), Value::Table(execution));

            Value::Table(entry)
        })
        .collect();
    Value::Array(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rstest::rstest;

    const BASE_TEMPLATE: &str = include_str!("../fixtures/base-config.toml");

    /// Creates a per-test temporary directory under `.tmp/`.
    fn test_tmp_dir() -> tempfile::TempDir {
        irys_testing_utils::TempDirBuilder::new()
            .prefix("config-test-")
            .build()
    }

    fn read_generated_toml(path: &Path) -> toml::map::Map<String, Value> {
        let content = std::fs::read_to_string(path).unwrap();
        let parsed: Value = content.parse().unwrap();
        parsed.as_table().unwrap().clone() // clone: extracting owned table from parsed value
    }

    fn get_section<'a>(
        table: &'a toml::map::Map<String, Value>,
        key: &str,
    ) -> &'a toml::map::Map<String, Value> {
        table[key].as_table().unwrap()
    }

    proptest! {
        #[test]
        fn config_roundtrip(
            api_port in 1024_u16..65535,
            gossip_port in 1024_u16..65535,
            reth_port in 1024_u16..65535,
            is_genesis in proptest::bool::ANY,
        ) {
            let role = if is_genesis { NodeRole::Genesis } else { NodeRole::Peer };
            let workspace = test_tmp_dir();
            let output_path = workspace.path().join("config.toml");
            let data_dir = workspace.path().join("data");

            generate_config(&ConfigParams {
                base_template: BASE_TEMPLATE,
                role,
                api_port,
                gossip_port,
                reth_port,
                data_dir: &data_dir,
                mining_key: "aaaa000000000000000000000000000000000000000000000000000000000001",
                reward_address: "0x0000000000000000000000000000000000000def",
                peer_endpoints: &[],
                output_path: &output_path,
            }).unwrap();

            let table = read_generated_toml(&output_path);

            let http = get_section(&table, "http");
            prop_assert_eq!(http["bind_port"].as_integer().unwrap(), i64::from(api_port));
            prop_assert_eq!(http["public_port"].as_integer().unwrap(), i64::from(api_port));

            let gossip = get_section(&table, "gossip");
            prop_assert_eq!(gossip["bind_port"].as_integer().unwrap(), i64::from(gossip_port));
            prop_assert_eq!(gossip["public_port"].as_integer().unwrap(), i64::from(gossip_port));

            let reth_net = get_section(table["reth"].as_table().unwrap(), "network");
            prop_assert_eq!(reth_net["bind_port"].as_integer().unwrap(), i64::from(reth_port));
            prop_assert_eq!(reth_net["public_port"].as_integer().unwrap(), i64::from(reth_port));

            let expected_mode = if is_genesis { "Genesis" } else { "Peer" };
            prop_assert_eq!(table["node_mode"].as_str().unwrap(), expected_mode);
            prop_assert_eq!(
                table["mining_key"].as_str().unwrap(),
                "aaaa000000000000000000000000000000000000000000000000000000000001"
            );
            prop_assert_eq!(
                table["reward_address"].as_str().unwrap(),
                "0x0000000000000000000000000000000000000def"
            );
        }

        #[test]
        fn generated_config_is_valid_toml(
            api_port in 1024_u16..65535,
            gossip_port in 1024_u16..65535,
            reth_port in 1024_u16..65535,
        ) {
            let workspace = test_tmp_dir();
            let output_path = workspace.path().join("config.toml");
            generate_config(&ConfigParams {
                base_template: BASE_TEMPLATE,
                role: NodeRole::Peer,
                api_port,
                gossip_port,
                reth_port,
                data_dir: workspace.path(),
                mining_key: "0000000000000000000000000000000000000000000000000000000000001234",
                reward_address: "0x0000000000000000000000000000000000005678",
                peer_endpoints: &[],
                output_path: &output_path,
            }).unwrap();

            let content = std::fs::read_to_string(&output_path).unwrap();
            let result: Result<Value, _> = content.parse();
            prop_assert!(result.is_ok(), "generated config is not valid TOML");
        }
    }

    #[test]
    fn invalid_template_returns_error() {
        let workspace = test_tmp_dir();
        let output_path = workspace.path().join("config.toml");
        let result = generate_config(&ConfigParams {
            base_template: "not valid toml [[[",
            role: NodeRole::Peer,
            api_port: 8080,
            gossip_port: 8081,
            reth_port: 8082,
            data_dir: workspace.path(),
            mining_key: "0000000000000000000000000000000000000000000000000000000000000001",
            reward_address: "0x0000000000000000000000000000000000000001",
            peer_endpoints: &[],
            output_path: &output_path,
        });
        assert!(result.is_err());
    }

    #[test]
    fn trusted_peers_are_preserved() {
        let workspace = test_tmp_dir();
        let output_path = workspace.path().join("config.toml");
        let peers = vec![
            PeerEndpoint {
                gossip_addr: "127.0.0.1:9001".to_owned(),
                api_addr: "127.0.0.1:8001".to_owned(),
                reth_addr: "127.0.0.1:30301".to_owned(),
            },
            PeerEndpoint {
                gossip_addr: "127.0.0.1:9002".to_owned(),
                api_addr: "127.0.0.1:8002".to_owned(),
                reth_addr: "127.0.0.1:30302".to_owned(),
            },
        ];
        generate_config(&ConfigParams {
            base_template: BASE_TEMPLATE,
            role: NodeRole::Peer,
            api_port: 8080,
            gossip_port: 8081,
            reth_port: 8082,
            data_dir: workspace.path(),
            mining_key: "0000000000000000000000000000000000000000000000000000000000000001",
            reward_address: "0x0000000000000000000000000000000000000001",
            peer_endpoints: &peers,
            output_path: &output_path,
        })
        .unwrap();

        let table = read_generated_toml(&output_path);
        let arr = table["trusted_peers"].as_array().unwrap();
        assert_eq!(arr.len(), 2);

        let peer0 = arr[0].as_table().unwrap();
        assert_eq!(peer0["gossip"].as_str().unwrap(), "127.0.0.1:9001");
        assert_eq!(peer0["api"].as_str().unwrap(), "127.0.0.1:8001");
        let exec0 = peer0["execution"].as_table().unwrap();
        assert_eq!(
            exec0["peering_tcp_addr"].as_str().unwrap(),
            "127.0.0.1:30301"
        );

        let peer1 = arr[1].as_table().unwrap();
        assert_eq!(peer1["gossip"].as_str().unwrap(), "127.0.0.1:9002");
        assert_eq!(peer1["api"].as_str().unwrap(), "127.0.0.1:8002");
        let exec1 = peer1["execution"].as_table().unwrap();
        assert_eq!(
            exec1["peering_tcp_addr"].as_str().unwrap(),
            "127.0.0.1:30302"
        );
    }

    #[test]
    fn data_dir_is_written_to_base_directory() {
        let workspace = test_tmp_dir();
        let output_path = workspace.path().join("config.toml");
        let data_dir = Path::new("/some/custom/path");
        generate_config(&ConfigParams {
            base_template: BASE_TEMPLATE,
            role: NodeRole::Peer,
            api_port: 7000,
            gossip_port: 7001,
            reth_port: 7002,
            data_dir,
            mining_key: "0000000000000000000000000000000000000000000000000000000000000001",
            reward_address: "0x0000000000000000000000000000000000000001",
            peer_endpoints: &[],
            output_path: &output_path,
        })
        .unwrap();

        let table = read_generated_toml(&output_path);
        assert_eq!(
            table["base_directory"].as_str().unwrap(),
            "/some/custom/path"
        );
    }

    #[rstest]
    #[case("chainId", "chain_id")]
    #[case("annualCostPerGb", "annual_cost_per_gb")]
    #[case("decayRate", "decay_rate")]
    #[case("blockRewardConfig", "block_reward_config")]
    #[case("expectedGenesisHash", "expected_genesis_hash")]
    #[case("numChunksInPartition", "num_chunks_in_partition")]
    #[case("sha1SDifficulty", "sha_1s_difficulty")]
    #[case("maxFutureTimestampDriftMillis", "max_future_timestamp_drift_millis")]
    #[case(
        "enableFullIngressProofValidation",
        "enable_full_ingress_proof_validation"
    )]
    fn camel_to_snake_known_consensus_fields(#[case] camel: &str, #[case] snake: &str) {
        assert_eq!(camel_to_snake(camel), snake, "failed for {camel}");
    }

    proptest! {
        #[test]
        fn camel_to_snake_preserves_already_snake_case(ref s in "[a-z][a-z_]{0,30}") {
            prop_assert_eq!(&camel_to_snake(s), s);
        }

        #[test]
        fn json_to_toml_recovers_integers_from_strings(n in 0_i64..i64::MAX) {
            let json = serde_json::Value::String(n.to_string());
            let toml_val = json_to_toml(&json).unwrap();
            prop_assert_eq!(toml_val.as_integer().unwrap(), n);
        }

        #[test]
        fn json_to_toml_preserves_non_numeric_strings(ref s in "[a-zA-Z][a-zA-Z0-9_]{0,20}") {
            let json = serde_json::Value::String(s.clone());
            let toml_val = json_to_toml(&json).unwrap();
            prop_assert_eq!(toml_val.as_str().unwrap(), s.as_str());
        }
    }
}
