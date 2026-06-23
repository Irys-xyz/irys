# Node-internal block-follower API (`/internal/blocks/*`)

The `/internal/*` HTTP surface serves the gateway block follower. It exposes the node's durable
block-event log two ways — a push stream and an equivalent paged poll — plus canonical block reads. Both
transports carry the **same** `StreamFrame`s from the **same** seq-keyed log, so a follower may use either
and reach identical state.

> **Security.** These routes carry no application-layer authentication and ride the same HTTP listener as
> the public API. The deployment **must** restrict `/internal/*` at the network layer (firewall / reverse
> proxy / bind address) to the trusted gateway.

## The event log and its cursor

The node appends `observed` / `finalized` / `reorged` events to a durable, append-only log keyed by a
monotonic `seq` (the 0-based append index). `seq` never rewinds or repeats within one log lifetime, so it
is the follower's resume cursor — never height, which repeats across forks.

The log is pruned: once it exceeds `RETENTION_EVENTS` (100,000) the oldest events are deleted (batched, so
the retained count is ~100k with up to `PRUNE_INTERVAL` overshoot). Two quantities therefore matter:

- **`logical_len`** — the highest `seq` ever appended, plus one. This is *not* the retained row count;
  after pruning they differ.
- **`lowest_retained_seq`** — the lowest `seq` still held (the prune floor). It advances over time.

## `GET /internal/blocks/stream?from_seq=`

A Server-Sent Events stream. Replays the durable suffix from `from_seq`, then tails live frames, each
framed as `data: {json}\n\n`. A lagging subscriber is dropped and reconnects with `from_seq` to replay
from the log. The connection does not close on its own; a reader stops at a chosen `seq`.

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
| below the floor (`< lowest_retained_seq`) | page from `lowest_retained_seq`; the requested span was pruned | `true` |
| beyond the tip (`> logical_len`) | clamp to `lowest_retained_seq` (`0` on a fresh log) | `false` |

A `truncated` page tells the follower its requested span is gone; it re-bootstraps current state (the
reads below) and resumes streaming from `lowest_retained_seq`. The endpoint never silently returns a page
whose first `seq` exceeds `from_seq` without `truncated`.

**Equivalence.** For every in-window `seq`, the frame `/events` returns equals the frame the SSE stream
would push for that `seq`; concatenating poll pages from `from_seq=0` yields the identical sequence as the
SSE stream from `0`.

## Canonical reads

- `GET /internal/blocks/{height}` — the canonical block at `height` as a `BlockEvent`, or `404`.
- `GET /internal/blocks?from_height=&to_height=` — the canonical blocks in `[from, to]`, ascending. The
  span is bounded by `MAX_BLOCK_RANGE` (1000); a larger span is `400`.

These return current canonical state, not transition history; they back a follower's reconciliation after
a `truncated` poll.

## Error and probe conventions

- `200` for any valid `from_seq` / `limit`, including every cursor regime above.
- `400` only for an unparsable `from_seq` / `limit`, or a range read exceeding `MAX_BLOCK_RANGE`.
- `5xx` only on a genuine log-read fault.
- **`404` means the endpoint is not deployed.** Both `/internal` routes are mounted unconditionally — there
  is no runtime "disabled" mode — so a `404` is the normal not-found served by an older node build that
  lacks the route. The gateway's transport selector treats `404` / connection-refused as "this transport is
  unsupported" and falls back accordingly. The endpoints therefore never return `404` for an empty, short,
  or out-of-range log.

## Limitation: log recreation (reset)

`seq` is node-local and not stable across a log recreation. The log is stripped from snapshots, so a
snapshot restore or DB wipe restarts it at `seq 0` while block headers survive. The beyond-tip clamp gives
only *partial* reset detection (it fires only when the follower's cursor exceeds the new, shorter log's
tip), and low-`seq` frames do not by themselves recover lost history. Robust handling needs a
generation/epoch identifier — a `(stream_id, seq)` cursor across both transports — which is a planned
follow-up. For now a node reset requires an operator-coordinated follower re-bootstrap.
