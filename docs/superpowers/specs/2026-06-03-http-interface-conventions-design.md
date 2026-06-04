# HTTP Interface Conventions — Design

**Date:** 2026-06-03
**Status:** Step 1 in implementation; steps 2–4 documented as roadmap.

## Goal

Refine the node's HTTP interface (`irys-api-server`, actix-web) so that:

1. **Conventions are enforced** across all routes — chiefly a single error format that no handler (or framework path) can deviate from.
2. The interface is described by a **machine-readable schema** (OpenAPI 3.x).
3. That schema can be **exported/imported across applications** — internal Rust clients (`irys-api-client`, TUI), cross-language SDKs, human docs, and gateways.

## Decisions (settled)

- **Schema artifact:** OpenAPI 3.x, **code-first** via `utoipa`. Rust stays the single source of truth.
- **Error format:** **RFC 9457 — Problem Details for HTTP APIs** (`application/problem+json`). The IETF standard; natively understood by client/server tooling across languages, which is what makes it importable across applications.
- **Correlation:** W3C Trace Context id, carried as a `traceId` member (matching the ASP.NET Core convention) and as a `traceparent` response header. Sourced from the existing OTEL span.
- **`instance`:** omitted (the request path is client-derivable; `traceId` is the useful correlation handle).

## The error contract (`Problem`)

One shared type, defined once in `irys-types`, imported by the server and every client. Serialized as `application/problem+json`.

```rust
/// RFC 9457 Problem Details. The one and only error shape on every route.
pub struct Problem {
    pub r#type: String,   // stable machine identifier (URI), serde(rename="type")
    pub title: String,    // short, human, stable per error class
    pub status: u16,      // HTTP status, mirrored into the body
    pub detail: Option<String>, // instance-specific human message
    pub code: String,     // short ergonomic code clients switch on, e.g. "BLOCK_NOT_FOUND"
    pub trace_id: String, // W3C trace id; serde(rename="traceId"); present on every error
    pub errors: Vec<ProblemItem>, // multi-field validation only; omitted when empty
    pub error: Option<String>,    // DEPRECATED migration mirror of `detail`; removed next release
}

pub struct ProblemItem {
    pub code: String,
    pub detail: String,
    pub pointer: Option<String>, // RFC 6901 JSON Pointer into the request body, e.g. "/signature"
}
```

### Wire examples

Single whole-request error (`GET /v1/block/0xabc123` → 404):

```json
{ "type": "https://docs.irys.xyz/api/errors/block-not-found", "title": "Block not found",
  "status": 404, "detail": "Block 0xabc123 was not found in the block tree or database",
  "code": "BLOCK_NOT_FOUND", "traceId": "0af7651916cd43dd8448eb211c80319c" }
```

Multi-field validation (`POST /v1/tx` → 400):

```json
{ "type": "https://docs.irys.xyz/api/errors/validation", "title": "Transaction validation failed",
  "status": 400, "code": "VALIDATION_FAILED", "traceId": "7c33d9f1aa20...",
  "errors": [
    { "code": "INVALID_SIGNATURE", "detail": "signature does not match signer", "pointer": "/signature" },
    { "code": "UNFUNDED", "detail": "balance 30 < required 50", "pointer": "/value" } ] }
```

5xx keeps `detail` generic (no internal leakage), `traceId` is the support handle.

## Enforcement — the five response origins

A convention only holds if every place a response is born funnels into `Problem`. This mirrors ASP.NET Core's model: a rich error type for known errors plus a middleware that problem-ifies everything else.

| Origin | Fix |
|---|---|
| Handler `Err(ApiError)` | `ApiError` is the rich, typed source. Every variant maps to `(status, type, title, code)`; `error_response()` builds the `Problem`. |
| `web::Json` body parse failure | `JsonConfig::error_handler` → `ApiError` (replaces today's plain-text bug) |
| `web::Path`/`web::Query` parse failure | `PathConfig`/`QueryConfig::error_handler` → `ApiError` |
| Unmatched route / 405 / 413 | `default_service` + payload-limit handler → `ApiError` |
| Anything else (panic, careless future handler) | **Response-normalizing middleware** backstop |

**Response-normalizing middleware:** inspects every outgoing response — if status ≥ 400 and `Content-Type` is not `application/problem+json`, replaces the body with a `Problem`. Also stamps the `traceparent` header on **all** responses (success included). One place, not 20.

**CI backstop:** a contract test walks every registered route, forces an error, and asserts `application/problem+json` + a schema-valid `Problem`. Drift becomes a test failure, not a review miss.

## OpenAPI generation & serving (step 2)

- `#[derive(ToSchema)]` on request/response types; `#[utoipa::path(...)]` on each handler. A shared helper supplies the standard `Problem` error responses so handlers don't each re-declare them.
- `utoipa-actix-web` binds the handler and collects its path together — registering a route without documenting it is not possible.
- Serve `GET /v1/openapi.json`, `GET /v1/openapi.yaml`, and Swagger UI at `/v1/docs` (Redoc is the read-only alternative).
- `cargo xtask openapi` writes the spec to a committed `crates/api-server/openapi.json`; **CI regenerates and diffs** → stale spec fails the build. This committed file is the single exportable artifact.

## Export / import across applications (step 2/4)

- **Internal Rust** (`irys-api-client`, TUI): keep hand-written (types already live in `irys-types`; generating would clash). Add typed `Problem` parsing + the contract test for parity.
- **Cross-language SDKs:** third parties run `openapi-generator` / `openapi-typescript` against `openapi.json`. Nothing to maintain beyond the spec.
- **Gateways / services:** consume `openapi.json` directly for validation; `application/problem+json` gives them error modeling for free.

## Rollout

1. **Foundation (this PR):** `Problem` in `irys-types`; `ApiError` → `Problem` with type/title/code per variant + `traceId` + deprecated `error` mirror + `errors[]`; extractor/default-service handlers; normalizing middleware + `traceparent`; contract test. Delivers the error-convention goal across all routes via the middleware without annotating every handler yet.
2. **utoipa scaffolding:** deps (`utoipa`, `utoipa-actix-web`, `utoipa-swagger-ui`); `ToSchema`/`#[utoipa::path]` on a 3-route pilot (`tx`, `block`, `balance`); serve `/v1/openapi.json` + `/v1/docs`; `xtask openapi` + CI drift check.
3. **Annotate remaining routes:** mechanical, parallelizable; add a route-coverage test (registered routes == documented paths) so the spec is provably complete.
4. **Collapse the TUI client** onto `irys-api-client` — one Rust client, removing the second drift source.

## Notes

- `irys-api-client` reads error bodies as raw text (does not parse the `error` field), so the shape change is non-breaking for it; the deprecated `error` mirror covers unknown external consumers for one release.
- `r#type` field uses `#[serde(rename = "type")]`; `trace_id` uses `#[serde(rename = "traceId")]`.
- Trace id is sourced in `irys-api-server` via `tracing-opentelemetry` (already a workspace dep); the `irys-types::Problem` type stays dependency-light (serde only) and is populated by the server.
