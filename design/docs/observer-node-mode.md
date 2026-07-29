# Observer Node Mode

## Status

Accepted

## Context

Some operators run a node to follow and serve the chain, not to mine it. Such a
node has no partition assignments, so three startup behaviors are wrong for it:

- **Startup VDF throughput check** (`crates/chain/src/chain.rs:849-922`) aborts
  the node when the CPU cannot produce one VDF step per second. A node that
  never mines does not need to hold that rate, so the check turns a usable
  node into a refusing one.
- **Partition mining** (`crates/chain/src/main.rs:93`) starts unconditionally.
- **Default submodule creation** (`crates/config/src/submodules.rs:138-160`)
  writes `.irys_submodules.toml` with three submodule paths and creates the
  directories when the file is absent. A node with no assignments stores
  nothing in them.

`NodeConfig.node_mode` already carries operational intent (`Genesis` | `Peer`)
and is a required TOML field. A separate boolean would have produced a
contradictory pair (`Genesis` plus non-mining) and would have needed inference
rules to decide the value when unset. Folding the new role into `NodeMode`
removes both problems: the states that make no sense cannot be expressed, and
there is nothing to infer because the field is mandatory.

The existing name `Peer` describes network position, not role. Both a mining
and a non-mining node are peers, so `Peer` cannot distinguish them.

## Decision

### The enum

```rust
pub enum NodeMode {
    Genesis,
    /// Join an existing network and mine. Previously named `Peer`.
    Miner,
    /// Join an existing network and follow it without mining.
    Observer,
}
```

`Observer` joins and syncs exactly as `Miner` does. It differs only in that it
does not mine.

`Observer` keeps running the local VDF. A local step count that tracks the
chain reduces the parallel VDF work that block validation must do, so the
saving from stopping it is smaller than the validation cost it adds.

### Retiring `Peer`

`node_mode = "Peer"` currently means "mine". Reusing the freed name for the
non-mining role would leave every deployed config parsing without error while
meaning the opposite, and every miner would stop mining on its next restart
with nothing to signal it.

`Peer` is therefore removed rather than reused. `NodeMode` gets a hand-written
`Deserialize` that rejects `"Peer"` with a migration message naming both
replacements. Derived `Serialize` is kept. The `#[serde(deny_unknown_fields)]`
attribute on the enum is dropped — it has no effect on an enum whose variants
are all unit variants.

### Behavior by mode

| | Genesis | Miner | Observer |
|---|---|---|---|
| Startup VDF throughput check | yes | yes | skipped |
| Partition mining | yes | yes | no |
| Local VDF | yes | yes | yes |
| Default submodule creation | yes | yes | no |
| Minimum 3 submodules | required | — | — |

`Observer` skips only the *creation* of default submodules. An existing
`.irys_submodules.toml` is honored, so an operator can still list paths.

`main.rs` splits the current `start_mining()` call: it always starts the VDF
and enables partition mining only outside `Observer`.

### Config validation

- `expected_genesis_hash` must be set (`crates/types/src/config/mod.rs:112`).
- Periodic sync check must be enabled (`crates/types/src/config/mod.rs:282`).
- `stake_pledge_drives = true` is rejected with `Observer`. Pledging drives
  creates the assignments the mode assumes absent.

The first two rules are currently written as
`matches!(node_config.node_mode, NodeMode::Peer)`. A new variant makes such a
test silently `false`, and the compiler does not report it. Both sites move to
a `NodeMode::joins_existing_network()` helper that covers `Miner` and
`Observer`.

## Consequences

- `Observer` plus `Genesis` cannot be expressed. The contradiction is removed
  by construction rather than by a validation rule.
- Every existing config file needs an edit. A config that still says `"Peer"`
  fails at startup with a message naming `Miner` and `Observer`. This is
  deliberate: a loud stop is the only outcome that does not risk a miner
  silently ceasing to mine.
- 14 Rust sites reference `NodeMode::Peer` and are renamed: `config/mod.rs`
  (6), `chain/src/chain.rs:558`, `config/src/submodules.rs` (2),
  `config/node.rs:1284`, and 4 sites under `crates/chain-tests`.
- Files carrying `node_mode = "Peer"` are updated to `"Miner"`: `SETUP.md:75`,
  `MAINNET_BETA.md:56`, `crates/config/templates/mainnet_config.toml`,
  `crates/config/templates/testnet_config.toml`, and 5 configs under `docker/`.
- Multiversion tests: `fixtures/base-config.toml` and
  `examples/base-config-new.toml` move to `"Miner"`;
  `examples/base-config-old.toml` stays `"Peer"`, the spelling a pre-rename
  binary expects. The generator only forces `Genesis` for a genesis node and
  otherwise keeps the template's `node_mode`, so each side's template must
  spell it the way its own binary expects. A span crossing this change must
  therefore pass `--base-config-old`; the one bundled default can no longer
  satisfy both parsers. This is the drift case the harness already documents
  (`crates/tooling/multiversion-tests/README.md`).
- `NodeConfig::testing()` constructors keep `Genesis` and `Miner`, so no
  existing test changes behavior.

### Out of scope

- Retention. `Observer` holds no more data than any other node; it only stops
  mining. Full-history retention is a separate axis and is not added here.
- Any runtime API to turn an `Observer` into a mining node.

## Testing

- `Observer` rejected without `expected_genesis_hash`.
- `Observer` rejected with the periodic sync check disabled.
- `Observer` rejected with `stake_pledge_drives = true`.
- `StorageSubmodulesConfig::load` under `Observer` writes no file, creates no
  directories, and returns an empty config; an existing file is still read.
- Deserializing `node_mode = "Peer"` fails and the error names both `Miner`
  and `Observer`.
- `NodeMode` serde round-trip over all three variants.
