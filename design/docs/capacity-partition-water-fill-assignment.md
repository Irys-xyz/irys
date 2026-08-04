# Water-Fill Selection for Capacity Partition Assignment

## Status

Proposed. Requires thorough review before any implementation.

An earlier design for the same defect — a per-address holdings cap plus budgeted eviction — was
considered and dropped; it is recorded under Rejected alternatives.

## Context

The capacity partition pool should hold as many distinct miner addresses as possible. Data ledger
slots can only reach a full replica set if enough distinct miners hold capacity, because
`process_slot_needs` (`crates/domain/src/snapshots/epoch_snapshot/epoch_snapshot.rs:651`) drops
every remaining partition of a chosen miner from the slot's candidate view (`:706`). The achievable
replica count for a slot is therefore:

```
min(distinct miner addresses holding capacity, num_partitions_per_slot)
```

Nothing in the epoch system prioritises that diversity.

### The gap

`assign_partition_hashes_to_pledges` (`:1084`) pairs unassigned partition hashes with active pledges
that have `partition_hash: None`. Both sides are sorted for determinism — hashes ascending (`:1092`),
pledges by `PledgeEntry.id` (`:1115`) — then popped front to front.

`PledgeEntry.id` is a commitment transaction id, a hash. Ordering is therefore a lottery in which
each pending pledge is one ticket. An address with 40 pending pledges holds 40 tickets against a
newly staked miner's one. Diversity is not a factor at any point.

Slot-level diversity is a consumer-side filter that degrades rather than corrects: when the filtered
candidate view empties, `process_slot_needs` emits a `warn!` and leaves the slot under-replicated
(`:689`). It never feeds back into who owns capacity.

### Three factors push against diversity

1. **The cost curve rewards depth over breadth.**
   `calculate_pledge_value_at_count` (`crates/types/src/commitment_v2.rs:102`) applies
   `pledge_base_value.apply_pledge_decay(count, pledge_decay)` with `pledge_decay = 0.9`
   (`crates/types/src/config/consensus.rs:794`, `:796`). At mainnet values a marginal pledge from an
   address already holding 40 costs about 495 IRYS against 14,000 for its first — a factor of 28.
2. **Supply is logarithmic, so the pool is small and contested.** `get_num_capacity_partitions`
   (`:726`) returns `ceil(trunc3(log10(base)) * capacity_scalar)` with `capacity_scalar = 100` —
   about 300 partitions at 1,000 data partitions, about 400 at 10,000.
3. **Concentration does not self-heal.** `return_expired_partition_to_capacity` (`:779`) reuses the
   existing `PartitionAssignment` and clears only `ledger_id` / `slot_index`, preserving
   `miner_address`. An expiring data slot returns its partitions to the same miner as capacity. The
   only routes into the unowned `unassigned_partitions` pool are newly minted capacity
   (`add_capacity_partitions`, `:756`) and voluntary `apply_unpledges` (`:987`).

Factor 3 bounds what any assignment rule can achieve: assignment governs the flow of unowned
capacity, never the stock already held.

## Decision

When the capacity pool cannot serve every pending pledge, serve the signers holding the fewest
capacity partitions. Otherwise behave exactly as today.

The change is confined to how `assign_partition_hashes_to_pledges` builds its pledge list. The sorted
`unassigned_parts` queue, the `pop_front`, and the `PartitionAssignment` insert all stay.

Note that which hash pairs with which pledge is irrelevant to diversity but is *not* irrelevant to
consensus: the loop pops hashes from a sorted queue, so pledge order determines the pairing, and the
pairing determines `capacity_partitions`, `get_hash`, and — through `backfill_missing_partitions` —
which miner ends up in which data slot. Keeping the pairing rule untouched wherever diversity does
not require changing it is therefore a deliberate constraint on this design, not an aesthetic
preference. See Consequences.

### Two decisions, two keys

The current single sort does two unrelated jobs: it decides **who is served** when supply runs out,
and it decides **which hash** each winner receives. Only the first bears on diversity. The rule
therefore uses a holdings-based key to select, then restores the existing txid key to order:

