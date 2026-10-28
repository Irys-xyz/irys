# ADR 0001: HTTP chunk POST acknowledges on enqueue

**Status:** Accepted

## Context

`POST /v1/chunk` previously waited on a oneshot until `handle_chunk_ingress_message` had finished merkle validation, the MDBX cache commit introduced in #1558, storage-module write/pack, gossip send, and the ingress-proof check. That made the HTTP status a durability signal. It also held Actix workers across packing I/O, which is the latency this change removes.

Gossip (`POST /gossip/v2/chunk` and v1) and data-sync still need the ingest result: they score peers and retry on `ChunkIngressError`. Those paths keep the oneshot.

Three acknowledgement contracts were considered:

1. Ack on enqueue — HTTP 200 means the body is admitted, not that it is valid or durable.
2. Ack after merkle validation — keep HTTP 400 for bad proofs; packing still asynchronous.
3. Ack after MDBX commit — keep #1558 on the public route; packing still asynchronous.

## Decision

We take (1). Public `POST /v1/chunk` returns 200 as soon as the chunk is admitted onto the existing unbounded `chunk_ingress` channel.

Admission is an HTTP-only semaphore (`max_http_chunk_admission`, default 256) held until the ingress worker finishes. Because Actix decodes `Json<UnpackedChunk>` before the handler can wait, a second semaphore (`max_http_chunk_waiters`, default 32) bounds decoded bodies that are waiting for an admission permit. The wait uses monotonic `tokio::time::timeout` (`http_chunk_admission_timeout_millis`, default 5000). Timeout or waiter saturation returns 503. Channel-closed send returns 500. Malformed JSON remains 400 from the extractor.

Fire-and-forget `IngestChunk` must not park the recv loop and must not spawn `acquire().await` waiters on the shared chunk-lane semaphore: Tokio assigns released permits to that waiter queue before `try_acquire`, which would starve gossip. Saturated HTTP ingests go onto an in-service `VecDeque` drained when a chunk-lane permit frees. Reply-bearing callers still receive `Advisory(Overloaded)` immediately. Control-plane fire-and-forget still parks.

## Consequences

- Public POST no longer reports `InvalidProof`, `InvalidDataHash`, or MDBX flush failure. Those remain worker logs/metrics and gossip/data-sync errors. #1558 durability is worker-only on this route.
- Worst-case retained HTTP bodies are `(256 + 32) × 1 MiB` at the JSON limit. Operators who cannot afford that lower the two semaphore knobs.
- Integration helpers that treated HTTP 200 as “chunk is stored” must poll (`ChunkProvider`, cache, or pending membership) after 200.
- Gossip saturation behaviour is unchanged for oneshot callers. An HTTP flood can still occupy the chunk lane with *running* handlers, but it cannot HOL-block recv or queue Tokio waiters ahead of gossip `try_acquire`.

## Amendment (2026-10-28): HTTP ingest ends at the cache commit

On devnet the admission permit lived for about 62 s under packing load, because the ingress worker awaited the storage-module write, which waits for an entropy read on a busy disk. Every permit was held, and most public POSTs returned 503 while the node still had cache headroom. The full analysis is in `agents/2026-10-28-002-bug-http-admission-decouple-design.md`.

For an ingest that holds an HTTP admission guard, the worker now validates the body, commits it to `CachedChunks`, queues it on each assigned storage module's disk lane with `StorageModule::enqueue_ingress_batch`, pushes it to peers, checks the ingress proof and finishes. It does not wait for the disk lane to pack the body, so the admission permit and the chunk-lane permit are released after the cache commit. A storage module whose pending bytes have reached its pending-memory ceiling is skipped; the body stays in the cache, which keeps a body this node must store until it is durable, and data sync writes it from there. `mempool.http_ingest_waits_for_module_write = true` restores the waiting write. Gossip, data-sync and pending-drain ingests are unchanged.

Admission gains a second 503 cause, `cache_full`: once `CachedChunks` holds `cache.max_cache_size_bytes` or more, new public bodies are refused before an admission permit is taken. This is a rejection watermark rather than a cap. Bodies already admitted or waiting still land, so the HTTP overshoot is at most `(max_http_chunk_admission + max_http_chunk_waiters)` bodies, and gossip, data-sync and pending-drain writes do not pass through the check.

The memory bound in Consequences covers bodies owned by admission and the ingress worker. It does not cover push gossip: the receiving node still pushes full bodies, each push runs as a detached task holding the serialised body until every peer send finishes, and those tasks are fed through an unbounded channel. With admission no longer paced by the disk, that retention is measured on devnet and bounded separately if the measurement warrants it.
