# Multi-Miner Genesis Setup

This guide covers creating a genesis block for a multi-miner Irys network.

There are two workflows:

- **From mining keys** -- generate commitments from private keys (for testing)
- **From existing commitments** -- package pre-signed commitments into a genesis
  block (for production)

## Prerequisites

- `irys-cli` binary built from the `irys-rs` repo
- A `config.toml` with the desired consensus configuration

## Helper: Derive Addresses from a Mining Key

Use `generate-miner-info` to derive a miner's Irys and EVM addresses:

```bash
irys-cli generate-miner-info --key <hex-private-key>
```

Output:
```
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

The balance must be >= stake value + (pledge_count * pledge_base_value),
denominated in the chain's smallest token unit.

### 3. Build the Genesis Block

```bash
CONFIG=config.toml irys-cli build-genesis \
  --miners genesis_miners.toml \
  --output ./genesis-artifacts/
```

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
address.

### 3. Build the Genesis Block

```bash
CONFIG=config.toml irys-cli build-genesis \
  --commitments commitments.json \
  --signing-key <hex-private-key-for-block-signature> \
  --output ./genesis-artifacts/
```

The `--signing-key` is the secp256k1 private key used to sign the genesis block
header. This does not need to be one of the miners' keys.

---

## Output Files

Both workflows produce two files in the output directory:

- `.irys_genesis.json` -- the signed genesis block header
- `.irys_genesis_commitments.json` -- all commitment transactions

The CLI prints the block hash. Record it for peer configuration.

## Inspect Partition Assignments

After building genesis, validate the partition assignments:

```bash
CONFIG=config.toml irys-cli inspect-genesis --genesis-dir ./genesis-artifacts/
```

Output:
```
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