1. **Select** — sort by `(level, id)` and truncate to the number of available hashes.
2. **Order** — re-sort the winners by `id` alone.
3. **Assign** — the existing loop, unchanged.

When supply meets demand the truncation is a no-op, so the winner set is every pending pledge and
step 2 reproduces `sort_unstable_by_key(|a| a.id)` exactly. Uncontended epochs are therefore
byte-identical to today through the same code path, with no conditional and no second branch. See
Consequences for why this property carries most of the deployment argument.

### The selection key: fill level

Selection needs no priority queue and no re-sort per assignment. For a signer starting at `n`
capacity partitions, its k-th pending pledge is the one that raises its holdings from `n+k-1` to
`n+k`. Stamp each pending pledge with the level it fills, then sort once:

```rust
// Capacity partitions currently held, per signer.
let held: BTreeMap<IrysAddress, usize> = self
    .partition_assignments
    .capacity_partitions
    .values()
    .fold(BTreeMap::new(), |mut acc, pa| {
        *acc.entry(pa.miner_address).or_default() += 1;
        acc
    });

// Within a signer, pending pledges claim consecutive levels in txid order.
unassigned_pledges.sort_unstable_by_key(|p| p.id);
let mut next: BTreeMap<IrysAddress, usize> = BTreeMap::new();
let mut keyed: Vec<(usize, IrysTransactionId, PledgeEntry)> = unassigned_pledges
    .into_iter()
    .map(|p| {
        let k = next.entry(p.signer).or_default();
        let level = held.get(&p.signer).copied().unwrap_or(0) + *k;
        *k += 1;
        (level, p.id, p)
    })
    .collect();

// 1. select by holdings
keyed.sort_unstable_by_key(|(level, id, _)| (*level, *id));
keyed.truncate(unassigned_parts.len());

// 2. order by the legacy key
let mut winners: Vec<PledgeEntry> = keyed.into_iter().map(|(_, _, p)| p).collect();
winners.sort_unstable_by_key(|p| p.id);
```

Sorting by `(level, id)` selects the same set as "serve the lowest current holder, recompute after
each assignment". That identity is what keeps selection a single pure function instead of an
incremental loop.

Water-filling is equivalently phrased as "find the level `L` at which supply is exhausted, serve
every pledge below `L`, break the boundary tie by txid". Sort-and-truncate is the same result with
less machinery.

Worked example. X holds 42 with 2 pending, Y holds 0 with 3 pending, Z holds 1 with 1 pending; four
partitions available.

| level | pledge |
| --- | --- |
| 0 | Y#1 |
| 1 | Y#2, Z#1 — tie, broken by txid |
| 2 | Y#3 |
| 42 | X#1 |
| 43 | X#2 |

Y and Z are selected and take all four hashes, in txid order among themselves. X receives nothing
until the field reaches 42. Had five partitions been available, nothing would be truncated and the
result would match today's assignment exactly, X included.

### Capacity-only counting

`held` counts capacity partitions, not data partitions. When a miner's capacity partition is promoted
into a data slot it leaves `capacity_partitions` and stops counting, so the miner rejoins the queue
at its remaining capacity count.

This is the intended reading of the goal: a promoted partition no longer occupies the capacity pool,
so it should not weigh against its holder. Counting data partitions instead would penalise miners for
holdings that `process_slot_needs` assigns to them at random (`:697`) rather than by choice, starving
the miners carrying the network's data.

### Tie-breaking

Ties within a level break on `PledgeEntry.id`, preserving the current secondary key. Address order
was rejected: an address is ground once and then confers permanent priority, whereas a txid tie-break
only buys position inside the signer's own level.

### Why replay makes contention the load-bearing property

Epoch snapshots are not persisted. Every node rebuilds them at startup by replaying every epoch block
from genesis: `chain.rs:1739` → `block_tree.rs:335` → `replay_epoch_data` (`:163`), which re-runs
`perform_epoch_tasks` per epoch block. Any assignment change is therefore retroactive by default.

