//! Optional per-run knobs for the multiversion test harness.
//!
//! Different OLD↔NEW spans expose different cross-version idiosyncrasies:
//! a span across a schema rename needs both sides of the rename treated as
//! equivalent and may need certain fields force-defaulted to avoid
//! canonical signature-hash mismatches. A span across a block-header
//! rename needs analogous handling on the block-comparison side. Rather
//! than hardcode any of that into the harness — which only fits one
//! particular version pair — we read it from a TOML file pointed to by
//! the `--run-config` xtask flag (passed through as the
//! `IRYS_TEST_RUN_CONFIG` env var).
//!
//! The default (no file, no env var) is the empty config: no aliases, no
//! skip lists, every applicable non-default field gets exercised. That's
//! the right default for adjacent-release tests where renames are rare.

use serde::Deserialize;

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunConfig {
    /// Cross-version handling for `DataTransactionHeader` JSON returned
    /// by `/v1/tx/{id}` and compared against the originally-signed tx.
    pub tx_header: SchemaConfig,
    /// Cross-version handling for `BlockHeader` JSON returned by
    /// `/v1/block/{hash}` and compared across cluster nodes.
    pub block_header: SchemaConfig,
    /// Tx-build overrides applied before signing. See [`TxBuildConfig`].
    pub tx_build: TxBuildConfig,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SchemaConfig {
    /// Pairs of camelCase field names that mean the same thing across
    /// the OLD and NEW schemas (e.g. `bundleFormat` ↔ `metadataFormat`
    /// after a rename). Field presence is checked on both sides, but
    /// values are *not* compared — since a rename can also change the
    /// type (e.g. `Option<u64>` → `u8`), no value-level equality holds.
    pub aliases: Vec<(String, String)>,
    /// Field names to skip entirely from the strict diff. Use this when
    /// a field's wire shape differs between OLD and NEW in a way that
    /// `aliases` doesn't model (e.g. a field that was added with no
    /// counterpart on the older side, and you don't want to flag its
    /// absence).
    pub skip: Vec<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TxBuildConfig {
    /// `data_tx::submit_data_tx` normally pokes a few normally-default
    /// header fields to non-default sentinel values before signing —
    /// without that, the on-disk `Compact` encoding for those fields
    /// is a zero-byte payload that survives any schema change
    /// trivially, and the strict round-trip check is partly vacuous.
    ///
    /// Naming a field here (using the Rust struct field name, not the
    /// camelCase wire name) keeps it at its default. Useful for
    /// version pairs that straddle a rename: a non-default value for
    /// the renamed field would change the canonical signature prehash
    /// on one side and the OLD side would (correctly) reject the tx
    /// as `Invalid Signature`.
    pub keep_default: Vec<String>,
}

impl RunConfig {
    /// Loads from the path in `IRYS_TEST_RUN_CONFIG`, or returns the
    /// default (empty) config if unset. Panics on read/parse failure
    /// because a misspelled config path is almost certainly a user
    /// error worth surfacing immediately.
    pub fn load() -> Self {
        match std::env::var("IRYS_TEST_RUN_CONFIG") {
            Ok(path) if !path.is_empty() => {
                let raw = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("failed to read run config at {path}: {e}"));
                toml::from_str(&raw)
                    .unwrap_or_else(|e| panic!("failed to parse run config at {path}: {e}"))
            }
            _ => Self::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_permissive() {
        let cfg = RunConfig::default();
        assert!(cfg.tx_header.aliases.is_empty());
        assert!(cfg.tx_header.skip.is_empty());
        assert!(cfg.block_header.aliases.is_empty());
        assert!(cfg.block_header.skip.is_empty());
        assert!(cfg.tx_build.keep_default.is_empty());
    }

    #[test]
    fn parses_toml_with_all_sections() {
        let raw = r#"
            [tx_header]
            aliases = [["bundleFormat", "metadataFormat"]]
            skip = ["promotedHeight"]

            [block_header]
            aliases = []
            skip = ["someBlockField"]

            [tx_build]
            keep_default = ["metadata_format"]
        "#;
        let cfg: RunConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.tx_header.aliases.len(), 1);
        assert_eq!(cfg.tx_header.aliases[0].0, "bundleFormat");
        assert_eq!(cfg.tx_header.aliases[0].1, "metadataFormat");
        assert_eq!(cfg.tx_header.skip, vec!["promotedHeight".to_string()]);
        assert_eq!(cfg.block_header.skip, vec!["someBlockField".to_string()]);
        assert_eq!(
            cfg.tx_build.keep_default,
            vec!["metadata_format".to_string()]
        );
    }

    #[test]
    fn omitted_sections_default() {
        let cfg: RunConfig = toml::from_str("").unwrap();
        assert!(cfg.tx_header.aliases.is_empty());
    }
}
