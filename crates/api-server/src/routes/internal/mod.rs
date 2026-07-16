//! The `/internal/*` HTTP surface: the block-stream SSE endpoint, the canonical block reads, and
//! the unpacked chunk-range read.
//!
//! These routes serve the gateway block follower and its verification pipeline. The block wire
//! shapes live in [`irys_types::block_stream`]. `stream` is registered before `{height}` so it is
//! matched as a literal segment rather than captured as a height.
//!
//! SECURITY: these endpoints carry no application-layer authentication. They are mounted only when
//! `http.expose_internal_api` is set (off by default); when enabled they ride the same HTTP listener
//! as the public API. They expose internal block data and a long-lived SSE stream, so a deployment
//! that enables them MUST restrict `/internal/*` at the network layer (firewall / reverse proxy /
//! bind address) to the trusted gateway.

use crate::ApiState;
use crate::error::ApiError;
use actix_web::{HttpResponse, dev::HttpServiceFactory, web};
use irys_actors::block_tree_service::get_block_header;
use irys_database::db::IrysDatabaseExt as _;
use irys_domain::BlockTreeEntry;
use irys_types::block_stream::{BlockEvent, EventsPage, StreamFrame};
use irys_types::{H256, LedgerChunkOffset};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use tracing::debug;

pub fn internal_routes() -> impl HttpServiceFactory {
    web::scope("internal")
        .route("/blocks/stream", web::get().to(blocks_stream))
        .route("/blocks/events", web::get().to(blocks_events))
        .route("/blocks/{height}", web::get().to(block_by_height))
        .route("/blocks", web::get().to(blocks_range))
        .route("/chunks", web::get().to(chunks_range))
}

#[derive(Debug, Deserialize)]
struct StreamQuery {
    #[serde(default)]
    from_seq: u64,
}

/// `GET /internal/blocks/stream?from_seq=` — the SSE stream. Replays the durable suffix `[start, end)`
/// captured atomically by `subscribe` (looping the same poll primitive via `replay_page`), then tails the
/// live frames, each framed as `data: {json}\n\n`.
async fn blocks_stream(
    state: web::Data<ApiState>,
    query: web::Query<StreamQuery>,
) -> Result<HttpResponse, ApiError> {
    let (start, end, mut live) = state
        .block_stream
        .subscribe(query.from_seq)
        .map_err(|e| ApiError::Internal { err: e.to_string() })?;
    let handle = Arc::clone(&state.block_stream);
    let body = async_stream::try_stream! {
        // Replay the immutable [start, end) snapshot one bounded page at a time. replay_page caps each
        // page by `end` (not events_page's has_more, which tracks a moving tip), so the live tail begins
        // exactly at seq=end.
        let mut cursor = start;
        while let Some((next, frames)) = handle
            .replay_page(cursor, end)
            .map_err(actix_web::error::ErrorInternalServerError)?
        {
            for frame in &frames {
                yield sse_bytes(frame)?;
            }
            cursor = next;
        }
        if cursor < end {
            // replay_page returned None before reaching `end`: a prune advanced the floor past the cursor
            // mid-replay. End the SSE as a clean EOF so the follower reconnects and resyncs — do not fall
            // through to the live tail (that would push a seq gap onto the wire).
            return;
        }
        // Live tail [end, ..). `recv` yields `None` when the producer halts and drops the sender, ending
        // the SSE cleanly after replay.
        while let Some(frame) = live.recv().await {
            yield sse_bytes(&frame)?;
        }
    };

    Ok(HttpResponse::Ok()
        .content_type("text/event-stream")
        // Explicit error type: try_stream! cannot infer it through actix's generic `.streaming`, which
        // only bounds `E: Into<BoxError>`.
        .streaming::<_, actix_web::Error>(body))
}

fn sse_bytes(frame: &StreamFrame) -> Result<web::Bytes, actix_web::Error> {
    let json = serde_json::to_string(frame).map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(web::Bytes::from(format!("data: {json}\n\n")))
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    #[serde(default)]
    from_seq: u64,
    limit: Option<u64>,
}

/// Default page size for `GET /internal/blocks/events` when `limit` is omitted.
const DEFAULT_EVENTS_PAGE: u64 = 256;

