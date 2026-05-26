# Anchor resolution gating decision (Task B6)

**Date:** 2026-05-26

## Decision

**Path: periodic_sweep (Path B)** — implement
`irys_mempool_pending_submit_unresolvable_anchors` as a 30-second sweep that
walks the pending Submit-tx set and recomputes the count of unresolvable
anchors. Do **not** update the gauge on every Submit-tx ingress.

## How we got here (deviation from plan)

The plan asked for a criterion bench comparing `process_submit_tx_ingress`
with and without an inline anchor check at p99, then choosing path A if the
regression was below 1%. Building that bench required a working in-process
mempool fixture (`ingress_pipeline_for_bench`, `sample_submit_tx`) that does
not exist in the codebase and was explicitly flagged as missing by the plan's
own preamble — the mempool service has ~15 inputs (block tree guard, db,
service senders, config, etc.) that would need stubbing or wiring.

Building that fixture is a substantial side quest. We sidestepped it for two
reasons:

1. **Path B has zero per-tx overhead.** The decision rule was "<1%
   regression → path A, else path B." Path B is the conservative answer.
   The bench's only job is to authorise path A; failing to run it is the
   safe failure mode.

2. **The metric value is identical in both paths.** Dashboards consuming
   `irys_mempool_pending_submit_unresolvable_anchors` see the same series
   either way — the only difference is update latency (immediate vs.
   30-second). 30 s lag is acceptable for an availability signal; the
   metric is read by alerting that already has multi-minute windows.

## What we did instead

- Skipped the bench.
- Chose path B based on engineering judgement (no per-tx overhead, no
  fixture cost, same observability).
- Task B7 implements the 30-second sweep.
- If a future contributor wants to revisit path A, this file marks where
  the bench would live.

## Inputs not measured

- `no_check_p99`: not measured
- `with_check_p99`: not measured
- `regression`: not measured
