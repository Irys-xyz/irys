# Observer Node Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a third `NodeMode` variant, `Observer`, that joins an existing network and follows it without mining, and rename the existing `Peer` variant to `Miner`.

**Architecture:** `NodeConfig.node_mode` is a required TOML field that already carries operational intent. All three behaviors an Observer must skip (partition mining, the startup VDF throughput check, default submodule creation) are driven off one predicate, `NodeMode::mines()`. A second predicate, `NodeMode::joins_existing_network()`, replaces the two `matches!(.., NodeMode::Peer)` validation tests so they cover `Observer` too. The old spelling `"Peer"` is removed rather than reused, and a hand-written `Deserialize` turns it into a migration error.

**Tech Stack:** Rust 1.93.0, edition 2024. `serde` + `toml` for config. `eyre::ensure!` for config validation. `rstest` for parameterized tests, `cargo nextest` as the runner.

## Global Constraints

- Design doc of record: `design/docs/observer-node-mode.md`. Read it before starting.
- The three final variant names are exactly `Genesis`, `Miner`, `Observer`. Serde spelling matches the identifier (the enum has no `rename_all`).
- `"Peer"` must never again be a valid value of `node_mode`.
- `crates/tooling/multiversion-tests/examples/base-config-old.toml` keeps `node_mode = "Peer"`. It configures an older binary that only accepts that spelling. Do not change it.
- The word "peer" is used throughout `crates/p2p` and elsewhere to mean "another node on the network". That usage is unrelated to `node_mode`. Only rename identifiers, strings, and comments that describe the *config mode*.
- `Observer` keeps running the local VDF. Only partition mining stops. Do not touch `start_vdf` / `stop_vdf` / `VdfController`.
- No new config field is added. There is no inference or defaulting of `node_mode`; it stays required.
- Before finishing: `cargo fmt --all` then `cargo clippy --workspace --tests --all-targets`.
- Integration tests need a writable temp dir. If `.tmp` is not writable in your sandbox, export `IRYS_CUSTOM_TMP_DIR` to a scratch path before running them.
- Commit after each task. Do not add `Co-Authored-By` lines.

---

### Task 1: Rename `NodeMode::Peer` to `NodeMode::Miner`

Pure rename, no new variant and no behavior change. Driven by the compiler plus the two template-parsing tests.

**Files:**
- Modify: `crates/types/src/config/node.rs:243-253` (enum), `:1284` (`testing_peer_with_signer` constructor)
- Modify: `crates/types/src/config/mod.rs:112`, `:282` (validation), `:1366`, `:1391` (template asserts), `:1889-1891`, `:2262-2267` (validation test setup)
- Modify: `crates/chain/src/chain.rs:558`, `:562`
- Modify: `crates/config/src/submodules.rs:90-91`, `:245-251`
- Modify: `crates/chain-tests/src/utils.rs:840`, `:849`
- Modify: `crates/chain-tests/src/multi_node/sync_chain_state.rs:218`
- Modify: `crates/chain-tests/src/block_production/reset_seed.rs:125`
- Modify: `crates/config/templates/mainnet_config.toml:2`, `crates/config/templates/testnet_config.toml:1`
- Modify: `SETUP.md:75`, `MAINNET_BETA.md:56`
- Modify: `docker/configs/irys-2.toml:1`, `docker/agent_cluster/configs/irys-2.toml:1`, `docker/agent_cluster/configs/irys-3.toml:1`, `docker/tests/data-sync/configs/irys-2.toml:1`, `docker/tests/data-sync/configs/irys-3.toml:1`
- Modify: `crates/tooling/multiversion-tests/fixtures/base-config.toml:1`, `crates/tooling/multiversion-tests/examples/base-config-new.toml:1`

**Interfaces:**
- Consumes: nothing.
- Produces: `NodeMode::Miner` — the variant every later task builds on. `NodeMode` is re-exported at the `irys_types` crate root.

- [ ] **Step 1: Update the two config templates so the existing parse tests fail**

In `crates/config/templates/testnet_config.toml:1` and `crates/config/templates/mainnet_config.toml:2`:

```toml
node_mode = "Miner"
```

- [ ] **Step 2: Run the template tests to verify they fail**

```bash
cargo nextest run -p irys-types test_parse_testnet_config_template test_parse_mainnet_config_template
```

Expected: both FAIL. The error comes from `toml::from_str::<NodeConfig>` and reads `unknown variant \`Miner\`, expected \`Genesis\` or \`Peer\``.

