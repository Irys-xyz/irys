---
name: writing-integration-tests
description: Use when writing integration tests for the Irys chain, especially multi-node cluster tests involving block production, peer sync, epoch boundaries, node restarts, or transaction flows
---

# Writing Irys Integration Tests

## Overview

Integration tests live in `crates/chain/tests/`. The core test harness is `IrysNodeTest` (defined in `crates/chain/tests/utils.rs`), which wraps a full Irys node for in-process testing. Tests prefix their function names with `heavy_test_` by convention.

## Test Structure Pattern

Every integration test follows this shape:

```rust
#[test_log::test(tokio::test)]
async fn heavy_test_my_feature() -> eyre::Result<()> {
    initialize_tracing();

    // 1. Configure
    let mut config = NodeConfig::testing();
    // or NodeConfig::testing_with_epochs(num_blocks) for epoch-aware tests

    // 2. Fund accounts
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);

    // 3. Start node(s)
    let node = IrysNodeTest::new_genesis(config).start().await;

    // 4. Interact (post txs, mine blocks, wait for state)
    // 5. Assert
    // 6. Cleanup
    node.stop().await;
    Ok(())
}
```

## IrysNodeTest Lifecycle

### Creating Nodes

| Method | Use When |
|--------|----------|
| `IrysNodeTest::new_genesis(config).start().await` | Starting the first node (genesis mode) |
| `IrysNodeTest::new(peer_config).start_with_name("PEER").await` | Starting a peer that syncs from genesis |
| `node.start_and_wait_for_packing("NAME", secs).await` | Need packing to complete before proceeding |
| `IrysNodeTest::default_async().start().await` | Quick single-node test with defaults |

### Creating Peers from a Running Genesis Node

```rust
// Simple peer (random signer)
let peer_config = genesis_node.testing_peer();
let peer = IrysNodeTest::new(peer_config).start_with_name("PEER").await;

// Peer with a specific signer (needed if you pre-funded them)
let peer_config = genesis_node.testing_peer_with_signer(&peer_signer);
let peer = IrysNodeTest::new(peer_config).start_with_name("PEER").await;

// Peer with partition assignments (stakes, pledges, mines to epoch, waits for packing)
let peer = genesis_node.testing_peer_with_assignments(&peer_signer).await?;
```

### Stopping & Restarting

`stop()` returns `IrysNodeTest<()>` (stopped state) which can be `.start().await` again. Modify `stopped.cfg` between stop/start to change config on restart:

```rust
let mut stopped = node.stop().await;
stopped.cfg.consensus.get_mut().hardforks.aurora = Some(aurora);
let node = stopped.start().await;
```

## Mining & Block Production

| Method | Behavior |
|--------|----------|
| `node.mine_block().await?` | Mine exactly 1 block, returns `IrysBlockHeader` |
| `node.mine_blocks(n).await?` | Mine exactly n blocks |
| `node.mine_until_next_epoch().await?` | Mine until next epoch boundary, returns `(blocks_mined, final_height)` |
| `node.mine_block_with_payload().await?` | Mine 1 block, returns `(header, EthBuiltPayload, BlockTransactions)` |
| `node.mine_block_without_gossip().await?` | Mine without broadcasting to peers |

## Waiting / Synchronization

These are the primary polling utilities. All take `seconds_to_wait: usize` (1 check/sec).

| Method | Use When |
|--------|----------|
| `node.wait_until_height(h, secs).await?` | Wait for canonical chain to reach height h |
| `node.wait_until_height_confirmed(h, secs).await?` | Wait for height h to be confirmed (past migration depth) |
| `node.wait_for_block_at_height(h, secs).await?` | Event-driven wait (no polling) for specific height |
| `node.wait_until_block_index_height(h, secs).await?` | Wait for block index (MDBX) to reach height |
| `node.wait_for_packing(secs).await` | Wait for storage module packing to complete |
| `node.wait_for_mempool(tx_id, secs).await?` | Wait for a tx to be known (checks mempool first, then DB) |
| `node.wait_for_mempool_commitment_txs(ids, secs).await?` | Wait for multiple commitment txs in mempool |
| `node.wait_for_ingress_proofs(tx_ids, secs).await?` | Wait for ingress proofs to appear in DB |
| `node.wait_for_migrated_txs(tx_headers, secs).await?` | Wait for txs to be migrated to block index (MDBX) |
| `node.wait_for_chunk(app, ledger, offset, secs).await?` | Wait for a chunk to be retrievable |
| `node.wait_for_block(hash, secs).await?` | Wait for specific block hash in block tree |
| `node.wait_for_evm_block(hash, secs).await?` | Wait for EVM block in Reth |

## Posting Transactions

### Data Transactions

Two variants with different error handling and routing:

```rust
let anchor = node.get_anchor().await?;
let data = "hello".as_bytes().to_vec();

// Via HTTP API — panics on failure (returns DataTransaction, not Result)
let tx = node.post_data_tx(anchor, data, &signer).await;

// Via mempool directly — returns Result<DataTransaction, AddTxError>
let tx = node.post_publish_data_tx(&signer, data).await?;
```

### Commitment Transactions (Stake/Pledge/etc.)

