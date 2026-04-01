# Multi-Miner Genesis Setup

This guide walks through creating a genesis block for a multi-miner Irys network.

## Prerequisites

- `irys-cli` binary built from the `irys-rs` repo
- A `config.toml` with the desired consensus configuration
- Each miner's hex-encoded secp256k1 private key

## Step 1: Collect Miner Keys

Each miner generates or provides their mining key. Use `generate-miner-info` to
derive their addresses:

```bash
irys-cli generate-miner-info --key <hex-private-key>
```

Output:
```
Mining key:   f57554aff54acd4...
Irys address: 2Z7NNbu2hgdx9qzoYbLX8YTAAJtR
EVM address:  0x81c23e4bde4c7086400cdcbca2dfe9a96dbd0fad
```

Record each miner's **EVM address** for Step 3 and their **mining key** for
Step 2.

## Step 2: Create `genesis_miners.toml`

```toml
[[miners]]
mining_key = "f57554aff54acd4cfaa084f45a7062d5869c8dbb789f7d6a883fade660960303"
pledge_count = 8

[[miners]]
mining_key = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0"
pledge_count = 3
```

**Notes:**
- `pledge_count` is the number of storage partitions this miner commits to store.
  Each miner also gets one implicit stake commitment.
- Miner order in the TOML does not affect the output -- miners are sorted by
  their derived Irys address before commitment generation.
- Every miner must have `pledge_count >= 1`.
- No two miners may share the same key.

## Step 3: Fund Miner Accounts

In the node's `config.toml`, add genesis account balances for each miner under
`[consensus.reth.alloc]`. Each miner needs enough balance to cover their stake
plus all pledge fees.

```toml
[consensus.reth.alloc."0x81c23e4bde4c7086400cdcbca2dfe9a96dbd0fad"]
balance = "10000000000000000000000000"

[consensus.reth.alloc."0xAnotherMinerEvmAddress"]
balance = "10000000000000000000000000"
```

The balance must be >= the sum of the miner's stake value + (pledge_count *
pledge_base_value), denominated in the chain's smallest token unit.

## Step 4: Build the Genesis Block

```bash
CONFIG=config.toml irys-cli build-genesis \
  --miners genesis_miners.toml \
  --output ./genesis-artifacts/
```

This produces two files in `./genesis-artifacts/`:
- `.irys_genesis.json` -- the signed genesis block header
- `.irys_genesis_commitments.json` -- all commitment transactions

The CLI prints the block hash. Record it for Step 5.

## Step 5: Configure Peer Nodes

Add the genesis block hash to every peer node's `config.toml`:

```toml
[consensus]
expected_genesis_hash = "<block-hash-from-step-4>"
```

## Step 6: Start the Network

1. The genesis block producer starts in `Genesis` mode with a trusted peer
   pointing to itself or with the genesis files on disk.
2. Peer nodes start in `Peer` mode and fetch genesis from the producer (or
   another peer that already has it).

## Security Notes

- **`genesis_miners.toml` contains raw private keys.** Treat it as a secret.
  Delete it after genesis block generation. For mainnet, a ceremony where
  miners sign their own commitments independently is recommended.
- **`expected_genesis_hash` is critical.** It prevents nodes from accepting a
  tampered genesis block. Every peer node must have this set.
- **Miner order is canonical.** The CLI sorts miners by Irys address before
  generating commitments, so two coordinators using the same miner set will
  always produce the same genesis block.