- [ ] **Step 3: Rename the enum variant**

`crates/types/src/config/node.rs:239-253` becomes:

```rust
/// # Node Operation Mode
///
/// Defines how the node participates in the network - either as a genesis node
/// that starts a new network or as a node that joins an existing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum NodeMode {
    /// Start a new blockchain network as the first node
    Genesis,

    /// Join an existing network by connecting to trusted peers, and mine.
    /// Requires `consensus.expected_genesis_hash` to be set.
    Miner,
}
```

- [ ] **Step 4: Update the remaining Rust references**

`crates/types/src/config/node.rs:1284` — in `testing_peer_with_signer`:

```rust
            node_mode: NodeMode::Miner,
```

`crates/types/src/config/mod.rs:112-117`:

```rust
        if matches!(self.node_config.node_mode, NodeMode::Miner) {
            ensure!(
                self.consensus.expected_genesis_hash.is_some(),
                "expected_genesis_hash must be set in consensus config for Miner nodes"
            );
        }
```

`crates/types/src/config/mod.rs:276-291` — keep the existing comment above the block, and change the test and the message:

```rust
        if matches!(self.node_config.node_mode, NodeMode::Miner) {
            let periodic_disabled = !self.node_config.sync.enable_periodic_sync_check
                || self.node_config.sync.periodic_sync_check_interval_secs == 0;
            ensure!(
                !periodic_disabled,
                "Miner nodes require sync.enable_periodic_sync_check = true \
                 and sync.periodic_sync_check_interval_secs > 0; without periodic re-engagement \
                 a node that boots before any peers are reachable would stay unsynced indefinitely"
            );
        }
```

`crates/chain/src/chain.rs:558-563`:

```rust
            NodeMode::Miner => {
                let expected_genesis_hash = self
                    .config
                    .consensus
                    .expected_genesis_hash
                    .expect("expected_genesis_hash must be configured for Miner nodes");
```

`crates/config/src/submodules.rs:90-91` — inside `load_for_test`:

```rust
        // Tests don't enforce genesis minimum — pass Miner to skip the check.
        Self::from_toml(config_path_local, NodeMode::Miner)
```

`crates/config/src/submodules.rs:239-254` — rename the test and its `from_toml` argument:

```rust
    #[rstest]
    #[case(0, false)]
    #[case(1, false)]
    #[case(2, false)]
    #[case(3, false)]
    #[case(10, false)]
    fn from_toml_submodule_count_boundary_miner(
        #[case] count: usize,
        #[case] should_err: bool,
    ) -> eyre::Result<()> {
        let dir = TempDirBuilder::new().build();
        let path = write_config_toml(dir.path(), count)?;
        let result = StorageSubmodulesConfig::from_toml(&path, NodeMode::Miner);
        assert_eq!(result.is_err(), should_err);
        Ok(())
    }
```

`crates/chain-tests/src/utils.rs:840` and `:849`:

```rust
        if matches!(node_config.node_mode, NodeMode::Miner) {
            panic!("Can only create a peer from a genesis config");
        }
```

```rust
        peer_config.node_mode = NodeMode::Miner;
```

`crates/chain-tests/src/multi_node/sync_chain_state.rs:218` and `crates/chain-tests/src/block_production/reset_seed.rs:125` — both become:

```rust
    ctx_peer1_node.node_mode = NodeMode::Miner;
```

(In `sync_chain_state.rs` the binding is `ctx_peer2_node`; keep the existing binding name.)

- [ ] **Step 5: Update the four tests in `crates/types/src/config/mod.rs`**

`:1366` and `:1391` — both template sanity asserts:

```rust
        assert!(matches!(config.node_mode, NodeMode::Miner));
```

`:1888-1891` — rename the test and its setter:

```rust
    #[test]
    fn validate_rejects_miner_mode_without_genesis_hash() {
        let cfg = config_with_node(|nc| {
            nc.node_mode = NodeMode::Miner;
            nc.consensus.get_mut().expected_genesis_hash = None;
        });
```

`:2262-2267` — rename the test and its setter, and update the inline comment:

```rust
    fn validate_rejects_miner_mode_without_periodic_sync(
        #[case] enable_periodic: bool,
        #[case] interval_secs: u64,
    ) {
        let cfg = config_with_node(|nc| {
            nc.node_mode = NodeMode::Miner;
            // Miner mode also requires expected_genesis_hash; set it so the
            // earlier-firing genesis-hash check passes and the later
            // periodic-sync check is the one that surfaces.
            nc.consensus.get_mut().expected_genesis_hash = Some(H256::zero());
```