Retroactive divergence is not a bookkeeping concern. A different pledge→hash pairing gives a different
`miner_address` per hash, so `backfill_missing_partitions` seats different miners in data slots — the
RNG draws the same hash from the same hash-sorted list, but the attached miner differs and the
same-miner `retain` (`:706`) then filters differently, compounding across slots. `slot_index` per
partition diverges, and PoA prevalidation computes `ledger_chunk_offset` from
`get_data_partition_assignment(...).slot_index` (`crates/actors/src/block_validation.rs:4555`-`:4574`),
so a node would reject historical blocks that were valid when produced. Packing entropy is keyed to
`miner_address`, so a shifted mapping also forces repacks.

This is precisely what the select-then-order structure avoids. An epoch in which supply met demand
replays identically, because the selection is total and the ordering is the legacy key. The rule is
inert on all history unless some past epoch was contended — which is an empirical question, not an
assumption (see Open Questions 1).

### Activation

Mainnet and testnet have been measured to contain no contended epoch (Open Questions 1), so nothing in
their history changes and the retroactive failure mode above is not reachable on them. The remaining
exposure is entirely prospective: ungated, upgraded and non-upgraded nodes diverge at the first
contended epoch, whose timing is set by miners choosing to pledge rather than by the maintainers.

Gating is therefore recommended but is a rollout-confidence decision rather than a correctness
requirement, given the uncontended identity above. A new epoch-aligned entry in `IrysHardforkConfig`
(`crates/types/src/hardfork_config.rs:16`) following the `Borealis` / `Cascade` pattern (`:81`, `:94`),
checked against the epoch block's timestamp exactly as `cascade_active` is (`:312`,
`is_cascade_active_at` at `hardfork_config.rs:198`), converts an externally triggered split into a
scheduled one. Naming is deferred to the maintainers.

The fork should be its own entry rather than an extension of `next_name_tbd` (`:22`), which already
carries unrelated ingress-proof parameters. There is no pending entry to attach to: `next_name_tbd` is
`None` and `cascade` activates `2026-08-03T12:00:00+00:00` (`crates/types/src/config/consensus.rs:809`-`:843`).

If gated, the pre-activation path is `sort_unstable_by_key(|a| a.id)` with no truncation. Note that
`try_genesis_init` (`:458`) also calls `assign_partition_hashes_to_pledges` (`:498`); a timestamp gate
covers it automatically, since a genesis block predates any activation timestamp. Ungated, genesis is
covered by the uncontended identity instead, provided the capacity partitions minted at genesis
(`:484`) are not fewer than the genesis pledge count — fixed and checkable per chain.

### Determinism

No new machinery. `capacity_partitions` is a `BTreeMap`
(`crates/domain/src/snapshots/epoch_snapshot/partition_assignments.rs:13`), so the `held` scan is
canonical; the level index comes from a txid sort; `unassigned_partitions` is already sorted before
assignment (`:1092`).

The rule deliberately avoids depending on `Vec<PledgeEntry>` order inside
`CommitmentState::pledge_commitments` (`crates/domain/src/snapshots/epoch_snapshot/commitment_state.rs:32`).
Holdings come from `capacity_partitions`, not from counting pledge entries, so whether that `Vec` is
in stable insertion order after `compute_commitment_state` (`:907`) and after `replay_epoch_data`
(`:163`) does not affect the outcome.

## Consequences

**Uncontended epochs are byte-identical to today.** Not "equivalent" — the same code path produces
the same pledge→hash pairing, because a total selection composed with the legacy sort *is* the legacy
sort. This is the design's most valuable property: it is what keeps the rule inert across history and
what makes activation a scheduling choice rather than a correctness requirement.

**New capacity concentrates only when uncontested.** Whenever an under-served address has pending
demand, a dominant holder is throttled to the network-minimum fill rate. When no under-served address
wants a partition, the surplus is still assigned. Utilisation is unchanged.

**The rule fires as a cliff, not a gradient.** One pledge beyond supply flips the epoch from the
legacy pairing to the water-filled one for every assignment in it, not just the marginal one. This
follows from truncation being all-or-nothing and is acceptable because the diversity benefit is only
needed at exactly that boundary.

