# Node-internal block-follower API (`/internal/*`)

The `/internal/*` HTTP surface serves the gateway block follower and its verification pipeline. It
exposes the node's durable block-event log two ways — a push stream and an equivalent paged poll — plus
canonical block reads and an unpacked chunk-range read. Both transports carry the **same** `StreamFrame`s
from the **same** seq-keyed log, so a follower may use either and reach identical state.

> **Security / runtime.** These routes carry no application-layer authentication. They are mounted only
> when the node sets `http.expose_internal_api` (off by default). That same flag starts the durable
> block-stream producer that appends `observed` / `finalized` / `reorged` frames; when the flag is off
> the producer is not started and no stream log is written. When enabled they ride the same HTTP
> listener as the public API. A deployment that enables them **must** restrict `/internal/*` at the
> network layer (firewall / reverse proxy / bind address) to the trusted gateway.
>
> **Flag toggle gap.** If the flag was on (log written), then off across restarts (no appends, no
> prune), then on again, `seq` stays contiguous but the event history has a hole for every block that
> migrated while the producer was disabled. Startup reconciliation only backfills missing `finalized`
> frames within `RECONCILE_SCAN_CAP` of the index tail; after a long disabled window it may abort and
> log a warning. Followers that resume from an old cursor will not receive a `truncated` signal for
> that semantic hole — they must re-bootstrap from canonical block reads. A stronger guarantee would
> require recording the disable period (e.g. a reset-marker frame).

## The event log and its cursor

The node appends `observed` / `finalized` / `reorged` events to a durable, append-only log keyed by a
monotonic `seq` (the 0-based append index). Within one log lifetime the **durable log** never rewinds
or repeats a `seq` — that is the follower's resume cursor (never height, which repeats across forks).

The **live SSE channel** is best-effort delivery of already-durable frames. In steady state the single
block-tree task enqueues in commit order so live order matches `seq`. At startup, reconciliation can
append frames that fan out before earlier writer frames still queued on the channel, so a connected
subscriber may briefly see `N+1` before `N`. Consumers should therefore track the highest *contiguous*
`seq` they have processed, not the highest `seq` they have seen, and ignore only frames at or below that
watermark. A frame that arrives above the watermark leaves a gap: buffer it until the missing frames
arrive, or re-sync from the first missing `seq` via poll/replay. Discarding on `seq <= highest seen`
instead loses `N` whenever `N+1` overtakes it. Poll pages always read the log in ascending `seq`.

The log is pruned: once it exceeds `RETENTION_EVENTS` (100,000) the oldest events are deleted (batched, so
the retained count is ~100k with up to `PRUNE_INTERVAL` overshoot). Two quantities therefore matter:

- **`logical_len`** — the highest `seq` ever appended, plus one. This is *not* the retained row count;
  after pruning they differ.
- **`lowest_retained_seq`** — the lowest `seq` still held (the prune floor). It advances over time.

## `GET /internal/blocks/stream?from_seq=`

A Server-Sent Events stream. Replays the durable suffix from `from_seq`, then tails live frames, each
framed as `data: {json}\n\n`. A lagging subscriber is dropped and reconnects with `from_seq` to replay
from the log. The connection does not close on its own; a reader stops at a chosen `seq`. A cursor past
the tip (`from_seq > logical_len`, only after a reset) replays from the retained floor — the same
beyond-tip clamp as the poll endpoint — so the follower sees below-`seq` frames and rewinds.

## `GET /internal/blocks/events?from_seq=&limit=`

The poll half: a bounded JSON page over the same log, for consumers that cannot hold a stream open. It
registers no live subscriber and reads in a single transaction.

**Query**

- `from_seq` — inclusive resume cursor; default `0`.
- `limit` — page size; default `256`, clamped to `MAX_PAGE` (1024). Over-size is clamped, not rejected.
  `limit=0` is a valid zero-frame probe.

**Response — `200 application/json`**

```jsonc
{
  "from_seq": 100,              // echoed
  "frames": [ /* StreamFrame, ascending seq, contiguous from the page's start */ ],
  "next_seq": 164,              // start + frames.len(); the next poll's from_seq
  "has_more": true,             // next_seq < logical_len
  "lowest_retained_seq": 0,     // prune floor (0 if unpruned)
  "truncated": false            // true iff from_seq < lowest_retained_seq
}
```

**Cursor regimes** (all `200`):

| `from_seq` vs the window | behaviour | `truncated` |
| --- | --- | --- |
| in-window / at-tip (`lowest_retained_seq..=logical_len`) | page from `from_seq`; `from_seq == logical_len` is a normal empty page (caught up) | `false` |
| below the floor (`< lowest_retained_seq`) | empty page; `next_seq` is the floor the follower resyncs forward to; the requested span was pruned | `true` |
| beyond the tip (`> logical_len`) | clamp to `lowest_retained_seq` (`0` on a fresh log) | `false` |

A `truncated` page is a resync signal: it carries no frames, and `next_seq` is the floor
(`lowest_retained_seq`). The follower discards any frames, force-resets its cursor forward to `next_seq`,
re-bootstraps current state (the reads below) up to the floor, and resumes streaming from there. The
endpoint never silently returns a page whose first `seq` exceeds `from_seq` without `truncated`.

