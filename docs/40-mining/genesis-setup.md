# Multi-Miner Genesis Setup

This guide covers creating a genesis block for a multi-miner Irys network.

There are two workflows:

- **From mining keys** -- generate commitments from private keys (for testing)
- **From existing commitments** -- package pre-signed commitments into a genesis
  block (for production)

## Prerequisites

- The `irys-rs` repo checked out with a working Rust toolchain
- A `config.toml` with the desired consensus configuration

All commands below use `cargo run -p irys-cli --` to build and run the CLI
from source. If you have a pre-built `irys-cli` binary, replace that prefix
with just `irys-cli`.

## Helper: Derive Addresses from a Mining Key

Use `generate-miner-info` to derive a miner's Irys and EVM addresses:

```bash
cargo run -p irys-cli -- generate-miner-info --key <hex-private-key>
```

> **Security warning:** Passing private keys on the command line exposes them
> in shell history and process listings. Prefer `--key-file <path>` or the
> `IRYS_SIGNING_KEY` environment variable instead.

Output:
```text
Mining key:   f57554aff54acd4...
Irys address: 2Z7NNbu2hgdx9qzoYbLX8YTAAJtR
EVM address:  0x81c23e4bde4c7086400cdcbca2dfe9a96dbd0fad
```

The **EVM address** is needed for funding accounts in `reth.alloc`.

---

## Workflow A: Genesis from Mining Keys (Testing)

Use this when you have access to all miners' private keys and want commitments
generated automatically.

### 1. Create `genesis_miners.toml`

```toml
[[miners]]
mining_key = "f57554aff54acd4cfaa084f45a7062d5869c8dbb789f7d6a883fade660960303"
pledge_count = 8

[[miners]]
mining_key = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0"
pledge_count = 3
```

**Notes:**
- `pledge_count` is the number of storage partitions this miner commits to
  store. Each miner also gets one implicit stake commitment.
- Miner order in the TOML does not affect the output -- miners are sorted by
  their derived Irys address before commitment generation.
- Every miner must have `pledge_count >= 1`.
- No two miners may share the same key.

### 2. Fund Miner Accounts

In `config.toml`, add genesis account balances for each miner:

```toml
[consensus.reth.alloc."0x81c23e4bde4c7086400cdcbca2dfe9a96dbd0fad"]
balance = "10000000000000000000000000"

[consensus.reth.alloc."0xAnotherMinerEvmAddress"]
balance = "10000000000000000000000000"
```

Every miner with pledges must also have a stake -- the genesis builder creates
one stake commitment per miner automatically. The alloc balance must cover
both the stake commitment value and any pledge fees (which increase with
pledge count per miner), denominated in the chain's smallest token unit.

### 3. Build the Genesis Block

```bash
CONFIG=config.toml cargo run -p irys-cli -- build-genesis \
  --miners genesis_miners.toml \
  --output ./genesis-artifacts/
```

Set the initial packed partitions used for genesis difficulty in
`config.toml` under `[consensus.genesis]` as `initial_packed_partitions`.

---

## Workflow B: Genesis from Existing Commitments (Production)

Use this when miners have independently signed their own commitment
transactions and you have them collected as a JSON file.

### 1. Collect Commitments

Gather all pre-signed `CommitmentTransaction` objects into a single JSON file.
The format is a JSON array matching the `Vec<CommitmentTransaction>` schema
(same format as `.irys_genesis_commitments.json`):

```json
[
  {
    "version": 2,
    "id": "...",
    "anchor": "...",
    "signer": "...",
    "commitmentType": "Stake",
    "chainId": "1270",
    "fee": "0",
    "value": "...",
    "signature": "..."
  },
  ...
]
```

### 2. Fund Miner Accounts

Same as Workflow A Step 2 -- add `reth.alloc` entries for each miner's EVM
address with sufficient balance to cover stake and pledge fees.

### 3. Build the Genesis Block

```bash
CONFIG=config.toml cargo run -p irys-cli -- build-genesis \
  --commitments commitments.json \
  --signing-key <hex-private-key-for-block-signature> \
  --output ./genesis-artifacts/
```

> **Security warning:** Passing private keys on the command line exposes them
> in shell history and process listings. Prefer `--signing-key-file <path>` or
> the `IRYS_SIGNING_KEY` environment variable instead.

The `--signing-key` is the secp256k1 private key used to sign the genesis block
header. This does not need to be one of the miners' keys.

---

## Dumping Commitments from an Existing Node

Use `dump-commitments` to extract all commitment transactions from a running
(or stopped) node's database. This is useful when migrating to a new genesis --
the exported file is directly usable as input to Workflow B.

### Usage

Run from the node's working directory (where `config.toml` lives):

```bash
CONFIG=config.toml cargo run -p irys-cli -- dump-commitments
```

Or specify a custom output path:

```bash
CONFIG=config.toml cargo run -p irys-cli -- dump-commitments --output /tmp/commitments.json
```

The default output file is `.irys_genesis_commitments.json` in the current
directory.

### What it does

1. **Exports commitments** -- walks the `IrysCommitments` database table and
   writes all commitment transactions as pretty-printed JSON.

2. **Replays all epoch blocks** -- reads every epoch block from the chain
   (genesis through the latest) and feeds each epoch's commitments into the
   `EpochSnapshot` to reconstruct the full ledger state. This captures
   commitments that were included in later epochs, not just genesis.