If these two tests assert on error-message substrings further down, update the expected substring to match the new wording from Step 4.

- [ ] **Step 6: Update the non-Rust config files and docs**

Set `node_mode = "Miner"` in each of:

```
SETUP.md:75
MAINNET_BETA.md:56
docker/configs/irys-2.toml:1
docker/agent_cluster/configs/irys-2.toml:1
docker/agent_cluster/configs/irys-3.toml:1
docker/tests/data-sync/configs/irys-2.toml:1
docker/tests/data-sync/configs/irys-3.toml:1
crates/tooling/multiversion-tests/fixtures/base-config.toml:1
crates/tooling/multiversion-tests/examples/base-config-new.toml:1
```

Preserve any trailing comment on the line (for example `#changeme` in `docker/configs/irys-2.toml`).

Leave `crates/tooling/multiversion-tests/examples/base-config-old.toml` at `"Peer"`.

- [ ] **Step 7: Confirm no `NodeMode::Peer` references remain**

```bash
grep -rn "NodeMode::Peer" --include=*.rs crates/
grep -rn 'node_mode = "Peer"' --include=*.toml --include=*.md . | grep -v target
```

Expected: the first command returns nothing. The second returns exactly one line, `crates/tooling/multiversion-tests/examples/base-config-old.toml:1`.

- [ ] **Step 8: Run the tests to verify they pass**

```bash
cargo xtask check
cargo nextest run -p irys-types -p irys-config
```

Expected: check succeeds; all `irys-types` and `irys-config` tests PASS.

- [ ] **Step 9: Add a note to the multiversion README**

In `crates/tooling/multiversion-tests/README.md`, immediately after the paragraph ending at line 110 (`... examples/base-config-old.toml and examples/base-config-new.toml.`), add:

```markdown
`node_mode` was renamed from `Peer` to `Miner`. A span whose OLD ref predates that
rename must pass `--base-config-old`, because the bundled `fixtures/base-config.toml`
tracks the current schema and the old binary rejects `Miner`.
```

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "refactor(config): rename NodeMode::Peer to NodeMode::Miner"
```

---

### Task 2: Add the `Observer` variant, mode predicates, and validation rules

Adds the variant and everything that decides whether a config is *valid*. No startup behavior changes yet — those are Tasks 4 and 5.

**Files:**
- Modify: `crates/types/src/config/node.rs` (enum + new `impl NodeMode` block)
- Modify: `crates/types/src/config/mod.rs` (two validation sites + one new rule)
- Modify: `crates/chain/src/chain.rs:553-565` (match arm)
- Test: `crates/types/src/config/node.rs` (new `mod node_mode_tests`), `crates/types/src/config/mod.rs` (`mod validate_tests`)

**Interfaces:**
- Consumes: `NodeMode::Miner` from Task 1.
- Produces:
  - `NodeMode::Observer`
  - `NodeMode::mines(self) -> bool` — `true` for `Genesis` and `Miner`. Tasks 4 and 5 gate on this.
  - `NodeMode::joins_existing_network(self) -> bool` — `true` for `Miner` and `Observer`.

- [ ] **Step 1: Write the failing tests for the two predicates**

Add a new test module at the end of `crates/types/src/config/node.rs` (the existing `mod run_mode_tests` at `:1497` is scoped to `RunMode`; do not put these there):

```rust
#[cfg(test)]
mod node_mode_tests {
    use super::*;

    #[test]
    fn mines_is_true_for_genesis_and_miner_only() {
        assert!(NodeMode::Genesis.mines());
        assert!(NodeMode::Miner.mines());
        assert!(!NodeMode::Observer.mines());
    }

