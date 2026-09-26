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