/// `GET /internal/blocks/events?from_seq=&limit=` — a bounded JSON page over the same durable event log
/// the SSE stream tails (the poll half of the follower contract). The page envelope (`next_seq`,
/// `has_more`, `truncated`, `lowest_retained_seq`) is built by `events_page` on the block-stream handle.
async fn blocks_events(
    state: web::Data<ApiState>,
    query: web::Query<EventsQuery>,
) -> Result<web::Json<EventsPage>, ApiError> {
    let page = state
        .block_stream
        .events_page(query.from_seq, query.limit.unwrap_or(DEFAULT_EVENTS_PAGE))
        .map_err(|e| ApiError::Internal { err: e.to_string() })?;
    Ok(web::Json(page))
}

/// `GET /internal/blocks/{height}` — the canonical block at `height` as a `BlockEvent`, or `404`.
async fn block_by_height(
    state: web::Data<ApiState>,
    path: web::Path<u64>,
) -> Result<web::Json<BlockEvent>, ApiError> {
    let height = path.into_inner();
    // Resolve through the same stabilised snapshot the range path uses (a single-height span), so a
    // reorg landing mid-read cannot return a block that has just been orphaned.
    snapshot_canonical_range(&state, height, height)?
        .into_iter()
        .next()
        .map(|source| resolve_snapshot_block(&state, source))
        .transpose()?
        .map(web::Json)
        .ok_or(ApiError::ErrNoId {
            id: height.to_string(),
            err: "no canonical block at height".to_string(),
        })
}

#[derive(Debug, Deserialize)]
struct RangeQuery {
    from_height: u64,
    to_height: u64,
}

/// Max number of heights one range request may span, to bound the per-request DB work.
const MAX_BLOCK_RANGE: u64 = 1_000;
const CANONICAL_SNAPSHOT_ATTEMPTS: usize = 3;

/// `GET /internal/blocks?from_height=&to_height=` — the canonical blocks in `[from, to]`, ascending.
async fn blocks_range(
    state: web::Data<ApiState>,
    query: web::Query<RangeQuery>,
) -> Result<web::Json<Vec<BlockEvent>>, ApiError> {
    // An inverted range is a malformed request, not an empty span: answering `200 []` here would
    // be indistinguishable from a genuinely empty canonical range on the endpoint a follower uses
    // to reconcile after truncation.
    if query.from_height > query.to_height {
        return Err(ApiError::InvalidBlockParameter {
            parameter: format!(
                "block range {}..={} is inverted (from_height exceeds to_height)",
                query.from_height, query.to_height
            ),
        });
    }
    // Inclusive count: the handler resolves `from_height..=to_height`, so a span of N covers N + 1
    // heights. Cap the inclusive count so the documented MAX_BLOCK_RANGE bound is exact.
    let height_count = query
        .to_height
        .saturating_sub(query.from_height)
        .saturating_add(1);
    if height_count > MAX_BLOCK_RANGE {
        return Err(ApiError::InvalidBlockParameter {
            parameter: format!(
                "block range {}..={} spans {height_count} heights, exceeding the maximum {MAX_BLOCK_RANGE}",
                query.from_height, query.to_height
            ),
        });
    }
    let snapshot = snapshot_canonical_range(&state, query.from_height, query.to_height)?;
    let events = snapshot
        .into_iter()
        .map(|source| resolve_snapshot_block(&state, source))
        .collect::<Result<Vec<_>, _>>()?;
    if !events.windows(2).all(|pair| {
        pair[0].header.height.checked_add(1) == Some(pair[1].header.height)
            && pair[1].header.previous_block_hash == pair[0].header.block_hash
    }) {
        return Err(ApiError::Internal {
            err: "canonical block range snapshot is not a contiguous parent-linked chain"
                .to_string(),
        });
    }
    Ok(web::Json(events))
}

#[derive(Debug, Deserialize)]
struct ChunksQuery {
    ledger: u32,
    offset: String,
}

/// Max chunks one `/internal/chunks` request may span. Sized above the gateway's request sizes —
/// the relay verifies bodies in 8-chunk fetches and the data service acquires extents of up to
/// 16 MiB (64 chunks) — because a conforming consumer must never see a capped response: it would
/// misread the shortfall as the node lacking the data.
const MAX_CHUNK_SPAN: u64 = 64;