    #[test]
    fn joins_existing_network_excludes_genesis() {
        assert!(!NodeMode::Genesis.joins_existing_network());
        assert!(NodeMode::Miner.joins_existing_network());
        assert!(NodeMode::Observer.joins_existing_network());
    }
}
```

- [ ] **Step 2: Write the failing validation tests**

Add to `mod validate_tests` in `crates/types/src/config/mod.rs`. The `config_with_node` helper is defined at `:1712` and starts from `NodeConfig::testing()`.

```rust
    #[test]
    fn validate_accepts_observer_mode() {
        let cfg = config_with_node(|nc| {
            nc.node_mode = NodeMode::Observer;
            nc.consensus.get_mut().expected_genesis_hash = Some(H256::zero());
        });
        cfg.validate()
            .expect("observer config with a genesis hash should validate");
    }

    #[test]
    fn validate_rejects_observer_mode_without_genesis_hash() {
        let cfg = config_with_node(|nc| {
            nc.node_mode = NodeMode::Observer;
            nc.consensus.get_mut().expected_genesis_hash = None;
        });
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("expected_genesis_hash"),
            "expected the genesis-hash rule to fire, got: {err}"
        );
    }

    #[rstest]
    #[case::disabled(false, 30)]
    #[case::zero_interval(true, 0)]
    fn validate_rejects_observer_mode_without_periodic_sync(
        #[case] enable_periodic: bool,
        #[case] interval_secs: u64,
    ) {
        let cfg = config_with_node(|nc| {
            nc.node_mode = NodeMode::Observer;
            nc.consensus.get_mut().expected_genesis_hash = Some(H256::zero());
            nc.sync.enable_periodic_sync_check = enable_periodic;
            nc.sync.periodic_sync_check_interval_secs = interval_secs;
        });
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("periodic_sync_check"),
            "expected the periodic-sync rule to fire, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_observer_with_stake_pledge_drives() {
        let cfg = config_with_node(|nc| {
            nc.node_mode = NodeMode::Observer;
            nc.consensus.get_mut().expected_genesis_hash = Some(H256::zero());
            nc.stake_pledge_drives = true;
        });
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("stake_pledge_drives"),
            "expected the stake_pledge_drives rule to fire, got: {err}"
        );
    }
```

- [ ] **Step 3: Run both test sets to verify they fail**

```bash
cargo nextest run -p irys-types node_mode_tests observer
```

Expected: compilation FAILS with `no variant or associated item named \`Observer\` found for enum \`NodeMode\``.

- [ ] **Step 4: Add the variant and the predicates**

In `crates/types/src/config/node.rs`, extend the enum written in Task 1:

```rust
pub enum NodeMode {
    /// Start a new blockchain network as the first node
    Genesis,

    /// Join an existing network by connecting to trusted peers, and mine.
    /// Requires `consensus.expected_genesis_hash` to be set.
    Miner,

    /// Join an existing network and follow it without mining. Skips partition
    /// mining, the startup VDF throughput check, and default submodule
    /// creation. The local VDF still runs: a step count that tracks the chain
    /// reduces the parallel VDF work block validation has to do.
    /// Requires `consensus.expected_genesis_hash` to be set.
    Observer,
}

impl NodeMode {
    /// Whether this mode participates in mining. Gates partition mining, the
    /// startup VDF throughput check, and default submodule creation —
    /// submodules exist to hold packed partitions, which only a mining node
    /// is assigned.
    pub const fn mines(self) -> bool {
        matches!(self, Self::Genesis | Self::Miner)
    }

    /// Whether this mode joins a network that already exists. Rules that
    /// govern joining — a pinned genesis hash, a working periodic sync
    /// check — must cover every such mode, not just `Miner`.
    pub const fn joins_existing_network(self) -> bool {
        matches!(self, Self::Miner | Self::Observer)
    }
}
```

- [ ] **Step 5: Switch the two validation sites to the predicate and add the new rule**

`crates/types/src/config/mod.rs:112` — replace the `matches!` test and widen the message:

```rust
        if self.node_config.node_mode.joins_existing_network() {
            ensure!(
                self.consensus.expected_genesis_hash.is_some(),
                "expected_genesis_hash must be set in consensus config for Miner and Observer nodes"
            );
        }
```

`crates/types/src/config/mod.rs:282` — same predicate:

```rust
        if self.node_config.node_mode.joins_existing_network() {
            let periodic_disabled = !self.node_config.sync.enable_periodic_sync_check
                || self.node_config.sync.periodic_sync_check_interval_secs == 0;
            ensure!(
                !periodic_disabled,
                "Miner and Observer nodes require sync.enable_periodic_sync_check = true \
                 and sync.periodic_sync_check_interval_secs > 0; without periodic re-engagement \
                 a node that boots before any peers are reachable would stay unsynced indefinitely"
            );
        }
```

Add the new rule directly after the `joins_existing_network` block at `:112`:

