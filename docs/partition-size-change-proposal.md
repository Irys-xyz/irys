# Smaller partitions and target-based capacity allocation

**Status:** Proposal. Hardware burn-in is assumed complete. Activation time, any ID-supply floor above the byte-preserving minimum, and any extra ingress reserve remain to be agreed.

Reduce partition size while preserving existing partition IDs and their packed prefixes. Mainnet has no ledger data, and all historical winning chunks are entropy chunks. This allows existing partitions to be truncated and the freed disk space to be packed under new IDs.

## Parameters

| Parameter | Proposed value |
| --- | --- |
| Chunks per partition | 14,500,000 |
| Chunk size | 262,144 bytes |
| Partition size | 3.801088 TB |
| HDD recall range | 80 contiguous chunks per VDF step |
| Sampling pass | 181,250 steps, approximately 50.35 hours at one step/second |
| Epoch length | Keep 7,200 blocks, nominally 24 hours |
| Ingress planning target | 20,000,000 bytes/second per active ledger |

Six partitions occupy 22.807 TB and require 120 MiB/s of mining reads at one VDF step/second. Sizes use decimal TB; read bandwidth uses binary MiB.

## Ledger allocation

Replace the half-partition threshold and previous-epoch growth forecast with reservation coverage plus a configured reserve. At each epoch boundary, include every reservation through that block, whether or not its chunks have arrived:

```text
epoch_seconds = blocks_per_epoch × target_block_seconds
reserve_chunks = ceil(target_ingress_bytes_per_second × planning_seconds / chunk_bytes)
required_slots = ceil((reserved_ledger_chunks + reserve_chunks) / partition_chunks)
slots_to_add = max(0, required_slots - total_allocated_slots)
```

Initially, `planning_seconds = epoch_seconds`; an agreed extra reserve can extend this horizon. One nominal epoch at 20 MB/s requires 6,591,797 chunks, approximately 1.728 TB. This is a planning assumption; slower epochs can consume more reserve.

Apply the rule independently to every active ledger, including genesis and newly activated ledgers. Use checked integer arithmetic and consensus parameters. Keep expired slots in `total_allocated_slots`: ledger offsets are cumulative and expired addresses are not reused.

A transaction reserves its entire logical range when included. Its chunks may arrive across multiple epochs. Reservations that exceed the target-based reserve must trigger enough additional slots to cover all reserved offsets plus the reserve. The ingress target sets spare capacity; it does not cap reservations, slot allocation, or aggregate throughput.

For example, 100 transactions of 5 TB each reserve 500 TB. Starting with an empty ledger, the next epoch allocation requires 132 slots (approximately 501.744 TB), covering those reservations plus the nominal one-day reserve of 1.728 TB. The protocol does not wait for all 500 TB of chunks to arrive before allocating those slots.

**Known limitation:** Current ingress proofs require each signer to cover the entire transaction data root. A node that validates every chunk can generate such a proof across multiple storage modules; the missing capability is combining different miners' proofs for their portions of a transaction spanning multiple slots. Every 5 TB transaction exceeds the proposed 3.801 TB partition size, and smaller transactions can also cross a slot boundary. The [multi-slot ingress proposal](multi-slot-ingress-proposal.md) has each miner generate a partial ingress proof for the transaction chunks within its assigned slot, and requires enough proofs from distinct assigned miners for every transaction/slot intersection before promotion. Until implemented, the example above describes allocation rather than support for promotion using partial coverage.

**Resize implementation scope:** Transactions spanning partition/slot boundaries are unsupported and must not be implemented or tested in the resize task. Partial ingress proofs are separate future work. Use synthetic aggregate ledger counters or transactions contained within individual slots for allocation and ingress tests.

Once partitions are assigned, chunks can be written to multiple slots concurrently. Aggregate ingress can exceed the target when those partitions have sufficient independent disk and network bandwidth. Filling slots with replicas still requires sufficient eligible pledged capacity; allocating slots alone does not supply the storage.

## Preserve packing and expand partition supply

For every existing partition, retain its ID, packing address, pledge binding, and chunks at offsets `0..14_500_000` (exclusive end). Packing inputs and packing iterations remain unchanged; partition size is not an input to entropy generation.

A fully packed old partition releases approximately 16 TB. The retained prefix remains usable, and additional partitions receive new IDs and fresh packing.