/// Concurrent `/internal/chunks` unpack jobs across all requests. Each job recomputes packing
/// entropy for up to [`MAX_CHUNK_SPAN`] chunks on the blocking pool, so without admission control
/// a burst of requests could saturate that pool and starve its other users (DB reads, file I/O).
static UNPACK_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

/// One chunk of `GET /internal/chunks`, unpacked. `bytes` and `proof` serialise as JSON integer
/// arrays (plain `Vec<u8>`), NOT the node's usual `Base64` strings — the gateway decodes them as
/// `Vec<u8>`. `proof` is the chunk's `data_path`, which gateway consumers `validate_path` against
/// the transaction's `data_root`.
#[derive(Debug, Serialize)]
struct ChunkRead {
    ledger_id: u32,
    offset: u64,
    bytes: Vec<u8>,
    proof: Vec<u8>,
}

/// Parses the `offset=from-to` query form into an inclusive span.
fn parse_offset_span(s: &str) -> Option<(u64, u64)> {
    let (from, to) = s.split_once('-')?;
    let from: u64 = from.parse().ok()?;
    let to: u64 = to.parse().ok()?;
    (from <= to).then_some((from, to))
}

/// `GET /internal/chunks?ledger=&offset=from-to` — the stored chunks of one inclusive
/// absolute-ledger-offset span, unpacked. Chunks the node does not hold are omitted rather than
/// erred: the gateway reads a shortfall as "this node lacks the range" and fails over to another
/// node, so absence must stay distinguishable from a malformed request (which 400s).
async fn chunks_range(
    state: web::Data<ApiState>,
    query: web::Query<ChunksQuery>,
) -> Result<web::Json<Vec<ChunkRead>>, ApiError> {
    let ledger = crate::routes::parse_ledger_id(query.ledger)?;
    let Some((from, to)) = parse_offset_span(&query.offset) else {
        return Err(ApiError::InvalidBlockParameter {
            parameter: format!(
                "offset span {:?} is malformed: expected from-to with from <= to",
                query.offset
            ),
        });
    };
    if to - from >= MAX_CHUNK_SPAN {
        return Err(ApiError::InvalidBlockParameter {
            parameter: format!(
                "chunk span {from}-{to} exceeds the maximum of {MAX_CHUNK_SPAN} chunks"
            ),
        });
    }
    // Unpacking recomputes packing entropy per chunk (CPU-bound, millions of hash rounds at
    // mainnet packing strength), so the read loop runs on the blocking pool, gated by
    // [`UNPACK_GATE`]. The semaphore is never closed, so acquire cannot fail.
    let _permit = UNPACK_GATE
        .acquire()
        .await
        .map_err(|e| ApiError::Internal { err: e.to_string() })?;
    let provider = Arc::clone(&state.chunk_provider);
    let chunks = web::block(move || {
        let mut out = Vec::new();
        for offset in from..=to {
            match provider
                .get_unpacked_chunk_by_ledger_offset(ledger, LedgerChunkOffset::from(offset))
            {
                Ok(Some(unpacked)) => out.push(ChunkRead {
                    ledger_id: query.ledger,
                    offset,
                    bytes: unpacked.bytes.0,
                    proof: unpacked.data_path.0,
                }),
                // Not stored at this offset: omit (the short-read contract).
                Ok(None) => {}
                Err(e) => {
                    debug!(?ledger, offset, "internal chunk read failed: {e:#}");
                }
            }
        }
        out
    })
    .await
    .map_err(|e| ApiError::Internal { err: e.to_string() })?;
    Ok(web::Json(chunks))
}

enum CanonicalBlockSource {
    InTree(BlockTreeEntry),
    Migrated { height: u64, block_hash: H256 },
}