```rust
        // Pledging drives creates the partition assignments an Observer node
        // is defined as not having. Reject the pair rather than silently
        // letting one win.
        ensure!(
            !(matches!(self.node_config.node_mode, NodeMode::Observer)
                && self.node_config.stake_pledge_drives),
            "node_mode = \"Observer\" cannot be combined with stake_pledge_drives = true: \
             pledging drives assigns partitions to mine, which an Observer node never does"
        );
```

- [ ] **Step 6: Add the `Observer` arm to the genesis-source match**

`crates/chain/src/chain.rs:553` — the `match node_mode` gains `Observer`, sharing the `Miner` arm. An Observer joins an existing network, so it fetches genesis from a trusted peer exactly as a Miner does:

```rust
            NodeMode::Miner | NodeMode::Observer => {
                let expected_genesis_hash = self
                    .config
                    .consensus
                    .expected_genesis_hash
                    .expect("expected_genesis_hash must be configured for Miner and Observer nodes");
```

The rest of the arm body is unchanged.

- [ ] **Step 7: Run the tests to verify they pass**

```bash
cargo xtask check
cargo nextest run -p irys-types -p irys-config
```

Expected: all PASS, including the six new tests.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat(config): add NodeMode::Observer with mode predicates and validation"
```

---

### Task 3: Retire `"Peer"` with a migration error

After Task 1, an old config already fails — but with serde's generic `unknown variant` message, which does not tell the operator which replacement preserves their node's behavior. This task replaces that with an explicit instruction.

**Files:**
- Modify: `crates/types/src/config/node.rs` (enum derive + new `Deserialize` impl)
- Test: `crates/types/src/config/node.rs` (`mod node_mode_tests`, added in Task 2)

**Interfaces:**
- Consumes: `NodeMode::{Genesis, Miner, Observer}` from Tasks 1 and 2.
- Produces: no new API. `NodeMode` gains a hand-written `Deserialize` and loses the derived one.

- [ ] **Step 1: Write the failing tests**

Add to `mod node_mode_tests` in `crates/types/src/config/node.rs`. A TOML document must be a table, so a bare `"Peer"` string is not parseable on its own — these go through a one-field wrapper, which also matches how `node_mode` is really read:

```rust
    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct ModeDoc {
        node_mode: NodeMode,
    }

    #[test]
    fn deserialize_peer_reports_the_rename() {
        let err = toml::from_str::<ModeDoc>("node_mode = \"Peer\"")
            .expect_err("\"Peer\" must not deserialize")
            .to_string();
        assert!(err.contains("Miner"), "error must name Miner: {err}");
        assert!(err.contains("Observer"), "error must name Observer: {err}");
    }

    #[test]
    fn deserialize_unknown_mode_lists_the_variants() {
        let err = toml::from_str::<ModeDoc>("node_mode = \"Nonsense\"")
            .expect_err("unknown variants must not deserialize")
            .to_string();
        assert!(err.contains("Genesis"), "error must list Genesis: {err}");
        assert!(err.contains("Miner"), "error must list Miner: {err}");
        assert!(err.contains("Observer"), "error must list Observer: {err}");
    }

    #[rstest]
    #[case(NodeMode::Genesis)]
    #[case(NodeMode::Miner)]
    #[case(NodeMode::Observer)]
    fn serde_round_trip(#[case] mode: NodeMode) {
        let doc = ModeDoc { node_mode: mode };
        let encoded = toml::to_string(&doc).expect("serializes");
        let decoded: ModeDoc = toml::from_str(&encoded).expect("deserializes");
        assert_eq!(doc, decoded);
    }
```

Add `use rstest::rstest;` to the `node_mode_tests` module's imports. `Serialize` and `Deserialize` come in via the module's `use super::*;`.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo nextest run -p irys-types node_mode_tests
```

Expected: `deserialize_peer_reports_the_rename` FAILS — the derived `Deserialize` produces `unknown variant \`Peer\`, expected one of \`Genesis\`, \`Miner\`, \`Observer\``, which contains neither the word "Miner" as guidance nor any mention of the rename. `serde_round_trip` and `deserialize_unknown_mode_lists_the_variants` should already PASS.

- [ ] **Step 3: Replace the derived `Deserialize` with a hand-written one**

