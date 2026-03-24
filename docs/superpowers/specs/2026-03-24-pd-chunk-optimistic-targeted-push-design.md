# PD Chunk Optimistic Targeted Push Design

**Date**: 2026-03-24
**Branch**: `feat/pd`
**Issues**: [#1231](https://github.com/Irys-xyz/irys/issues/1231), [#1217](https://github.com/Irys-xyz/irys/issues/1217)
**Alternative To**: `docs/superpowers/specs/2026-03-23-pd-chunk-availability-gossip-design.md`
**Status**: Exploratory alternative

## Executive Summary

This alternative keeps PD chunk bytes on a push path, but only for hot, mempool-live PD transactions and only to a small targeted subset of peers.

This document assumes the simplest useful suppression rule:

- keep a local TTL cache of which peers already tried to push a given content fragment to us
- do not push that same fragment back to those peers
- do not carry transitive peer-history metadata on the wire

In short:

- **current pull-only** is bandwidth-efficient but creates assignee hotspots at validation time
- **availability gossip + targeted pull** is still bandwidth-efficient and spreads load later, but it does not make validation-time network waits disappear
- **optimistic targeted push** can make validation-time network wait approach zero for hot transactions that spend enough time in mempool, but it does so by spending speculative byte traffic before those bytes are actually demanded

The key claim is:

> Optimistic targeted push is a latency optimization, not a bandwidth optimization. It only wins when the transaction is hot, likely to land soon, and likely to be validated by many peers before the pushed bytes age out.

This design does **not** solve historical block catch-up. If a node is validating older blocks after the mempool window has passed, the only durable source is still the partition holders and any recent `CachedChunks` survivors. That is true for this design and for the current availability-gossip design.

### Worked Example

Assumptions for the comparison below:

- `100` validators
- `10` seed nodes already have the PD data when the mempool transaction arrives
  - these are typically the block producer, partition assignees, or any node that already fetched the tx data
- the hot PD working set is `100` chunks
- chunk size is `256KiB`
- total pushed data per validator warm-up is `100 * 256KiB = 25MiB`
- `90` validators do not initially have that data
- network RTT between nodes = `100ms`
- line rate per node = `1Gbps ~= 125MB/s ~= 119MiB/s`

Useful math:

- transfer time for one `25MiB` push at `1Gbps` is about `25 / 119 ~= 0.21s`
- one push round therefore costs about `210ms` of wire time, or about `310ms` if you budget `1 RTT + transfer`
- minimum useful downstream data to warm all `90` missing validators is:
  - `90 * 25MiB = 2250MiB ~= 2.2GiB`

### Comparison Table

| Approach | When bytes move | Total PD bytes moved in worked example | Validation-time network wait | Helps catch-up on old blocks? | Main tradeoff |
|----------|-----------------|----------------------------------------|------------------------------|-------------------------------|---------------|
| Current pull-only | only when validator actually needs chunks | about `2.2GiB` useful traffic | best-case near `310ms`; tail grows under assignee hotspots | yes | lowest byte overhead, highest validation-time hotspot risk |
| Availability gossip + targeted pull | metadata early, bytes only on demand | about `2.2GiB` useful traffic plus tiny control overhead | still near pull lower bound, but with better provider spread | yes | low byte overhead, but not zero-wait at validation time |
| Optimistic targeted push, ideal lower bound | bytes pushed before inclusion, no duplicate deliveries | about `2.2GiB` | near `0ms` if warm-up completes before inclusion | no | best-case latency, but only if push finishes in time |
| Optimistic targeted push, bounded-fanout model (`k=4`) | bytes pushed before inclusion, with collisions | about `16.0GiB` | near `0ms` if tx lands after about `3.25` rounds (`~0.68s` wire-time floor, `~1.01s` with RTT budget) | no | much faster hot-path validation, but about `7.3x` the useful bytes |
| Brute-force epidemic push | bytes pushed to everyone after validation | about `241.7GiB` | can preload, but bandwidth dominates | no | catastrophic duplicate bulk transfer |

### Fanout Sensitivity for Optimistic Push

I ran a simple Monte Carlo model for bounded-fanout optimistic push:

- `100` validators, `10` seeds
- each informed node pushes the same `25MiB` hot set to `k` peers per round
- there is **no global coordination**
- all pushes succeed
- recipients become senders next round

This table is meant to show the traffic shape of the optimistic-push family, not to claim that every bookkeeping variant has exactly these numbers.

Results:

| Fanout `k` | Ideal rounds with perfect coordination | Simulated average rounds | Simulated push attempts | Simulated bytes moved | Duplicate factor vs useful minimum |
|-----------:|---------------------------------------:|-------------------------:|------------------------:|----------------------:|------------------------------------:|
| `2` | `3` | `5.16` | `589` | `14.4GiB` | `6.5x` |
| `4` | `2` | `3.25` | `655` | `16.0GiB` | `7.3x` |
| `8` | `2` | `2.21` | `736` | `18.0GiB` | `8.2x` |

Interpretation:

- higher fanout reduces warm-up rounds
- but higher fanout also increases duplicate full-batch deliveries
- under this bounded-fanout model, optimistic push warms the network quickly, but not cheaply

### Why This Can Still Be Attractive

If the transaction sits in mempool for at least about `1s` before inclusion, bounded targeted push can plausibly warm most validators ahead of time. Then block validation becomes a local `CachedChunks` hit instead of a network pull.

That is the main appeal:

- **hot mempool transaction** -> bytes arrive before the block does
- **block arrives** -> validation reads locally and avoids the assignee herd

### Why This Can Still Be Wasteful

Optimistic push spends bytes speculatively.

If the transaction:

- never lands in a block
- is replaced
- is evicted from mempool
- or is only validated by a minority of peers before cache expiry

then some or most of the pushed bytes were unnecessary.

Example:

- with the `k=4` simulation above, the network moved about `16.0GiB`
- if only half of the `90` candidate validators ever needed the `25MiB` data before expiry, useful delivery would be only about `1.1GiB`
- the remaining `~14.9GiB` would be speculative waste

So this approach only makes sense when the tx is both hot **and** likely to be consumed widely and soon.

### High-Level Flow Comparison

Current pull-only:

```text
Block arrives
    |
Validator B derives needed PD offsets
    |
    |---- pull chunk x from assignee ---->
    |<--- chunk x ------------------------
    |
    |---- pull chunk y from assignee ---->
    |<--- chunk y ------------------------
    |
validate block
```

Availability gossip + targeted pull:

```text
Early validator finishes block validation
    |
    |---- announce ranges to fanout ---->
    |
Later validator needs chunk
    |
    |---- pull from hinted provider ----->
    |<--- chunk --------------------------
    |
validate block
```

Optimistic targeted push:

```text
PD tx enters mempool
    |
Seed node has content context + chunks
    |
    |---- push content-scoped chunk fragment to peers A/B/C/D ---->
    |
Peers A/B/C/D verify + store in CachedChunks
    |
Peers A/B/C/D may each do their own bounded fanout when they first become able to serve the fragment
    |
Block including that tx arrives later
    |
Most validators hit CachedChunks locally
    |
validators that missed the warm-up still fall back to pull
```

Catch-up on historical blocks:

```text
Node is syncing older blocks
    |
No hot mempool context remains
    |
CachedChunks may already be gone
    |
---- pull from partition holders / durable sources ---->
```

## Problem

The current bottleneck is not that PD bytes can never move through the network. It is that too many validators ask the same durable sources for the same bytes at the same moment, right when block validation is on the critical path.

The optimistic targeted-push idea attacks that timing problem directly:

- move bytes **before** block validation instead of during it
- only do so for hot mempool PD transactions
- only send to a bounded targeted subset, not to all peers

## Goals

- Reduce validation-time PD pull pressure for hot mempool PD transactions.
- Allow locally available PD chunk bytes to spread before block inclusion.
- Avoid full-mesh byte gossip.
- Keep all push bookkeeping local and TTL-bounded.
- Preserve existing pull-from-assignee behavior as the fallback and catch-up path.

## Non-Goals

- Guarantee zero network wait for every validator.
- Solve validation of historical blocks long after mempool propagation has ended.
- Push full block-sized PD data to the whole network.
- Build a globally consistent chunk-location index.

## Core Idea

Replace post-validation availability gossip with **pre-validation optimistic targeted push** for mempool-live PD transactions.

When a node learns about a PD transaction in the mempool and also has the corresponding chunk data locally, it proactively pushes those chunks to a small subset of peers.

In the simplified variant, that fanout happens when the node first becomes able to serve the fragment, rather than as an open-ended resend loop.

The subset is chosen using one deliberately simple suppression rule:

- exclude peers that already pushed the same fragment to us recently
- otherwise choose a bounded fanout, preferably mixing high-score and random peers

Recipients verify and store those chunks in `CachedChunks`. If the tx later lands in a block while the cache is still warm, validation can read locally instead of pulling from partition assignees.

This variant intentionally does **not** try to maintain a transitive “who already saw this” graph. That optimization is possible, but it adds complexity and trust surface for only moderate byte savings.

## Important Consequence: The Key Space Changes

Unlike the block-validation path, the mempool path does **not** yet have stable publish-ledger offsets for the future block.

So this design should be keyed by **content identity**, not by `(ledger, offset)`:

- `data_root`
- `data_size`
- `tx_chunk_offset`

`data_root` alone is not enough because rightmost-chunk validation depends on the total `data_size`. So the natural key is:

- `PdContentKey { data_root, data_size }`

That makes the natural push payload content-scoped, not block-scoped.

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Push trigger | PD tx enters mempool and local node has chunk data | Moves bytes before block validation |
| Data key | `PdContentKey { data_root, data_size }` | Content identity is what chunk verification and cache storage actually use |
| Push unit | `PdContentChunkFragment` | Fragmented, content-scoped push avoids giant block batches |
| Fanout | small targeted subset, likely `k=2..4` | Keeps warm-up bounded while containing duplicate bytes |
| Receiver precondition | content key should already be known recently, or receiver rejects as unknown content | Avoids unsolicited byte spam |
| Receiver storage | `CachedChunks` keyed by `(data_root, tx_chunk_offset)` | Makes pre-pushed data available during later validation |
| Local bookkeeping | TTL cache of inbound pushers per content fragment | Gives one clean duplicate suppression rule without propagation-history complexity |
| Fallback | existing assignee-targeted pull path | Still required for misses and catch-up |
| Catch-up | explicitly unsupported by the optimistic fast path | Hot-cache design, not archival dissemination |
| Route limits | small fragment cap plus per-peer byte budget | Byte-push route must be byte-limited, not batch-count-limited |

## Proposed Wire Types

This design needs content-scoped push payloads, not block-scoped payloads:

```rust
pub struct PdContentChunkPush {
    pub tx_chunk_offset: u64,
    pub chunk: UnpackedChunk,
}

pub struct PdContentChunkFragment {
    pub fragment_id: H256,
    pub data_root: H256,
    pub data_size: u64,
    pub tx_id_hint: Option<IrysTransactionId>,
    pub chunks: Vec<PdContentChunkPush>,
}
```

Notes:

- `data_root` identifies the content tree
- `data_size` is still required to validate the rightmost-chunk edge case, so `data_root` alone is not enough
- `tx_chunk_offset` is stable before block inclusion
- `tx_id_hint` is optional mempool correlation metadata only, not part of the data key
- `fragment_id` is for sender/receiver dedup at the fragment level

## Required Local State

### `RecentPdContentContextCache`

Short-lived cache keyed by `PdContentKey`:

```rust
PdContentKey -> {
    chunk_count,
    mempool_expiry,
    recent_tx_ids_hint,
}
```

Purpose:

- sender-side proof that this content is still associated with recent PD mempool activity
- receiver-side anti-spam gate for “is this content currently hot enough to accept optimistic push?”
- optional correlation back to recent `tx_id`s for tracing only

### `PdContentPushInboundCache`

Tracks which peers tried to push a given content fragment to us:

```rust
(PdContentKey, fragment_id) -> small set of PeerId
```

Purpose:

- if peer `P` tried to push fragment `F` to us, then `P` obviously already has `F`
- we should not target `P` with the same fragment again

This is the only peer-propagation bookkeeping in the simplified design.

## Sender Flow

```text
PD tx enters mempool
  -> detect PD tx
  -> derive PdContentKey { data_root, data_size }
  -> store content context in RecentPdContentContextCache
  -> if local node can serve content chunks:
       build content-scoped PdContentChunkFragment values
       choose up to k peers excluding:
         - self
         - peers in inbound cache for this content/fragment
       push fragment bytes directly
       do not attach or merge any transitive peer-history metadata
```

## Receiver Flow

```text
POST /gossip/v2/pd_content_chunk_fragment
  -> check route-specific body cap before JSON decode
  -> verify sender identity
  -> enforce per-peer byte budget and fragment budget
  -> derive PdContentKey from (data_root, data_size)
  -> look up PdContentKey in RecentPdContentContextCache
       if missing:
         reject UnknownContent
         optional: tiny orphan queue only, but no large byte parking
  -> verify data_root / data_path / chunk hashes against content key
  -> store chunks in CachedChunks
  -> record inbound sender in PdContentPushInboundCache
  -> optionally schedule onward optimistic push if the content is still associated with recent mempool PD activity
```

## Validation-Time Behavior

If the block arrives while optimistic push data is still hot:

```text
block validation needs chunk
  -> resolve (data_root, tx_chunk_offset) via on-chain tx metadata
  -> CachedChunks hit
  -> no network pull on critical path
```

If the block arrives before optimistic propagation finishes, or if this validator missed the wave:

```text
block validation needs chunk
  -> CachedChunks miss
  -> fall back to existing assignee-targeted pull
```

So this design is additive, not replacement:

- hot path = local cache hit
- cold path = existing pull path

## Catch-Up and Historical Validation

This design does not materially change historical validation behavior.

Why:

- the optimistic push is tied to mempool-time content liveness derived from recent PD transactions
- `CachedChunks` is not archival
- by the time a syncing node reaches an old block, the optimistic wave is usually gone

Therefore:

- catch-up remains assignee pull
- sync durability still depends on partition holders and any other durable sources

## Why This Is Better Than Brute Force

Compared to full-mesh chunk-byte gossip, optimistic targeted push has real guardrails:

- pushes are content-scoped, not block-scoped
- fanout is bounded
- outbound targets are filtered by one simple inbound-pusher exclusion rule
- no node should blindly resend to every peer

That keeps it far below epidemic push.

In the worked example:

- brute-force epidemic push: about `241.7GiB`
- optimistic targeted push, realistic `k=4`: about `16.0GiB`

So it is about an order of magnitude better than brute force.

## Why This Is Still Worse Than Metadata Gossip + Pull

Compared to availability gossip + pull, optimistic targeted push spends byte traffic before demand is known.

In the same worked example:

- availability gossip + pull: about `2.2GiB` useful traffic plus tiny metadata
- optimistic targeted push, realistic `k=4`: about `16.0GiB`

So the price of near-zero validation-time wait is about `7.3x` more bytes in this model.

## Guardrails

These are hard design invariants:

1. **Never full-mesh optimistic push.** Fanout must remain small and bounded.
2. **Never push by future block hash.** Mempool-time dissemination must be keyed by content identity; `tx_id` is optional hint metadata only.
3. **Never rely on batch-count rate limiting alone.** This route carries bytes; it needs a route-specific body cap and per-peer byte budgets before handler work explodes.
4. **Never park unlimited unknown-content byte payloads.** If the receiver lacks recent content context, reject or keep only a tiny bounded orphan queue.
5. **Never treat optimistic push as archival dissemination.** Historical block validation still falls back to durable pull sources.
6. **Never allow unbounded content size on the optimistic path.** Production deployments likely need a max optimistic-push byte cap per content item or per fragment.
7. **Never exchange transitive peer-history metadata on the wire in this variant.** The only suppression rule is local inbound-pusher exclusion.

## Recommended Parameters

If this design is explored, the safe starting point is conservative:

- optimistic fanout `k = 2..4`
- small fragment size, for example `1MiB` to `4MiB`
- strict per-peer byte budget, not just "fragments per minute"
- short TTLs, for example `2` to `5` minutes
- disable optimistic push entirely for very large PD txs

The numbers above suggest:

- `k=8` gains speed, but byte duplication rises quickly
- `k=2` is cheapest among the tested fanouts, but warm-up may miss short mempool-to-block windows
- `k=4` is the most balanced starting point for experiments

## Suggested Positioning

This alternative makes the most sense as:

- a **hot-path accelerator** for mempool-live PD txs
- especially when mempool dwell time is long enough to finish at least a few push rounds

It makes less sense as:

- a full replacement for availability gossip + pull
- a solution for historical block sync
- a protocol for large txs or block-sized PD working sets

## Bottom Line

Optimistic targeted push is viable as a speculative fast path if the real goal is:

- spend more bandwidth
- to remove validation-time PD pulls
- for hot transactions only

It is not the best choice if the real goal is:

- minimizing total network traffic
- keeping bytes demand-driven
- or helping historical validation

If adopted, the cleanest framing is probably:

- keep pull as the correctness and catch-up path
- keep availability gossip + pull as the bandwidth-efficient general solution
- add optimistic targeted push only as an optional low-latency accelerator for small or medium hot PD txs