fn snapshot_canonical_range(
    state: &ApiState,
    from_height: u64,
    to_height: u64,
) -> Result<Vec<CanonicalBlockSource>, ApiError> {
    for _ in 0..CANONICAL_SNAPSHOT_ATTEMPTS {
        let entries = state.block_tree.read().get_canonical_chain().0;
        let signature: Vec<(u64, H256)> = entries
            .iter()
            .map(|entry| (entry.height(), entry.block_hash()))
            .collect();
        let mut in_tree: BTreeMap<u64, BlockTreeEntry> = entries
            .into_iter()
            .filter(|entry| (from_height..=to_height).contains(&entry.height()))
            .map(|entry| (entry.height(), entry))
            .collect();
        let snapshot = snapshot_with_index(state, from_height, to_height, &mut in_tree)?;
        let unchanged = state
            .block_tree
            .read()
            .get_canonical_chain()
            .0
            .iter()
            .map(|entry| (entry.height(), entry.block_hash()))
            .eq(signature);
        if unchanged {
            return Ok(snapshot);
        }
    }
    Err(ApiError::Internal {
        err: "canonical chain changed repeatedly while snapshotting block range".to_string(),
    })
}

fn snapshot_with_index(
    state: &ApiState,
    from_height: u64,
    to_height: u64,
    in_tree: &mut BTreeMap<u64, BlockTreeEntry>,
) -> Result<Vec<CanonicalBlockSource>, ApiError> {
    state
        .db
        .view_eyre(|tx| {
            let mut snapshot = Vec::new();
            for height in from_height..=to_height {
                if let Some(entry) = in_tree.remove(&height) {
                    snapshot.push(CanonicalBlockSource::InTree(entry));
                } else if let Some(block_hash) =
                    irys_database::block_index_hash_by_height(tx, height)?
                {
                    snapshot.push(CanonicalBlockSource::Migrated { height, block_hash });
                }
            }
            Ok(snapshot)
        })
        .map_err(|e| ApiError::Internal { err: e.to_string() })
}

fn resolve_snapshot_block(
    state: &ApiState,
    source: CanonicalBlockSource,
) -> Result<BlockEvent, ApiError> {
    match source {
        CanonicalBlockSource::InTree(entry) => Ok(BlockEvent::from_sealed(
            entry.sealed_block(),
            state.config.consensus.chunk_size,
        )),
        CanonicalBlockSource::Migrated { height, block_hash } => {
            resolve_block_hash(state, block_hash)?.ok_or_else(|| ApiError::Internal {
                err: format!(
                    "canonical range snapshot block {block_hash} at height {height} is unavailable"
                ),
            })
        }
    }
}

fn resolve_block_hash(state: &ApiState, block_hash: H256) -> Result<Option<BlockEvent>, ApiError> {
    let chunk_size = state.config.consensus.chunk_size;
    if let Some(sealed) = state.block_tree.read().get_sealed_block(&block_hash) {
        return Ok(Some(BlockEvent::from_sealed(&sealed, chunk_size)));
    }

    let header = get_block_header(&state.block_tree, &state.db, block_hash, false)
        .map_err(|e| ApiError::Internal { err: e.to_string() })?;
    let Some(header) = header else {
        return Ok(None);
    };

    // One read transaction resolves every ledger's tx headers — a migrated block can carry
    // hundreds of ids, and a range request resolves up to MAX_BLOCK_RANGE blocks. Fail the read if
    // any header cannot be fully resolved, rather than returning a silently truncated block: a
    // dropped tx would shift the computed `tx_start_offset` of every surviving tx in its ledger.
    let event = state
        .db
        .view_eyre(|tx| {
            let mut resolve_err: Option<eyre::Report> = None;
            let event = BlockEvent::from_header_and_txs(
                &header,
                |ledger| match irys_database::block_ledger_tx_headers(tx, &header, ledger) {
                    Ok(txs) => txs,
                    Err(e) => {
                        resolve_err.get_or_insert(e);
                        Vec::new()
                    }
                },
                chunk_size,
            );
            match resolve_err {
                Some(e) => Err(e),
                None => Ok(event),
            }
        })
        .map_err(|e| ApiError::Internal { err: e.to_string() })?;
    Ok(Some(event))
}

#[cfg(test)]
mod tests {
    use super::parse_offset_span;

    #[test]
    fn offset_span_parses_inclusive_bounds() {
        assert_eq!(parse_offset_span("0-0"), Some((0, 0)));
        assert_eq!(parse_offset_span("3-11"), Some((3, 11)));
    }

    #[test]
    fn offset_span_rejects_malformed_or_inverted() {
        for bad in ["", "5", "-", "a-b", "1-2-3", "-4", "4-", "9-3"] {
            assert_eq!(parse_offset_span(bad), None, "{bad:?}");
        }
    }
}