In `crates/types/src/config/node.rs`, drop `Deserialize` from the derive list and drop `#[serde(deny_unknown_fields)]` — the attribute has no effect on an enum whose variants are all unit variants, and serde's `Serialize` derive does not accept it:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum NodeMode {
```

Add the impl immediately after the enum:

```rust
/// Hand-written so the retired `Peer` spelling produces a migration error.
///
/// `Peer` used to mean "join the network and mine". The name was removed rather
/// than reused for the non-mining mode: a reused name would leave every
/// deployed config parsing without error while meaning the opposite, and every
/// miner would stop mining on its next restart with nothing to signal it.
impl<'de> Deserialize<'de> for NodeMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        const VARIANTS: &[&str] = &["Genesis", "Miner", "Observer"];
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "Genesis" => Ok(Self::Genesis),
            "Miner" => Ok(Self::Miner),
            "Observer" => Ok(Self::Observer),
            "Peer" => Err(serde::de::Error::custom(
                "node_mode = \"Peer\" no longer exists: use \"Miner\" to keep the previous \
                 behavior (join the network and mine), or \"Observer\" to join and follow the \
                 chain without mining",
            )),
            other => Err(serde::de::Error::unknown_variant(other, VARIANTS)),
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo nextest run -p irys-types
```

Expected: all PASS. In particular the two template-parsing tests still pass, confirming the hand-written impl reads the real config files correctly.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(config): reject retired node_mode \"Peer\" with a migration error"
```

---

### Task 4: Skip default submodule creation for `Observer`

**Files:**
- Modify: `crates/config/src/submodules.rs:94-161` (`load`), and the doc comment at `:10-27`
- Test: `crates/config/src/submodules.rs` (`mod tests`)