3. **Logs partition assignments** -- after replaying all epochs, prints the
   current partition hash assignments grouped by miner address:

   ```text
   Replaying 5 epoch blocks (epoch_len=100, chain_height=450)
     Epoch 0 (height 0): 14 commitments
     Epoch 1 (height 100): 3 commitments
     Epoch 2 (height 200): 0 commitments
     Epoch 3 (height 300): 1 commitments
     Epoch 4 (height 400): 0 commitments
   Partition assignments by miner:
     Miner 2Z7NNbu2hgdx9qzoYbLX8YTAAJtR
       [data L0 S0] 6mZBRJ...
       [capacity] 3peqVp...
       [capacity] 8xKm2Q...
     Miner 4WpfDT3q9K2cNbvmrF8xYWJtR
       [data L1 S0] 9fE3xp...
       [capacity] 2Pgf5v...
   ```

### Notes

- The database is opened **read-only**, so it is safe to run against a live node.
- The `config.toml` must match the node's configuration (it provides the epoch
  length, chain ID, and database paths needed for replay).
- If no `CONFIG` env var is set and no `config.toml` exists in the current
  directory, the CLI falls back to testnet defaults, which will not point at
  your production database.

### Using the output with `build-genesis`

The exported JSON file can be fed directly into Workflow B:

```bash
# 1. Dump from the existing node
CONFIG=config.toml cargo run -p irys-cli -- dump-commitments --output commitments.json

# 2. Build a new genesis block from those commitments
CONFIG=config.toml cargo run -p irys-cli -- build-genesis \
  --commitments commitments.json \
  --signing-key <hex-private-key> \
  --output ./genesis-artifacts/
```

> **Security warning:** Passing private keys on the command line exposes them
> in shell history and process listings. Prefer `--signing-key-file <path>` or
> the `IRYS_SIGNING_KEY` environment variable instead.

---

## Output Files

Both workflows produce two files in the output directory:

- `.irys_genesis.json` -- the signed genesis block header
- `.irys_genesis_commitments.json` -- all commitment transactions

The CLI prints the block hash. Record it for peer configuration.

## Inspect Partition Assignments

After building genesis, validate the partition assignments:

```bash
CONFIG=config.toml cargo run -p irys-cli -- inspect-genesis --genesis-dir ./genesis-artifacts/
```

Output:
```text
Genesis Block
  Hash:        4eiVupe...
  Timestamp:   1700000000000
  Difficulty:  115792089237...

Commitments: 14 total (2 stakes, 12 pledges)

Partition Assignments:
  Miner 2Z7NNbu2hgdx9qzoYbLX8YTAAJtR
    [data L0 S0] 6mZBRJ...   (ledger_id=0, slot=0)
    [capacity] 3peqVp...
    [capacity] 8xKm2Q...

  Miner 4WpfDT3q9K2cNbvmrF8xYWJtR
    [data L1 S0] 9fE3xp...   (ledger_id=1, slot=0)
    [capacity] 2Pgf5v...

Summary:
  Miners:              2
  Total partitions:    5
  Capacity partitions: 3
  Data partitions:     2
```

This replays the genesis commitments through the `EpochSnapshot` to show exactly
which miner is assigned to which partition hash, and which partitions are
promoted to data partitions for ledger slots.

## Compare Current Network vs Target Genesis

Before resetting a network onto a newly built genesis, compare the current live
partition state with the target genesis assignments:

```bash
CONFIG=config.toml cargo run -p irys-cli -- compare-genesis \
  --genesis-dir ./genesis-artifacts/
```

To print the complete set of partition hashes that are retained globally
between current network state and target genesis, add:

```bash
  --list-retained-partition-hashes
```

This command:

1. Replays the current network's epoch history from the node database.
2. Replays the target genesis commitments from disk.
3. Computes a per-miner reset plan showing:
   - `Wipe` partitions currently present on that miner but not assigned in the target genesis
   - `Add` partitions the miner will need after the reset
   - `Role change` partitions whose hash stays on the same miner but whose assignment changes between capacity and data (or to a different ledger slot)

This is the report to use when deciding which miners need to wipe local
partition data before restarting on the new genesis.

## Configure Peer Nodes

Add the genesis block hash to every peer node's `config.toml`:

```toml
[consensus]
expected_genesis_hash = "<block-hash>"
```

## Start the Network

1. The genesis block producer starts in `Genesis` mode with a trusted peer
   pointing to itself or with the genesis files on disk.
2. Peer nodes start in `Peer` mode and fetch genesis from the producer (or
   another peer that already has it).

### Offline Bootstrap (Alternative)

Instead of fetching genesis over the network, peers can preload genesis
artifacts locally:

1. Copy the genesis output directory to the peer machine.
2. Run `irys-cli import-genesis --genesis-dir <path-to-genesis-artifacts>` to
   import the genesis block and commitments into the local database.
3. Set `expected_genesis_hash` in the peer's `config.toml` as described above.
4. Start the peer node in `Peer` mode — it will use the preloaded genesis
   instead of fetching it from the network.

## Security Notes

- **`genesis_miners.toml` contains raw private keys.** Treat it as a secret.
  Delete it after genesis block generation.
- **For production,** use Workflow B where miners sign their own commitments
  independently rather than sharing private keys.
- **`expected_genesis_hash` is critical.** It prevents nodes from accepting a
  tampered genesis block. Every peer node must have this set.
- **Miner order is canonical.** When using `--miners`, the CLI sorts miners by
  Irys address before generating commitments, so two coordinators using the
  same miner set will always produce the same genesis block.