**Equivalence.** For every in-window `seq`, the frame `/events` returns equals the frame the SSE stream
would push for that `seq`; concatenating poll pages from `from_seq=0` yields the identical sequence as the
SSE stream from `0`.

## Canonical reads

- `GET /internal/blocks/{height}` — the canonical block at `height` as a `BlockEvent`, or `404`.
- `GET /internal/blocks?from_height=&to_height=` — the canonical blocks in `[from, to]`, ascending. The
  span is bounded by `MAX_BLOCK_RANGE` (1000); a larger span is `400`.

These return current canonical state, not transition history; they back a follower's reconciliation after
a `truncated` poll.

## `GET /internal/chunks?ledger=&offset=from-to`

The stored chunks of one inclusive absolute-ledger-offset span, **unpacked** (packing entropy reversed,
tail chunk trimmed to the data). This backs the gateway's verification pipeline: the relay reads bodies
back in 8-chunk windows to verify them against the block's committed `data_root`, and the data service
acquires extents through the same route.

**Query**

- `ledger` — numeric `DataLedger` id (0 = Publish, 1 = Submit, 10 = OneYear, 20 = ThirtyDay).
- `offset` — the inclusive span, `from-to`, in absolute ledger chunk offsets (the space `tx_start_offset`
  in a `StreamFrame`'s `TxMeta` points into). The span is bounded by `MAX_CHUNK_SPAN` (64 chunks); a wider
  span is `400`.

**Response — `200 application/json`**

```jsonc
[
  {
    "ledger_id": 1,
    "offset": 42,            // absolute ledger chunk offset
    "bytes": [7, 0, 255],    // unpacked chunk data, plain integer array (not Base64)
    "proof": [1, 2, 3]       // the chunk's data_path; validates against the tx's data_root
  }
]
```

`bytes` and `proof` are JSON integer arrays, unlike the `Base64` strings of the public chunk routes — the
gateway decodes them as raw byte vectors.

**Short reads.** A chunk the node does not hold is omitted from the array; a fully unstored span is `[]`.
The response is still `200`: the gateway reads a shortfall as "this node lacks the range" and fails over
to another node, so absence must stay distinguishable from a malformed request. Consumers must therefore
size requests within `MAX_CHUNK_SPAN`, or a conforming node would answer with a permanently short page.

## Error and probe conventions

- `200` for any valid `from_seq` / `limit`, including every cursor regime above, and for any well-formed
  chunk span (however little of it is stored).
- `400` only for an unparsable `from_seq` / `limit`, a range read exceeding `MAX_BLOCK_RANGE`, or a chunk
  request that is malformed (bad `ledger`, unparsable or inverted `offset`) or wider than `MAX_CHUNK_SPAN`.
- `5xx` only on a genuine log-read fault.
- **`404` means the endpoint is not available.** The `/internal` routes are mounted only when the node
  sets `http.expose_internal_api` (off by default); with the flag off the durable producer is also not
  started. A node with it disabled, or an older build that lacks the route entirely, serves a normal
  not-found. The gateway's transport selector treats `404` / connection-refused as "this transport is
  unsupported" and falls back accordingly. When the routes are mounted they never return `404` for an
  empty, short, or out-of-range log. The one exception is `GET /internal/blocks/{height}`, whose `404`
  also means "no canonical block at that height" — a transport prober must key on the stream/events
  routes, never on the by-height read.

## Durability: frames commit with their transitions

Production frames are appended inside the same consensus database transaction as the state
transition they report: confirmation writes `observed`/`reorged` frames with its metadata, and
migration writes `finalized` with its block-index push. A crash therefore cannot lose a frame for a
transition that persisted — the two are atomic — and a frame cannot exist for a transition that did
not. The producer's remaining append path is startup reconciliation, which repairs `finalized`
frames that a pre-atomic-append build lost between a migration commit and its separate producer
append, walking the block index tail and appending the gap in height order. Live SSE delivery is
best-effort on top of this durable log: a committed frame whose fan-out is missed (say, a halted
producer) is recovered through replay on reconnect for as long as it remains retained. Only a cursor
that has fallen below `lowest_retained_seq` is unrecoverable by replay — that is the `truncated`
case, which returns no frames and requires a re-bootstrap from canonical block reads.

**FCU timing.** An `observed` (or `reorged`) frame becomes durable when confirmation's consensus
txn commits, which is **before** the execution-layer fork-choice update (FCU) for that tip.
Live SSE defers fan-out until after a successful FCU ack; the poll transport reads the log with no
FCU gate, so a poll can return `observed` for a block whose EL head has not advanced yet (window:
one FCU round-trip). Presence in the log does not imply the EL head has moved. Gateways that
cross-read `eth_*` at `latest` on `observed` must tolerate that lag or wait for EL confirmation
separately.

## Limitation: log recreation (reset)

`seq` is node-local and not stable across a log recreation. The log is stripped from snapshots, so a
snapshot restore or DB wipe restarts it at `seq 0` while block headers survive. The beyond-tip clamp gives
only *partial* reset detection (it fires only when the follower's cursor exceeds the new, shorter log's
tip), and low-`seq` frames do not by themselves recover lost history. Robust handling needs a
generation/epoch identifier — a `(stream_id, seq)` cursor across both transports — which is a planned
follow-up. For now a node reset requires an operator-coordinated follower re-bootstrap.