**Interfaces:**
- Consumes: `NodeMode::mines()` from Task 2.
- Produces: `StorageSubmodulesConfig::load` returns `Self::default()` (empty `submodule_paths`, `is_using_hardcoded_paths: false`) for a non-mining mode with no config file present. Signature is unchanged.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/config/src/submodules.rs`:

```rust
    #[test]
    fn load_skips_default_config_for_observer() -> eyre::Result<()> {
        let dir = TempDirBuilder::new().build();
        let config = StorageSubmodulesConfig::load(dir.path().to_path_buf(), NodeMode::Observer)?;

        assert!(config.submodule_paths.is_empty());
        assert!(
            !dir.path().join(SUBMODULES_CONFIG_FILE_NAME).exists(),
            "observer must not write a submodules config file"
        );
        assert!(
            !dir.path().join("storage_modules/submodule_0").exists(),
            "observer must not create default submodule directories"
        );
        Ok(())
    }

    #[test]
    fn load_honors_an_existing_config_for_observer() -> eyre::Result<()> {
        let dir = TempDirBuilder::new().build();
        // A Miner run creates the file; a later Observer run must read it
        // rather than ignore it.
        let created = StorageSubmodulesConfig::load(dir.path().to_path_buf(), NodeMode::Miner)?;
        let reloaded = StorageSubmodulesConfig::load(dir.path().to_path_buf(), NodeMode::Observer)?;

        assert_eq!(created, reloaded);
        assert_eq!(reloaded.submodule_paths.len(), 3);
        Ok(())
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo nextest run -p irys-config submodules::tests
```

Expected: `load_skips_default_config_for_observer` FAILS — `config.submodule_paths` has 3 entries and the config file exists, because `load` currently creates defaults for every mode. `load_honors_an_existing_config_for_observer` should already PASS.

- [ ] **Step 3: Add the mode gate to `load`**

In `crates/config/src/submodules.rs`, the final `else` branch at `:138` becomes an `else if` chain. Everything before it (the symlink cleanup at `:97-107` and the `config_path_local.exists()` branch at `:109-137`) is unchanged:

```rust
        } else if !node_mode.mines() {
            // A non-mining node is assigned no partitions, so the three
            // default submodules would only ever be empty directories.
            // Leave the filesystem untouched and run with no storage modules.
            tracing::info!(
                "node_mode does not mine — not creating a default submodule config at {:?}",
                config_path_local
            );
            Ok(Self::default())
        } else {
            tracing::info!("Creating default config at {:?}", config_path_local);
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo nextest run -p irys-config
```

Expected: all PASS.

- [ ] **Step 5: Update the module doc comment**

`crates/config/src/submodules.rs:15-17` currently reads:

```rust
/// This file is automatically created if it does not exist when the node starts, and is
/// populated with `submodule_paths` set to a default configuration of 3 storage modules
/// (This should be the same as the minimum required configuration to initiate a network genesis)
```

Replace with:

```rust
/// This file is automatically created if it does not exist when the node starts, and is
/// populated with `submodule_paths` set to a default configuration of 3 storage modules
/// (This should be the same as the minimum required configuration to initiate a network genesis).
/// Modes that do not mine are assigned no partitions, so no file is created for them and
/// the config loads empty.
```

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(config): skip default submodule creation for non-mining modes"
```

---

### Task 5: Skip the VDF throughput check and partition mining for `Observer`

Both gates read the same `mines()` predicate that Task 2 unit-tested. The wiring itself is two one-line conditions in startup code with no test seam — it is verified by the compiler, clippy, and one existing integration test proving the mining path is unchanged. Do not invent an integration test asserting "did not mine": `PartitionMiningController` (`crates/actors/src/partition_mining_service.rs:36-46`) exposes only `set_mining`, with no way to read the state back, so such a test could not observe the behavior it claims to check.

**Files:**
- Modify: `crates/chain/src/chain.rs:849-852`
- Modify: `crates/chain/src/main.rs:93`

**Interfaces:**
- Consumes: `NodeMode::mines()` from Task 2; `IrysNodeCtx.config` (`crates/chain/src/chain.rs:112`), which holds the resolved `Config` and therefore `config.node_config.node_mode`.
- Produces: nothing later tasks depend on. This is the last task.

- [ ] **Step 1: Gate the VDF throughput check**

`crates/chain/src/chain.rs:849-852` — extend the comment and the condition:

```rust
        // VDF throughput check — verify this CPU can keep up with the chain's
        // VDF difficulty before committing to a full startup.
        // Skipped when sha_1s_difficulty is low (test configs use ~1000), and
        // for modes that do not mine: they never need to hold the one-step-per-
        // second rate, so failing them here would refuse a usable node.
        if self.config.node_config.node_mode.mines() && self.config.vdf.sha_1s_difficulty >= 1_000_000
        {
```

The body of the block is unchanged.

- [ ] **Step 2: Gate partition mining at startup**

`crates/chain/src/main.rs:93` — replace the single `handle.start_mining()?;` call:

```rust
    // An Observer keeps running the VDF: a local step count that tracks the
    // chain reduces the parallel VDF work block validation has to do. It just
    // never mines partitions.
    handle.start_vdf();
    if handle.config.node_config.node_mode.mines() {
        handle.set_partition_mining(true)?;
    }
```

`start_vdf` and `set_partition_mining` are both public on `IrysNodeCtx` (`crates/chain/src/chain.rs:299` and `:291`). Together they are exactly what `start_mining()` (`:284`) does, so the mining path is unchanged for `Genesis` and `Miner`.

- [ ] **Step 3: Verify it compiles clean**

```bash
cargo xtask check
cargo clippy --workspace --tests --all-targets
```

Expected: no errors. If clippy reports `start_mining` as dead code, leave it — it is public API used by the test harness and by `crates/chain-tests`.

- [ ] **Step 4: Verify the mining path still works end to end**

```bash
cargo nextest run -p irys-chain-tests heavy_test_can_resume_from_genesis_startup_with_ctx
```

Expected: PASS. This boots a real `Genesis` node through `start()` and produces blocks, so it exercises both edited gates on the mining side. If `.tmp` is not writable in your sandbox, export `IRYS_CUSTOM_TMP_DIR` to a scratch path first.

- [ ] **Step 5: Run the full local check suite**

```bash
cargo fmt --all
cargo xtask local-checks
```

Expected: clean. Fix anything it reports.

- [ ] **Step 6: Mark the design doc accepted**

In `design/docs/observer-node-mode.md`, change the `## Status` section body from `Proposed` to `Accepted`.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(chain): skip VDF throughput check and partition mining for Observer"
```

---

## Verification Summary

After Task 5, the following must all hold:

- `grep -rn "NodeMode::Peer" --include=*.rs crates/` returns nothing.
- `grep -rn 'node_mode = "Peer"' . | grep -v target` returns only `crates/tooling/multiversion-tests/examples/base-config-old.toml:1`.
- A config with `node_mode = "Peer"` fails to start with an error naming both `Miner` and `Observer`.
- A config with `node_mode = "Observer"` and no `expected_genesis_hash`, or with the periodic sync check disabled, or with `stake_pledge_drives = true`, fails `Config::validate()`.
- `cargo xtask local-checks` is clean.