```rust
// Using the node's own signer
let stake_tx = node.post_stake_commitment(None).await?;
let pledge_tx = node.post_pledge_commitment(None).await?;

// Using a specific signer
let stake_tx = node.post_stake_commitment_with_signer(&signer).await?;

// Custom commitment
node.post_commitment_tx(&commitment_tx).await?;
```

## Querying State

| Method | Returns |
|--------|---------|
| `node.get_canonical_chain_height().await` | `u64` - current tip height |
| `node.get_block_by_height(h).await?` | Block from block tree at height |
| `node.get_block_by_height_from_index(h, include_chunk)?` | Block from MDBX (migrated/confirmed) |
| `node.get_block_by_hash(hash)?` | Block from block tree by hash |
| `node.get_tx_header(tx_id)?` | Data tx header from MDBX |
| `node.get_storage_tx_header_from_mempool(tx_id).await?` | Data tx header from mempool |
| `node.get_balance(addr, evm_hash).await` | EVM balance at specific block |
| `node.get_partition_assignments(addr)` | Partition assignments for miner |
| `node.get_commitment_snapshot_status(tx)` | Commitment status in canonical snapshot |
| `node.get_data_price(ledger, size).await?` | Price info for data storage |
| `node.get_stake_price().await?` | Price info for staking |

## Multi-Node Patterns

### Peer Staking & Partition Assignment

To get a peer that can mine, it needs stake + pledge + epoch boundary:

```rust
// Fund peer signer in genesis config BEFORE starting nodes
let peer_signer = IrysSigner::random_signer(&config.consensus_config());
config.fund_genesis_accounts(vec![&peer_signer]);

let genesis = IrysNodeTest::new_genesis(config.clone())
    .start_and_wait_for_packing("GENESIS", 30).await;

let peer_config = genesis.testing_peer_with_signer(&peer_signer);
let peer = IrysNodeTest::new(peer_config).start_with_name("PEER").await;

// Post stake + pledge from peer
let stake = peer.post_stake_commitment(None).await?;
let pledge = peer.post_pledge_commitment(None).await?;

// Wait for txs to reach genesis mempool (they gossip)
genesis.wait_for_mempool(stake.id(), 30).await?;
genesis.wait_for_mempool(pledge.id(), 30).await?;

// Mine epoch to get partition assignments
genesis.mine_blocks(NUM_BLOCKS_IN_EPOCH).await?;
peer.wait_until_height(NUM_BLOCKS_IN_EPOCH as u64, 30).await?;
peer.wait_for_packing(30).await;
```

### Verifying Chain Consistency Between Nodes

```rust
for height in 1..=final_height {
    let genesis_block = genesis.get_block_by_height(height).await?;
    let peer_block = peer.get_block_by_height(height).await?;
    assert_eq!(genesis_block.block_hash, peer_block.block_hash,
        "Block hash mismatch at height {}", height);
}
```

### Gossip Control

```rust
// Mine without broadcasting
let (header, payload, txs) = node.mine_block_without_gossip().await?;

// Manually send block to specific peer
node.send_block_to_peer(&peer, sealed_block).await?;
// or full block with txs + EVM payload:
node.send_full_block(&peer, &header, payload, txs).await?;
```

### Peer Discovery / Handshakes

```rust
node_a.announce_to(&node_b).await?;
IrysNodeTest::announce_between(&node_a, &node_b).await?;
IrysNodeTest::announce_mesh(&[&node_a, &node_b, &node_c]).await?;
node_a.wait_until_sees_peer(&target_addr, 100).await?;
```

### Reorg Detection

```rust
let reorg_future = node.wait_for_reorg(30);
// ... trigger competing block ...
let reorg_event = reorg_future.await?;
```

## Configuration Helpers

| Helper | Purpose |
|--------|---------|
| `NodeConfig::testing()` | Default single-node test config |
| `NodeConfig::testing_with_epochs(n)` | Config with n blocks per epoch |
| `config.fund_genesis_accounts(vec![&signer])` | Pre-fund accounts in genesis |
| `config.consensus.get_mut()` | Mutate consensus config (chunk_size, hardforks, etc.) |
| `IrysSigner::random_signer(&config.consensus_config())` | Create a random funded signer |

## Common Pitfalls

- **Always fund signers before starting nodes** - genesis accounts are baked into genesis block
- **Use `wait_for_mempool` before mining on genesis** when txs originate from a peer - gossip takes time
- **Epoch-aware tests need `testing_with_epochs(n)`** - default config has very large epochs
- **Block migration depth** - blocks only appear in block index after `block_migration_depth` confirmations; use `wait_until_block_index_height` to wait
- **Stop nodes in correct order** - stop peers before genesis to avoid connection errors in logs
- **Each signer can only be used once per commitment type** - create arrays of signers for multi-block tests
- **`mine_blocks(n)` enables then disables mining** - it's deterministic, not concurrent with the VDF
- **`post_data_tx` panics on failure** - use `post_publish_data_tx` if you need error handling (returns `Result`)
- **`consensus.get_mut()` panics on non-Custom variants** - only works after `NodeConfig::testing()` which creates a `Custom` variant