At activation, derive a byte-preserving minimum total partition count from the complete active ID inventory on E's parent branch, including assigned and unassigned IDs:

```text
preserved_id_floor = ceil(pre_activation_id_count × old_partition_chunks / new_partition_chunks)
required_total_ids = max(preserved_id_floor, configured_id_floor, ongoing_policy_total_ids)
```

Append IDs using the existing deterministic hash sequence until supply reaches this requirement. Approximately 5.209 times the old ID count preserves aggregate nominal byte capacity: 1,000 old IDs require at least 5,210 new-size IDs, including the retained 1,000. An optional configured floor may increase this requirement. Preserve or reconstruct the activation baseline from ancestry; later epochs, replay, and restart must not multiply the expanded count again.

Make the ongoing capacity-reserve policy account for partition size as well: use storage demand and reserve budgets in bytes or fixed reference-size units, then convert the reserve budget into current-size partitions. Equivalent byte demand must retain the intended reserve budget subject to rounding. Preserve the original policy for pre-fork blocks.

New IDs follow the existing pledge-assignment rules. Peers provision additional storage-module paths for the new assignments. Creating IDs preserves nominal protocol capacity; actual packed capacity recovers as the new partitions are pledged and packed. The existing `num_capacity_partitions` setting only affects genesis and cannot perform this mainnet expansion.

## Hardfork boundary

Following Cascade's convention, activation block **E** is the first positive-height epoch block whose timestamp meets or exceeds the partition-resize fork's activation timestamp. Require empty data ledgers at activation; mainnet's separate data-transaction prohibition outside master protects this rollout assumption.

1. **E is mined under the old proof rules.** Its epoch tasks establish the new geometry, target-based allocation, and expanded ID supply.
2. **E publishes the difficulty for its children.** Apply the normal old-regime adjustment if due, then increase the winning-hash count by a factor of five, capped at the full hash space, to account for recalls falling from 400 to 80. Reset the adjustment timestamp and window origin to E, with subsequent adjustments at E + n × interval. Producer, validator, and cumulative-weight accounting must agree on this transition.
3. **Children of E use the new geometry.** Start a fresh sampling pass at `E.global_step_number + 1`. Rebuild miner sampling state and discard queued solutions produced under the old rules. Resolve activation and sampling origin from the block's ancestry, including during reorgs.
4. **After E is finalized, truncate physical storage.** Quiesce packing and writes, clip persisted intervals while preserving retained packed status, truncate files, and resume packing. The migration must be restart-safe and idempotent. Do not use the existing storage-module reset operation, which clears packed status.

Normal difficulty adjustment follows the increase in mining work as new partitions become packed. Epoch length and retention durations stay unchanged.

## Historical validation

Pre-fork blocks retain their original recall schedule and offset eligibility. An old winning offset above 14,500,000 remains valid for its historical block.

Entropy can be regenerated from the historical miner, partition ID, offset, chain ID, and packing parameters without the original partition file. Winning chunks are also stored separately in the block database. Mainnet's historical entropy proofs compare the regenerated entropy directly with the PoA chunk; they do not require a data-root proof.

## Implementation and acceptance

Primary changes are in consensus/hardfork configuration, `EpochSnapshot` allocation and ID supply, sampling/mining and proof validation, difficulty handling, and storage-module migration. Preserve historical parameter resolution rather than replacing the original mainnet constants globally.

Required tests:

- Retained IDs, pledge bindings, and packed bytes survive truncation and restart.
- Interrupted migration resumes safely; old packing jobs cannot recreate the tail.
- Target allocation handles exact boundaries, large reservations, expiry, new ledgers, and insufficient replicas.
- A synthetic aggregate reservation frontier of 500 TB allocates all required slots plus the reserve at the next epoch boundary, even before chunks arrive. Do not construct 5 TB transactions for this test. Concurrent ingress tests use separate transactions contained within their respective slots and verify that the planning target is not a rate limit.
- ID expansion preserves aggregate nominal bytes, is deterministic, and does not repeat on replay or subsequent epochs. The ongoing reserve policy preserves its intended byte budget for equivalent storage demand, subject to rounding.
- Reorgs across E restore the correct geometry, sampling origin, and difficulty.
- A fresh node validates the complete chain, including old high-offset winners, after partition tails have been deleted.