**A newcomer converges to the field, not to a quota.** In a contended epoch a newly staked miner with
pending pledges is selected ahead of every deeper holder until it reaches their level. This delivers the
self-healing intent of a holdings cap, applied to flow rather than to stock.

**Holdings stay unbounded.** No cap exists, so an address alone on the network still acquires the
whole pool. Combined with the no-self-heal property above, that position persists once competitors
arrive — they are served first from *new* capacity, but nothing reclaims the stock. Accepting this is
the deliberate trade for removing the eviction path.

**Concentration predating activation is not drained.** Mainnet-beta at epoch 2037600, measured via
`/v1/epoch/current/partition-assignments` and per-miner `/v1/ledger/{miner}/assignments`, holds one
address at 42 capacity partitions against two others at 6 each, with roughly 77 partitions unassigned.
Under this rule those 77 go to miners 4 and up as they stake and pledge, and the existing 42 remain.

**No repack is ever forced.** Nothing reassigns a held partition, so entropy keyed to `miner_address`
is never invalidated by this rule. The storage module reassignment path is untouched.

**This does not fix today's 3/10 replication.** Current under-replication on mainnet-beta is caused by
three miners existing against `num_partitions_per_slot = 10`. Ordering cannot manufacture miners.

**Multi-identity front-running is acceptable.** Under contention the rule gives newcomers absolute
priority, so an entity splitting across identities front-runs the queue. This is already handled economically: a stake
costs 400,000 IRYS against a 14,000 IRYS first pledge (`consensus.rs:792`, `:794`), so each additional
identity costs roughly 29 first-pledges before it holds anything. The ordering rule taxes concentration
in time while the decay curve taxes diversity in money; expensive stakes with relatively cheap pledges
is the intended balance. No identity rule is attempted.

**Most test and devnet configs are unaffected.** Single-miner configurations are unaffected regardless
of contention: one signer means one level sequence, which is the txid sequence. Multi-miner tests are
unaffected unless they run a contended epoch. Only tests that both post more pending pledges than the
pool holds and assert on the resulting distribution will shift.

## Rejected alternatives

**Per-address cap with budgeted eviction** — cap each address at `C = max(1, P/R)` where `P` is the
contested pool and `R` the required replica count, gated at assignment, plus eviction of one partition
per over-cap address per epoch to drain concentration predating activation. Guarantees that an exhausted
pool is held by at least `R` distinct miners.

Rejected as more consensus surface than the goal justifies. The cap delivers a floor of `R` distinct
holders rather than maximising distinct holders, and eviction carries the real cost: the trigger is
reachable by an adversary posting one cheap refundable pledge, entropy is keyed to `miner_address` so
each eviction forces the victim into a full repack (up to 20 TB at 1,000,000 iterations per chunk,
`consensus.rs:788`), and it requires the storage module reassignment path to handle a partition moving
between miners. Water-filling delivers the diversity goal on new capacity with no eviction, no new
parameter, and no repack — at the cost of leaving existing concentration in place.

**Unconditional water-fill ordering** — sort by `(level, id)` and assign in that order, with no
truncate-and-re-sort step. Rejected because it permutes the pledge→hash pairing in *every* epoch,
including uncontended ones where the participation set is unchanged and diversity is already maximal.
That makes the rule retroactive against replayed history for no benefit, forcing a hardfork gate as a
correctness requirement, and it changes more than the diversity goal needs even when contended.

**Explicit scarcity branch** — `if unassigned_parts.len() < unassigned_pledges.len()` selecting between
the level sort and the legacy sort. Reaches the same outcomes as the accepted design and was the first
form considered. Rejected as strictly weaker on reviewability: a reviewer must verify both the predicate
and that the else-arm reproduces the old behavior, and the two arms can drift. With select-then-order the
uncontended identity is algebraic rather than conditional, and there is only one code path.

**Weak fair-share ordering** — key on rank among a signer's *pending* pledges rather than on holdings,
tie-broken by txid. Rejected as close to a no-op: a 42-holder's first pending pledge ties a newcomer's
first, so the deep holder still takes about half of each round.

