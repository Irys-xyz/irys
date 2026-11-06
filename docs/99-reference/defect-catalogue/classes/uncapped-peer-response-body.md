+++
id = "uncapped-peer-response-body"
severity = "dos"
scope = "code that reads an HTTP response body from a peer, error paths included"
flag = "`.json()`, `.text()` or `.bytes()` on a peer response, with no byte cap before the full body is read"
paths = ["crates/p2p/**", "crates/actors/**", "crates/api-client/**", "crates/chain/**"]
patterns = ['\.(json(::<.+>)?|text|bytes)\(\)(\s*\.await|\s*$)']
+++

## Pattern

Code reads the full body of a peer HTTP response with `reqwest`'s `.json()`, `.text()` or
`.bytes()`. These read the whole body into memory before any size check:

```rust
let response = client.get(&url).send().await?;
let chunk: ChunkFormat = response.json().await?;
```

Error paths have the same defect, for example `response.text().await.unwrap_or_default()` to log
the error body.

## Why it is a defect

A peer controls the size of its response body. Without a limit, one peer can send a very large body
and make the node use too much memory, until it stops (DoS). This applies to every peer HTTP read:
gossip, sync, block and chunk fetches, health checks, and the error bodies of each.

## Correct form

Read the body in pieces and stop at the configured limit (`max_peer_response_bytes`, default
32 MiB), before parsing. Use `irys_types::peer_response::accumulate_response_bytes` to add each piece
to the total; it returns `ResponseBodyTooLarge` past the limit. The existing readers that use it are
`GossipClient::read_capped_bytes`, `IrysApiClient::read_capped_bytes`, and
`read_capped_bytes` in `crates/actors/src/data_sync_service/chunk_fetcher.rs`. Parse the returned
bytes with `serde_json::from_slice`.

## Detection

`cargo xtask defects scan`: the pattern matches `.json()`, `.json::<T>()`, `.text()` and `.bytes()`
followed by `.await` on the same line, or at the end of a line (a chain that rustfmt splits). It
misses a body read through a helper that hides the call, and a read outside the listed crates. It
also matches requests to non-peer services; check that the response comes from a peer. Manual check:
for each new `client.get(..).send()` to a peer, follow the response to where the body is read.
