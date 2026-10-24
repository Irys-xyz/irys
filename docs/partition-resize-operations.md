# Estuary: partition resize

The resize is the Estuary hardfork (`estuary` in the consensus configuration), optional and scheduled independently of Delta. Mainnet activation remains **unscheduled**. Keep the original top-level partition and recall parameters: they are needed for genesis, historical validation, and the physical layout of existing storage modules.

For the eventual coordinated network schedule, add this table to the consensus configuration with the agreed activation timestamp:

```toml
[hardforks.estuary]
activation_timestamp = "<RFC 3339, e.g. 2027-01-01T00:00:00+00:00>"  # rollout value, not yet agreed
partition_chunks = 14500000
recall_chunks = 80
target_ingress_bytes_per_second = 20000000
minimum_partition_ids = 0
```

The table path above is relative to the consensus configuration. `minimum_partition_ids = 0` means no additional configured floor; the protocol still preserves the entire pre-activation ID pool's nominal byte capacity automatically. Omitting `estuary` leaves the fork disabled; Delta's slot seeding is unaffected either way. Do not choose a new timestamp independently of the network.

## Activation

Like Cascade, E is the first positive-height epoch block whose timestamp meets or exceeds Estuary's `activation_timestamp`. Crossing the timestamp within an epoch does not change that epoch's geometry. All data ledgers must have zero reserved chunks at E. While Estuary is scheduled but not yet active on a branch, every block must carry empty data ledgers: validators reject blocks whose ledgers hold chunks, and the producer leaves data transactions and promotions out of its blocks, so E can always be built. Transactions anchored within `tx_anchor_expiry_depth` blocks of E are included from E + 1; older ones expire and must be re-posted. A node refuses to start when Estuary is scheduled and its chain already holds ledger data, since activation could never succeed there.

E's proof uses the old 75,534,400-chunk partitions and 400-chunk recall ranges. Its difficulty applies the normal old-regime adjustment if due, then a fivefold increase in winning hashes, capped at the full hash space. E resets the difficulty timestamp and starts a new adjustment window. Subsequent adjustments occur at E + interval, E + 2 × interval, and so on; a partial old window is not treated as a full new window. Cumulative difficulty is credited per partition-step: E and its descendants add `old recall / new recall` times the hash-count increment, so a branch that keeps mining under the old recall range gains no fork-choice advantage over the activated branch at equal capacity. E's children use 14,500,000-chunk partitions and 80-chunk recall ranges, with a new sampling pass starting at E's VDF step plus one. Epoch length, chunk size, retention settings, packing iterations, and entropy inputs stay unchanged.

Each active ledger receives enough cumulative slots for its reservations plus one nominal epoch of target ingress. Expired slot indexes remain part of cumulative addressing. Reservations can exceed the target, and eligible pledged replicas are assigned through the existing rules. The ingress target is not a throughput limit.

The activation snapshot records its original ID inventory and sampling origin. Replay reconstructs them; later epochs do not expand the same inventory again. Forks carry their own activation snapshots, so mining and validation follow the selected branch's geometry.

## Storage and restart

Deploy the fork-aware binary and agreed consensus schedule before E. Keep existing module directories and `packing_params.toml` files. Existing partition IDs, miner addresses, pledge bindings, and prefix packing remain usable. Normal assignment and expiry rules continue to apply.

Logical bounds change when the canonical branch activates. Physical tails remain until E reaches the existing finalized frontier, computed from the confirmed block index and the block-tree/migration depths. The storage service captures the finalized activation and runs disk work on a blocking worker, outside the storage-module registry lock. The versioned `partition_resize.json` records the chain, activation height/hash, and old/new chunk counts. The worker serializes against chunk flushes, drops pending suffix writes, clips persisted intervals, and truncates the suffix of each submodule file. Prefix bytes and packing status are retained. Packing workers stop scheduling suffix work; late writes cannot recreate the tail.

If interrupted, reopening the module validates the journal against the configured chain and sizes and completes the shrink before accepting writes. Keep the journal with the module. Do not reset or delete interval files to migrate. Before finality a reorg can restore the old logical bounds; after finality the removed tail cannot be restored by rolling back the binary or schedule. Historical entropy PoA validation still regenerates old high-offset chunks independently of partition files; stored block PoA chunks are retained.

## Additional partitions

The ID pool preserves nominal capacity, not already-packed capacity. For example, 1,000 old IDs become at least 5,210 total IDs. Original IDs stay in place; the additional IDs are appended through the existing deterministic sequence.

After disk space is released, append fresh directory paths to `.irys_submodules.toml` and restart through the existing storage provisioning workflow. Preserve existing path ordering and directories. Obtain the required additional pledges through the usual process; the resize does not create free pledges. Newly assigned IDs receive fresh packing. Six fully packed new partitions occupy 22.806528 decimal TB before filesystem and database overhead.

## Capacity reporting

`GET /v1/storage/capacity` reports the active `geometry` (partition chunks, recall chunks, sampling origin), chunk bytes, target ingress, epoch reserve, and total partition-ID supply. Network allocation is captured with the canonical tip and epoch snapshot.

Each ledger reports reservations, cumulative allocated slots and headroom, required/assigned replicas for nonexpired slots, and fully assigned slots. The nested `local` counts and top-level `local` total describe only this node: packed chunks include observed entropy and data, while packed writable chunks count entropy on ledger-assigned modules accepting data writes. Packed capacity-pool entropy is not reported as writable ledger capacity. Local counters are observational, include pending writes, and may advance during a request; they are not a network-wide durability or throughput guarantee. Assignments that disagree with the captured epoch are excluded. Targets and epoch reserves are `null` before activation.

## Scope

Transactions spanning partition/slot boundaries are not supported by this change. Partial ingress proofs, multi-slot proof aggregation, and related reward changes remain future work. Resize tests use synthetic reservation counters or transactions contained within individual Submit and Publish slots. SSD and scattered sampling support are outside this fork.