**Counting capacity plus data holdings** — see Capacity-only counting above. Rejected because data
partitions are assigned by the protocol at random, so counting them penalises miners for the work the
network wants them doing.

**Raising capacity supply to meet demand** — mint beyond the `log10` target when pledges are
unassignable. Converts a distribution problem into a supply problem, and changes total partition count,
mining difficulty, and reward economics. Out of scope.

**Changing the pledge decay curve** so a marginal pledge stops undercutting a new address's first.
Attacks the incentive rather than the mechanism, but changes staking economics and unpledge refund
semantics (`commitment_v2.rs:166`), and Sybil identities defeat it regardless.

## Testing

Alongside `unique_addresses_per_slot_test` and `partitions_assignment_determinism_test` in
`crates/actors/tests/epoch_snapshot_tests.rs`:

The uncontended identity is the property most worth spending test effort on, since the deployment
argument rests on it.

- **Uncontended identity, property-based.** Over randomised holdings, signer counts, and pending-pledge
  counts, with `unassigned_parts.len() >= pending.len()`, assert the resulting `capacity_partitions`,
  every `PledgeEntry.partition_hash`, and `get_hash` are bit-identical to the pre-change implementation.
  This is the test that permits shipping; the boundary case `len() == len()` must be included explicitly.
- **Contended selection** — the worked example: a zero-holder and a one-holder are selected ahead of a
  42-holder, and the selected winners receive hashes in txid order among themselves.
- **Level derivation** — a signer holding `n` with `m` pending pledges claims levels `n..n+m`, with the
  within-signer sequence ordered by txid.
- **Selection equivalence** — the `(level, id)` sort plus truncate selects the same set as a reference
  implementation that re-picks the minimum holder until supply is exhausted, over randomised inputs.
- **Uncontested surplus** — with only a deep holder posting pending pledges, every available partition
  is still assigned; nothing is reserved.
- **Promotion resets the count** — a signer whose capacity partition moves to a data slot via
  `process_slot_needs` is served at its reduced capacity count, not its total holdings.
- **Tie-break** — equal-level pledges resolve by txid; assert the outcome is independent of the input
  order of the pledge collection.
- **Determinism** — identical snapshot hash across replay and across permuted input orderings of
  commitments and pledges.
- **Genesis path** — `try_genesis_init` (`:498`) output is unchanged for a genesis config whose minted
  capacity (`:484`) meets or exceeds its pledge count, so replay from height 0 is unaffected.
- **Pre-activation**, if gated — byte-identical to today before the fork timestamp, asserted on a
  *contended* fixture, since an uncontended one cannot distinguish the two paths.
- **Integration** — a multi-miner scenario in `crates/chain-tests` where a late-joining miner reaches a
  full diverse replica set from newly minted capacity while a dominant holder is throttled.

## Open questions

Resolve before implementation:

1. ~~**Has any live network ever had a contended epoch?**~~ **Measured — no.** The deployment argument
   depends on this, so it was tested rather than assumed. A probe logging `unassigned_partitions.len()`
   against the pending-pledge count, placed ahead of the early returns in
   `assign_partition_hashes_to_pledges` (the zero-supply case returns early at `:1086` and would
   otherwise be missed), was run on mainnet and testnet nodes. Startup replay re-runs
   `perform_epoch_tasks` for every epoch block from genesis
   (`query_replay_data` loops `0..latest_height / num_blocks_in_epoch + 1`), so one restart covers each
   chain's full local history. No contended epoch was recorded on either network, so the rule is inert
   across that history and the activation question reduces to the prospective case.

   Scope and control. Both probed nodes were synced from genesis, so replay covered each chain's full
   history, and the `uncontended` control line was confirmed present — an empty contended result is
   therefore evidence rather than a filtered-out probe. Startup replay is sufficient coverage on its own:
   assignment runs only at epoch boundaries, so re-running every epoch block visits every assignment
   decision the chain has ever made. Devnet was not probed and is treated as resettable.

   This result is consistent with the supply model rather than a surprise: capacity supply is
   logarithmic but generous against a three-miner field, which is the same measurement that leaves
   roughly 77 partitions unassigned on mainnet-beta.
