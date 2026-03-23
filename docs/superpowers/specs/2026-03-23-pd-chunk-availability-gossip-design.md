# PD Chunk Availability Gossip Design

**Date**: 2026-03-23
**Branch**: `feat/pd`
**Issues**: [#1231](https://github.com/Irys-xyz/irys/issues/1231), [#1217](https://github.com/Irys-xyz/irys/issues/1217)
**Supersedes**: `docs/superpowers/specs/2026-03-20-pd-chunk-push-gossip-design.md`

## Executive Summary

This design keeps PD chunk bytes on an on-demand pull path and only gossips small availability metadata.

In short:

- **current pull-only** moves close to the minimum necessary number of bytes, but it concentrates them on the canonical assignees
- **brute-force chunk push** removes some pull RTTs, but explodes total network traffic and turns bandwidth into the bottleneck
- **this proposal** keeps total chunk bytes near the pull-only minimum while spreading those bytes across many more providers over time

The key claim is simple:

> On a 1Gbps network, the physical lower bound to deliver 25MiB of PD data is already about `210ms` of wire time, so a design that causes every node to spray the same 25MiB to everyone else loses to bandwidth long before it wins on RTT.

### Worked Example

Assumptions for the comparison below:

- `100` validators
- `10` canonical partition assignees for the needed PD chunk set
- block references `100` PD chunks
- chunk size is `256KiB`
- total PD data needed by a validator that misses all chunks = `100 * 256KiB = 25MiB`
- `90` validators need to fetch those chunks
- network RTT between nodes = `100ms`
- line rate per node = `1Gbps ~= 125MB/s ~= 119MiB/s`

Useful math:

- transfer time for `25MiB` on a `1Gbps` link is about `25 / 119 ~= 0.21s`
- best possible "one validator gets all missing PD data from the network" floor is therefore roughly:
  - `1 RTT + 25MiB transfer ~= 100ms + 210ms ~= 310ms`
- total useful PD data delivered to `90` validators is:
  - `90 * 25MiB = 2250MiB ~= 2.2GiB`

### Comparison Table

| Approach | Control-plane ping-pongs | Data ping-pongs | Total PD bytes moved in worked example | Bottleneck | Latency implication |
|----------|--------------------------|-----------------|----------------------------------------|------------|---------------------|
| Current pull-only | none | `90 * 100 = 9000` per-chunk pull responses | about `2.2GiB` useful traffic | canonical assignees | best-case near `310ms`; tail grows as assignees queue up |
| Brute-force push from 10 assignees to all peers | none | `10 * 99 = 990` full-batch pushes | `10 * 99 * 25MiB = 24.2GiB` | assignee egress bandwidth | each assignee must transmit about `2.42GiB` (`~20s` at 1Gbps) |
| Brute-force epidemic rebroadcast | none | `100 * 99 = 9900` full-batch pushes | `100 * 99 * 25MiB = 241.7GiB` | every node's ingress and egress bandwidth | each node sees about `2.42GiB` inbound and outbound (`~20s` each direction at 1Gbps) |
| Availability gossip + targeted pull (this design) | about `100 * 8 = 800` tiny availability announcements at fanout 8 | still roughly `9000` per-chunk pulls in the worst case, but spread across more providers | about `2.2GiB` useful traffic plus tiny announce overhead | actual chunk providers, which grow beyond the 10 assignees | first validators look like current pull-only; later validators move toward the `310ms` floor instead of waiting on assignee hotspots |

### Why Brute Force Loses

Even the mild brute-force version is already worse than pull-only.

If only the `10` canonical assignees push the `25MiB` PD set to all `99` other validators:

- aggregate traffic is `24.2GiB`, about **11x** more than the `2.2GiB` of actually useful downstream transfers
- each assignee must send `99 * 25MiB = 2475MiB ~= 2.42GiB`
- at `1Gbps`, that is about `2475 / 119 ~= 20.8s` of serialized wire time per assignee before protocol overhead

If the design is epidemic and every validator re-pushes after validation:

- aggregate traffic becomes `241.7GiB`
- every node must receive about `2.42GiB`
- every node must also send about `2.42GiB`
- at `1Gbps`, this is still about `20s` of wire time in each direction per node

And this worked example is intentionally modest. The protocol ceiling is much higher than `100` PD chunks per block, so every brute-force byte-push number above scales up linearly from here.

So brute-force push does not really "save 100ms RTT". It trades a few RTTs for **tens of seconds of avoidable bulk transfer**.

### Why This Design Wins

This proposal stays close to the pull-only byte minimum:

- chunk bytes move only to nodes that actually need them
- availability gossip is tiny compared to chunk bytes
- as soon as a validator has fetched and validated a chunk, it can become a provider too

That means the system keeps the good property of pull-only:

- total bytes stay close to the useful minimum

while fixing the bad property of pull-only:

- the same 10 assignees no longer carry nearly all the load for the whole validation wave

### High-Level Flow Comparison

Current pull-only:

```text
Validator B                Assignee A1/A2
    | receive block              |
    | derive PD offsets          |
    |---- pull chunk x --------> |
    |<--- chunk x -------------- |
    |---- pull chunk y --------> |
    |<--- chunk y -------------- |
    |---- pull chunk z --------> |
    |<--- chunk z -------------- |
    | validate block             |
```

Availability gossip + targeted pull:

```text
Stage 1: an early validator becomes a provider

Validator A                    Fanout Peers
    |                               |
    | validate block                |
    | find chunks it can serve      |
    |---- announce ranges --------> |   tiny metadata only
    |<--- accepted ---------------- |

Stage 2: a later validator uses the hint

Fanout Peers / Hint Cache       Validator B                Provider A
         |                          |                          |
         | store hint: x,y,z -> A   |                          |
         |------------------------> |                          |
         |                          | derive PD offsets        |
         |                          | choose hinted provider   |
         |                          |---- pull chunk x ---------------------->|
         |                          |<--- chunk x --------------------------- |
         |                          |---- pull chunk y ---------------------->|
         |                          |<--- chunk y --------------------------- |
         |                          | verify + cache chunks    |
         |                          | validate block           |

Stage 3: the later validator now helps too

Validator B                    Fanout Peers
    |                               |
    | announce ranges               |
    |---- tiny metadata only -----> |
```

Brute-force push:

```text
Validator A                    All Other Validators
    | validate block               |
    |-- push full 25MiB batch ---> B
    |-- push full 25MiB batch ---> C
    |-- push full 25MiB batch ---> D
    |-- push full 25MiB batch ---> ...

Then B, C, D, ... do the same again after they validate.
```

### Expected Validation Latency

Under the worked-example assumptions:

- **Current pull-only**
  - network lower bound is about `310ms`
  - but all `2.2GiB` of useful traffic is concentrated on about `10` assignees
  - if balanced perfectly, each assignee serves about `225MiB`, which is about `1.9s` of serialized transmit time at `1Gbps`
  - in practice this produces a long tail as validators queue on the same providers

- **Availability gossip + targeted pull**
  - the first few validators are close to current pull-only, because hinted providers do not exist yet
  - after those validators finish, they advertise and join the provider set
  - if the effective provider pool grows from `10` to `50`, average provider egress for the same `2.2GiB` falls from about `225MiB` to about `45MiB`
  - `45MiB` at `1Gbps` is about `380ms` of wire time instead of `1.9s`
  - the design does not beat the physical `310ms` floor, but it materially reduces the hotspot-driven tail

- **Brute-force push**
  - in the abstract it looks like "preload once, validate later"
  - on a `1Gbps` network the real cost is bulk transfer, not RTT
  - the preload wave itself takes seconds to tens of seconds, so the apparent RTT win disappears

## Problem

PD chunk fetching is currently pull-only: when a node needs chunks for PD transaction validation, it queries the small set of partition assignees who store that data. In a 100+ validator network, many validators needing the same chunks at the same time creates a thundering herd on those few assignees.

The original "push full chunk bytes to all peers" idea spreads load, but it creates a worse failure mode:

- full chunk batches can be enormous at the real PD limit
- every validator rebroadcasting chunk bytes to every peer becomes quadratic
- receiver dedup only happens after the duplicate bytes already crossed the network
- generic block gossip cache keys cannot safely be reused for PD chunk delivery

## Goals

- Spread PD chunk read load away from canonical partition assignees during same-block validation.
- Allow any validator that already has a PD chunk to become a source for later validators.
- Keep gossip bandwidth bounded by small control-plane messages, not by chunk bytes.
- Reuse the existing per-chunk pull path and verification model for correctness.
- Keep implementation close to the current code structure.

## Non-Goals

- Pre-seed the entire network with all PD chunk bytes.
- Replace pull-based PD chunk transfer.
- Build a long-lived global chunk-location index.
- Guarantee that the first validator to request a chunk never hits an assignee.

## Solution

Replace **chunk-byte push gossip** with **availability gossip plus targeted pull**.

After a node validates a block containing PD transactions, it announces a compact inventory of the PD chunks from that block that it can currently serve. The announcement contains only ledger coordinates, coalesced into ranges. It is sent to a small randomized fanout, not to the full peer set.

Receivers store those announcements as short-lived provider hints. When they later need a PD chunk, they still use the existing `GossipDataRequestV2::PdChunk(ledger, offset)` pull path, but they try hinted providers before falling back to canonical partition assignees.

This keeps the data plane on-demand and verified, while still letting every validator participate: if a validator pulled a chunk during validation and stored it in `CachedChunks`, then after it validates the block it can announce that chunk and serve subsequent pulls.

## Guardrails

These are hard design invariants:

1. **Never gossip PD chunk bytes.** Gossip carries only availability metadata.
2. **Never full-mesh PD availability.** Announcements go to a bounded fanout only.
3. **Never rebroadcast third-party announcements blindly.** A node only announces chunks it can currently serve itself.
4. **Never reuse `GossipCacheKey::Block`.** PD availability gets its own cache namespace.
5. **Never rely on batch-count rate limiting alone.** The route must have a small body cap, and limits must be based on bytes and/or announced chunks.

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Gossip payload | `PdChunkAvailabilityBatch { availability_id, block_hash, ranges }` | Compact control-plane message; no chunk bytes |
| Push trigger | `BlockStateUpdated::Valid` | Same timing as the original proposal; only validated blocks cause announcements |
| Fanout | Small randomized subset (`pd_availability_fanout`) | Avoids quadratic full-mesh gossip |
| Data transfer | Existing `GossipDataRequestV2::PdChunk(ledger, offset)` pull | Keeps correctness and verification in one place |
| Participation model | Any node with local storage or `CachedChunks` may announce | Lets non-assignee validators become providers |
| Receiver state | Short-lived provider-hint cache keyed by `(ledger, offset)` | Cheap, bounded, advisory state |
| Dedup | Dedicated `GossipCacheKey::PdChunkAvailability(H256)` per announcement fragment | No collision with block gossip; no suppression of complementary fragments |
| Route limits | Small request-body cap plus per-peer byte/chunk budget | Prevents large parse and memory spikes |
| Candidate pull peers | Hinted providers first, assignees second, both capped and shuffled | Spreads load without creating a new herd |

## Why This Avoids the Previous Footguns

### No quadratic chunk-byte gossip

Validators do not send chunk bytes to every peer. They send small availability announcements to a bounded fanout. Actual chunk bytes move only when some receiver explicitly needs them.

### No block-key collision

PD availability uses its own `GossipCacheKey::PdChunkAvailability(...)`. It never shares the `Block(block_hash)` namespace used by block header/body gossip.

### No oversized per-block payloads

The wire payload contains only ranges of `(ledger, offset)` coordinates. Even at the real PD limit, the message size stays in the KB range, not the GB range.

### No batch-count-only rate limiting

The route has a small body limit before JSON deserialization, and the handler enforces per-peer limits by bytes and/or announced chunks, not just by "number of batches".

## Architecture

### Availability Announce Flow (Sender)

```
BlockTreeService::on_block_validation_finished
  -> BlockStateUpdated::Valid { block_header, block_body, ... }
    -> P2PService receives event
      -> Fetch evm_block from ExecutionPayloadCache using block_header.evm_block_hash
      -> extract_pd_chunk_specs_from_block(&evm_block)
      -> specs_to_ledger_offsets(...) -> Vec<(ledger, offset)>
      -> For each offset:
           storage_provider.has_chunk_for_pd(ledger, offset)
             OR CachedChunks-backed get_chunk_for_pd existence check
      -> Keep only offsets this node can currently serve
      -> Coalesce contiguous offsets into PdChunkRange
      -> Split into bounded fragments
      -> Select fanout peers (top scored + random remainder)
      -> Send each PdChunkAvailabilityBatch directly to those peers
```

Important:

- This path **must not** use the generic `broadcast_data()` loop that eventually walks the full peer list.
- Each announcement fragment is sent only to the selected fanout peers.
- The sender records a dedicated PD-availability seen-key only for those fragments.

### Availability Receive Flow (Receiver)

```
POST /gossip/v2/pd_chunk_availability
  -> Small route-specific body limit check
  -> Sync / gossip reception check
  -> Per-peer byte-or-chunk budget check
  -> Parse PdChunkAvailabilityBatch
  -> Validate:
       - ledger == DataLedger::Publish for every range
       - ranges are sorted and non-overlapping
       - total_announced_chunks <= pd_availability_max_chunks_per_message
  -> Resolve source peer address from peer_id
  -> Expand ranges into (ledger, offset) keys
  -> Insert source peer into PdChunkProviderCache[(ledger, offset)]
       - TTL bounded
       - max providers per chunk bounded
  -> Return Accepted
```

The receiver does **not** verify chunk contents at this stage. Availability announcements are advisory only. Real verification still happens when chunk bytes are pulled.

### Targeted Pull Flow (Unchanged Data Plane)

```
PdService needs (ledger, offset)
  -> GossipPdChunkFetcher::fetch_chunk(peers_from_assignees, ledger, offset)
    -> Look up hinted providers for (ledger, offset) in PdChunkProviderCache
    -> Build candidate list:
         1. up to pd_fetch_max_hinted_peers hinted providers, shuffled
         2. up to pd_fetch_max_assignee_peers canonical assignees, shuffled
    -> Drop duplicates and self
    -> pull_pd_chunk_from_peers(candidates, ledger, offset, peer_list)
      -> existing HTTP API first, gossip pull fallback second
    -> On success: return ChunkFormat
```

This keeps the current correctness model intact:

- each chunk is still verified against on-chain metadata on receipt
- `CachedChunks` is still the local secondary-store
- assignees remain the correctness fallback if no hinted provider works

### Participation Flow

```
Partition assignee A validates block N
  -> announces availability for the PD chunk ranges it can serve

Peer B is still validating block N
  -> receives A's availability announcement
  -> pulls needed chunk(s) from A instead of only from assignees
  -> stores chunk(s) in CachedChunks

Peer B later validates block N
  -> its own availability announce flow now sees those CachedChunks
  -> B announces itself as a provider too

Peer C can now pull from A, B, or assignees
```

This is how every validator can participate without anyone pushing chunk bytes to the whole network.

## Wire Format

### New Types (`crates/types/src/gossip.rs`)

```rust
/// A contiguous run of PD chunk coordinates that a peer can serve.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdChunkRange {
    pub ledger: u32,
    pub start_offset: u64,
    pub chunk_count: u16,
}

/// One bounded availability announcement fragment.
/// `availability_id` is unique per fragment and is the dedup key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdChunkAvailabilityBatch {
    pub availability_id: H256,
    pub block_hash: BlockHash,
    pub ranges: Vec<PdChunkRange>,
}
```

### `GossipDataV2` Change

Add `PdChunkAvailability(PdChunkAvailabilityBatch)` for availability gossip.

Keep `PdChunk(ChunkFormat)` as the pull-response type. It remains data-plane only.

### `GossipCacheKey` Change

Add a dedicated PD-availability cache key:

```rust
pub enum GossipCacheKey {
    Chunk(ChunkPathHash),
    Transaction(IrysTransactionId),
    Block(BlockHash),
    ExecutionPayload(B256),
    IngressProof(H256),
    PdChunkAvailability(H256),
}
```

No `Block(block_hash)` reuse.

## Local State

### `PdChunkProviderCache`

Add a short-lived provider-hint cache in `crates/p2p`:

```rust
type PdChunkKey = (u32, u64); // (ledger, offset)

PdChunkProviderCache:
    PdChunkKey -> small set of PeerAddress
```

Requirements:

- TTL: same order as current gossip cache (for example 5 minutes)
- max providers per chunk: bounded (for example 8)
- only stores `DataLedger::Publish`
- duplicate inserts are ignored

This cache is advisory only. A peer may advertise a chunk and later evict it; pull code must tolerate misses and fall back.

## Shared Utilities

### Reused

These utilities from the original proposal are still useful:

- `extract_pd_chunk_specs_from_block`
- `specs_to_ledger_offsets`
- `derive_chunk_verification_info`
- `ChunkProvider::get_chunk_for_pd` checking `CachedChunks` before packed storage

### New

#### `ChunkStorageProvider::has_chunk_for_pd`

Availability gossip should not materialize full chunk bytes just to decide whether this node can serve them.

Add a cheap existence check:

```rust
fn has_chunk_for_pd(&self, ledger: u32, offset: u64) -> eyre::Result<bool>;
```

Preferred behavior:

- `true` if the chunk is in `CachedChunks`
- `true` if the chunk is available in local storage modules
- `false` otherwise

#### `coalesce_ledger_offsets_to_ranges`

Convert sorted offsets into compact `PdChunkRange`s:

```rust
pub fn coalesce_ledger_offsets_to_ranges(
    offsets: &[(u32, u64)],
) -> Vec<PdChunkRange> { ... }
```

## Message Sizing

`PdChunkAvailabilityBatch` must be explicitly bounded.

Recommended limits:

- `pd_availability_max_chunks_per_message = 256`
- `pd_availability_max_ranges_per_message = 64`
- `pd_availability_max_body_bytes = 256 * 1024`

If a block yields more available ranges than fit in one message, the sender emits multiple fragments, each with its own `availability_id`.

This means:

- the real PD ceiling no longer threatens multi-GB push bodies
- the receiver never needs a 100MB JSON body limit for PD gossip
- complementary fragments are safe because each one has its own dedup key

## Peer Selection

### Announcement Fanout

Use a bounded fanout instead of "all peers sorted by score".

Recommended policy:

- select `pd_availability_fanout` peers total
- take half from the best-scored connected peers
- take half randomly from the remaining connected peers
- shuffle the final list before send

This preserves reachability while avoiding clique-only behavior.

### Pull Candidate Selection

Do not turn provider hints into a new full-fanout pull herd.

For each `(ledger, offset)` request:

- try at most `pd_fetch_max_hinted_peers` hinted providers
- try at most `pd_fetch_max_assignee_peers` canonical assignees
- randomize order within each bucket
- deduplicate addresses before fetching

Example defaults:

- `pd_availability_fanout = 8`
- `pd_fetch_max_hinted_peers = 4`
- `pd_fetch_max_assignee_peers = 4`

## Verification Model

### Availability Gossip

Availability gossip is not authoritative. It only means "this peer believes it can serve this chunk".

Receiver checks are intentionally cheap:

- structural validation of the message
- publish-ledger-only validation
- bounded size validation

No Merkle verification is done here.

### Chunk Pull

Actual chunk verification remains unchanged and authoritative:

1. resolve `(ledger, offset)` via `derive_chunk_verification_info`
2. validate Merkle path against `data_root`
3. validate leaf hash against chunk bytes
4. write verified chunk to `CachedChunks`

This keeps correctness exactly where it already belongs today: on receipt of the actual chunk bytes.

## Serving from Cache

To make non-assignee peers actually useful after they pull chunks, `ChunkProvider::get_chunk_for_pd` must check `CachedChunks` first and only fall back to packed storage if the cache misses.

That makes three classes of serving peers possible:

1. partition assignees serving from local storage
2. block producers serving from local storage
3. validators serving from `CachedChunks` after they previously fetched the chunk

## Broadcast Wiring

`P2PService` still subscribes to `BlockStateUpdated::Valid`, but it uses a dedicated PD-availability send path instead of the generic full broadcast loop.

The implementation should:

1. build the availability fragments
2. choose the fanout subset
3. pre-serialize each fragment once
4. send only to the chosen peers
5. record `GossipCacheKey::PdChunkAvailability(availability_id)` only for peers that actually **Accepted** the message

Rejected responses must **not** mark the peer as seen for that availability fragment.

## Rate Limiting

The new route needs two independent guards:

### 1. Pre-deserialization body cap

Use a route-specific `JsonConfig::limit(pd_availability_max_body_bytes)` that is much smaller than the global 100MB limit.

This prevents large request bodies from being buffered and deserialized in the first place.

### 2. Per-peer announced-chunk / byte budget

The handler should reject a peer if it exceeds a sliding-window budget, for example:

- total PD-availability bytes per minute
- or total announced PD chunks per minute

Counting only "batches" is insufficient because message cost scales with bytes and announced coordinates.

## Bandwidth Considerations

This design does **not** try to reduce the minimum bytes required for validators that truly need a chunk. Those bytes still have to move somewhere.

What it does change is where those bytes move:

- gossip traffic becomes small availability metadata only
- chunk bytes move only to peers that asked for them
- as more validators finish and advertise, later pulls spread across a wider provider set

Compared to pushing full chunk bytes:

- there is no multi-GB worst-case broadcast payload
- there is no quadratic "every validator sends all chunk bytes to every peer" behavior
- the network only transfers chunk bytes for actual demand

## Reorg Behavior

If a block is validated, availability is announced, and that block is later reorged out:

- provider hints are harmless and expire by TTL
- cached chunks are still valid Publish-ledger data
- future pulls still independently verify the chunk against authoritative migrated state

The announcement is only a hint about who may serve, not a commitment about chain state.

## Backward Compatibility

The pull-response side remains:

- request: `GossipDataRequestV2::PdChunk(u32, u64)`
- response: `GossipDataV2::PdChunk(ChunkFormat)`

The new wire type is additive:

- push/control-plane: `GossipDataV2::PdChunkAvailability(PdChunkAvailabilityBatch)`

No production network depends on a deployed PD chunk-byte push format, so introducing availability gossip is a safe direction.

## Changes by Crate

### `crates/types`

- add `PdChunkRange`
- add `PdChunkAvailabilityBatch`
- add `GossipDataV2::PdChunkAvailability(...)`
- add `GossipCacheKey::PdChunkAvailability(H256)`
- extend `ChunkStorageProvider` with `has_chunk_for_pd`

### `crates/p2p`

- add `PdChunkProviderCache`
- add `/gossip/v2/pd_chunk_availability`
- add handler for `PdChunkAvailabilityBatch`
- add targeted fanout send helper for PD availability
- update `GossipPdChunkFetcher` to merge provider hints with canonical assignee peers before pull
- add per-route body limit and per-peer byte/chunk rate limiting

### `crates/domain`

- implement `get_chunk_for_pd` cache-first lookup through `CachedChunks`

### `crates/actors`

- reuse `BlockStateUpdated::Valid` subscription point
- no full chunk-byte push orchestration
- keep existing pull verification logic

### Shared Utilities

- reuse `extract_pd_chunk_specs_from_block`
- reuse `specs_to_ledger_offsets`
- reuse `derive_chunk_verification_info`
- add `coalesce_ledger_offsets_to_ranges`

## Summary

The network problem is not that validators learn too much about PD chunks; it is that the original push design tried to move the chunk bytes themselves through gossip.

The safer design is:

- gossip **who can serve**
- pull **what you actually need**
- let any validator that already has a chunk become a provider
- keep the data plane verified and on-demand

That preserves the original goal of spreading load away from partition assignees without introducing a full-mesh chunk flood.
