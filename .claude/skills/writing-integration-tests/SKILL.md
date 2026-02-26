---
name: writing-integration-tests
description: Use when writing, modifying, or reading integration tests in crates/chain/tests/, or when needing to understand the IrysNodeTest harness, block production patterns, multi-node setups, or test synchronization utilities
---

# Writing Irys Integration Tests

## Overview

Integration tests live in `crates/chain/tests/`. The harness is `IrysNodeTest` (in `crates/chain/tests/utils.rs`), wrapping a full Irys node for in-process testing. Function names use `heavy_test_` prefix by convention.

For the full `IrysNodeTest` API (all methods for mining, waiting, querying, posting txs, peer networking), **read `crates/chain/tests/utils.rs` directly**.

## Test Structure

```rust
#[test_log::test(tokio::test)]
async fn heavy_test_my_feature() -> eyre::Result<()> {
    initialize_tracing();
    let mut config = NodeConfig::testing();
    // or NodeConfig::testing_with_epochs(n) for epoch-aware tests

    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);

    let node = IrysNodeTest::new_genesis(config).start().await;
    // ... interact, assert ...
    node.stop().await;
    Ok(())
}
```

## Key Patterns

### Node Creation

| Method | Use When |
|--------|----------|
| `IrysNodeTest::new_genesis(config).start().await` | First/only node |
| `IrysNodeTest::new(peer_config).start_with_name("PEER").await` | Peer syncing from genesis |
| `node.start_and_wait_for_packing("NAME", secs).await` | Need packing complete first |
| `genesis.testing_peer_with_signer(&signer)` | Peer config with pre-funded signer |
| `genesis.testing_peer_with_assignments(&signer).await?` | Peer with stake+pledge+partitions |

### Stop & Restart

```rust
let mut stopped = node.stop().await;
stopped.cfg.consensus.get_mut().hardforks.aurora = Some(aurora);
let node = stopped.start().await;
```

### Multi-Node: Peer Staking & Partition Assignment

Fund signers before starting nodes, wait for gossip before mining:

```rust
let peer_signer = IrysSigner::random_signer(&config.consensus_config());
config.fund_genesis_accounts(vec![&peer_signer]);

let genesis = IrysNodeTest::new_genesis(config).start_and_wait_for_packing("GENESIS", 30).await;
let peer = IrysNodeTest::new(genesis.testing_peer_with_signer(&peer_signer))
    .start_with_name("PEER").await;

let stake = peer.post_stake_commitment(None).await?;
let pledge = peer.post_pledge_commitment(None).await?;

// Wait for gossip to reach genesis before mining
genesis.wait_for_mempool(stake.id(), 30).await?;
genesis.wait_for_mempool(pledge.id(), 30).await?;
genesis.mine_blocks(NUM_BLOCKS_IN_EPOCH).await?;
peer.wait_until_height(NUM_BLOCKS_IN_EPOCH as u64, 30).await?;
peer.wait_for_packing(30).await;
```

### Block Events Idle (Subscribe-Before-Act)

Subscribe to events *before* the action, then await after:

```rust
let idle = peer.wait_until_block_events_idle(Duration::from_millis(500), Duration::from_secs(10));
genesis.gossip_block_to_peers(&Arc::new(block))?;
idle.await;
```

## Example Tests by Pattern

Study these as templates when writing new tests:

| Pattern | Example Test | File |
|---------|-------------|------|
| **Single-node block production** | `heavy_test_blockprod` | `block_production/block_production.rs` |
| **Multi-node peer sync** | `heavy_test_p2p_reth_gossip` | `multi_node/sync_chain_state.rs` |
| **Fork recovery & reorg** | `slow_heavy_fork_recovery_submit_tx_test` | `multi_node/fork_recovery.rs` |
| **Epoch transitions + commitments** | `heavy_test_commitments_3epochs_test` | `api/tx_commitments.rs` |
| **Validation rejection (evil strategy)** | `slow_heavy_block_insufficient_perm_fee_gets_rejected` | `validation/data_tx_pricing.rs` |
| **Data upload + chunk migration** | `heavy_data_promotion_test` | `promotion/data_promotion_basic.rs` |
| **Node restart** | `heavy_test_can_resume_from_genesis_startup_with_ctx` | `startup/startup.rs` |
| **Mempool & tx ordering** | tests in | `multi_node/mempool_tests.rs` |

All paths relative to `crates/chain/tests/`.

## Common Pitfalls

- **Fund signers before starting nodes** — genesis accounts are baked into genesis block
- **`wait_for_mempool` before mining on genesis** when txs originate from a peer — gossip takes time
- **Epoch-aware tests need `testing_with_epochs(n)`** — default config has very large epochs
- **Block migration depth** — blocks appear in block index only after `block_migration_depth` confirmations; use `wait_until_block_index_height` or `wait_for_block_in_index`
- **Stop peers before genesis** — avoids connection errors in logs
- **One signer per commitment type** — create arrays of signers for multi-commitment tests
- **`post_data_tx` panics on failure** — use `post_publish_data_tx` for `Result`
- **Subscribe to events before the action** — `wait_until_block_events_idle` and `wait_for_reorg` must be called before the triggering action
- **Block index vs block tree** — `wait_until_block_index_height` only checks index height; `wait_for_block_in_index` checks both index and DB header