2. ~~**Truncation versus the pre-existing hash-burn.**~~ **Resolved — truncation is safe.** The
   assignment loop pops a hash (`:1120`) *before* the signer lookups that can `continue` (`:1130`,
   `:1135`), so a miss would consume a hash without assigning it and `truncate(k)` would serve `k-1`.
   Both arms are unreachable:

   - `get_mut(&pledge.signer)` cannot miss. The only production writer that inserts pledge entries is
     `compute_commitment_state` (`:968`-`:972`), which keys by `pledge_commitment.signer()` and sets
     `PledgeEntry.signer` from the same expression, so key ≡ `entry.signer` for every entry. The other
     production mutators do not rekey: `apply_unpledges` (`:1049`-`:1053`) does `get_mut` plus
     `remove(pos)`, and `apply_unstakes` (`:381`) removes. The three `insert` calls in
     `commitment_snapshot.rs` (`:572`, `:602`, `:676`) are inside `mod tests` (`:441`).
   - `find(|e| e.id == pledge.id)` cannot miss. `unassigned_pledges` is cloned from that same map
     (`:1096`-`:1106`), and between the clone and the loop nothing removes entries — the loop mutates
     only `entry.partition_hash` (`:1139`) and `self.unassigned_partitions` (`:1158`).
     `apply_unpledges` runs earlier in the epoch (`:347` before `:357`), never interleaved.

   One adjacent observation, recorded because the epoch layer depends on it without asserting it: a
   duplicate pledge txid would not trigger either arm — `find` would return the same first entry twice,
   the second iteration overwriting `partition_hash` while the first hash had already been inserted into
   `capacity_partitions` (`:1142`) and pruned from `unassigned_partitions` (`:1158`), orphaning it.
   `compute_commitment_state` does not dedupe by design (`:929`) and `validate_commitments` (`:190`)
   checks only count equality and bidirectional membership, all of which a twice-listed txid passes. The
   guard is one layer up, in `crates/actors/src/commitment_dedup.rs`, which explicitly rejects a
   commitment listed twice within the block under validation. No change needed here.
3. **Which counted state is authoritative at call time.** `assign_partition_hashes_to_pledges` runs
   last in `perform_epoch_tasks` (`:357`), after `expire_ledger_slots` (`:345`) has returned expired
   data partitions to their holders as capacity and after `backfill_missing_partitions` (`:353`) has
   promoted capacity into slots. Confirm `capacity_partitions` at that point reflects both, so `held`
   measures the true post-epoch pool. The same question applies to the `try_genesis_init` call site,
   which runs before any backfill.
4. **Interaction with the one-epoch assignment lag.** Backfill runs before minting and assignment, so
   a pledge assigned at epoch N cannot enter a data slot until N+1. Confirm water-filling does not
   extend that lag for newcomers in a way that delays replica-set growth beyond the current behavior.
5. **Existing test fixtures.** Identify which tests in `crates/actors/tests/epoch_snapshot_tests.rs`
   and `crates/chain-tests` run a contended epoch and assert on the resulting distribution. Uncontended
   fixtures need no change.
6. **Over-pledged collateral.** A deep holder's pledges stay active with `partition_hash: None`,
   locking collateral that is served only when demand elsewhere is exhausted. This is a delay, not a
   hard gate — the pledge stays serviceable — so decide whether any mempool signal is warranted or
   whether the delay is acceptable as-is.

## Out of scope

- **Any bound on total holdings**, and therefore any eviction, reclaim, or repack path.
- **Minimum-replica guard on unpledge** — `apply_unpledges` (`:1018`) permits unpledging a data
  partition that is the sole replica of a slot, leaving the slot at zero replicas with no data to
  recover from. A separate defect and a separate fix.
- **Capacity supply formula**, **pledge cost curve**, and **Sybil identity**, per above.

## Source

Design discussion, 2026-08-04. Distribution measurements taken at epoch 2037600 via
`mainnet-beta-rpc.irys.xyz`. The historical-contention result comes from a temporary probe run against
mainnet and testnet nodes during startup epoch replay.
