//! The `/internal/*` HTTP surface: the block-stream SSE endpoint and the canonical block reads.
//!
//! These routes serve the gateway block follower. The wire shapes live in
//! [`irys_types::block_stream`]. `stream` is registered before `{height}` so it is matched as a
//! literal segment rather than captured as a height.
//!
//! SECURITY: these endpoints carry no application-layer authentication and ride the same HTTP
//! listener as the public API. They expose internal block data and a long-lived SSE stream, so the
//! deployment MUST restrict `/internal/*` at the network layer (firewall / reverse proxy / bind
//! address) to the trusted gateway.

use crate::ApiState;
use crate::error::ApiError;
use actix_web::{HttpResponse, dev::HttpServiceFactory, web};
use irys_actors::block_tree_service::get_block_header;
use irys_database::db::IrysDatabaseExt as _;
use irys_types::block_stream::{BlockEvent, StreamFrame};
use irys_types::{DataLedger, DataTransactionHeader, IrysBlockHeader, app_state::DatabaseProvider};
use serde::Deserialize;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::ReceiverStream;

pub fn internal_routes() -> impl HttpServiceFactory {
    web::scope("internal")
        .route("/blocks/stream", web::get().to(blocks_stream))
        .route("/blocks/{height}", web::get().to(block_by_height))
        .route("/blocks", web::get().to(blocks_range))
}

#[derive(Debug, Deserialize)]
struct StreamQuery {
    #[serde(default)]
    from_seq: u64,
}

/// `GET /internal/blocks/stream?from_seq=` — the SSE stream. Replays the durable suffix from
/// `from_seq`, then tails live frames, each framed as `data: {json}\n\n`.
async fn blocks_stream(
    state: web::Data<ApiState>,
    query: web::Query<StreamQuery>,
) -> Result<HttpResponse, ApiError> {
    let (replay, live) = state
        .block_stream
        .subscribe(query.from_seq)
        .map_err(|e| ApiError::Internal { err: e.to_string() })?;

    let body = tokio_stream::iter(replay)
        .chain(ReceiverStream::new(live))
        .map(frame_to_sse);

    Ok(HttpResponse::Ok()
        .content_type("text/event-stream")
        .streaming(body))
}

fn frame_to_sse(frame: StreamFrame) -> Result<web::Bytes, actix_web::Error> {
    let json = serde_json::to_string(&frame).map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(web::Bytes::from(format!("data: {json}\n\n")))
}

/// `GET /internal/blocks/{height}` — the canonical block at `height` as a `BlockEvent`, or `404`.
async fn block_by_height(
    state: web::Data<ApiState>,
    path: web::Path<u64>,
) -> Result<web::Json<BlockEvent>, ApiError> {
    let height = path.into_inner();
    resolve_block_event(&state, height)?
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

/// `GET /internal/blocks?from_height=&to_height=` — the canonical blocks in `[from, to]`, ascending.
async fn blocks_range(
    state: web::Data<ApiState>,
    query: web::Query<RangeQuery>,
) -> Result<web::Json<Vec<BlockEvent>>, ApiError> {
    let span = query.to_height.saturating_sub(query.from_height);
    if span > MAX_BLOCK_RANGE {
        return Err(ApiError::InvalidBlockParameter {
            parameter: format!(
                "block range {}..={} spans {span} heights, exceeding the maximum {MAX_BLOCK_RANGE}",
                query.from_height, query.to_height
            ),
        });
    }
    let mut events = Vec::new();
    for height in query.from_height..=query.to_height {
        if let Some(event) = resolve_block_event(&state, height)? {
            events.push(event);
        }
    }
    Ok(web::Json(events))
}

/// Resolves the canonical block at `height` to a `BlockEvent`, or `None` if there is none.
///
/// In-tree blocks are built from the sealed block in hand (recent blocks are not yet persisted to
/// `IrysDataTxHeaders`); migrated blocks are rebuilt from the header plus per-tx headers resolved
/// from the DB via `tx_header_by_txid`.
fn resolve_block_event(state: &ApiState, height: u64) -> Result<Option<BlockEvent>, ApiError> {
    let chunk_size = state.config.consensus.chunk_size;

    let in_tree = state
        .block_tree
        .read()
        .get_canonical_chain()
        .0
        .iter()
        .find_map(|entry| (entry.height() == height).then(|| entry.block_hash()));
    let block_hash = match in_tree {
        Some(hash) => hash,
        None => match state.block_index.read().get_item(height) {
            Some(item) => item.block_hash,
            None => return Ok(None),
        },
    };

    if let Some(sealed) = state.block_tree.read().get_sealed_block(&block_hash) {
        return Ok(Some(BlockEvent::from_sealed(&sealed, chunk_size)));
    }

    let header = get_block_header(&state.block_tree, &state.db, block_hash, false)
        .map_err(|e| ApiError::Internal { err: e.to_string() })?;
    let Some(header) = header else {
        return Ok(None);
    };

    let db = &state.db;
    // Fail the read if any ledger's tx headers cannot be fully resolved, rather than returning a
    // silently truncated block. The closure stashes the first error; the event built alongside it
    // is discarded when one is present.
    let mut resolve_err: Option<ApiError> = None;
    let event = BlockEvent::from_header_and_txs(
        &header,
        |ledger| match resolve_ledger_txs(db, &header, ledger) {
            Ok(txs) => txs,
            Err(e) => {
                resolve_err.get_or_insert(e);
                Vec::new()
            }
        },
        chunk_size,
    );
    if let Some(e) = resolve_err {
        return Err(e);
    }
    Ok(Some(event))
}

/// Resolves the ordered transaction headers for one ledger of a migrated block from the DB.
///
/// Fails closed: a tx id with no header row (or a DB error) aborts the read rather than yielding a
/// truncated ledger. A dropped tx would also shift the computed `tx_start_offset` of every
/// surviving tx in the ledger, so a short block silently misrepresents canonical contents.
fn resolve_ledger_txs(
    db: &DatabaseProvider,
    header: &IrysBlockHeader,
    ledger: DataLedger,
) -> Result<Vec<DataTransactionHeader>, ApiError> {
    let ledger_id = ledger.get_id();
    let Some(data_ledger) = header
        .data_ledgers
        .iter()
        .find(|dl| dl.ledger_id == ledger_id)
    else {
        return Ok(Vec::new());
    };
    let mut txs = Vec::with_capacity(data_ledger.tx_ids.0.len());
    for tx_id in &data_ledger.tx_ids.0 {
        let header = db
            .view_eyre(|tx| irys_database::tx_header_by_txid(tx, tx_id))
            .map_err(|e| ApiError::Internal { err: e.to_string() })?
            .ok_or_else(|| ApiError::Internal {
                err: format!("canonical block missing tx header for {tx_id}"),
            })?;
        txs.push(header);
    }
    Ok(txs)
}
