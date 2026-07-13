# Mempool pending transaction list API

## Endpoint

```http
GET /v1/mempool/txs
GET /v1/mempool/txs?limit=100
GET /v1/mempool/txs?limit=100&cursor=<next_cursor>
```

Public, unauthenticated HTTP (same CORS model as `/v1/tip` and `/v1/mempool/status`).

| Param    | Default | Cap   | Description |
|----------|---------|-------|-------------|
| `limit`  | `100`   | `500` | Max entries **per list** (`data_txs` and `commitment_txs`) |
| `cursor` | —       | —     | Opaque dual-list page token from a prior `next_cursor`. Malformed → **400**. |

Related count-only endpoint (unchanged): `GET /v1/mempool/status`.

## Semantics

- **Pending** = in this node’s mempool and unconfirmed (`included_height` /
  `promoted_height` unset). Includes out-of-order `pending_pledges`.
- **Order**: each list ascending by raw `H256` bytes (not base58 string order).
- **Empty pool**: **200** with empty arrays / zero counts (never 404).
- **Ids**: base58 `H256`, same as block ledger `txIds` and `GET /v1/tx/{id}`.

## Response

```json
{
  "data_txs": [
    {
      "id": "<base58 H256>",
      "byte_size": 1234,
      "chunks": 5,
      "data_root": "<base58 H256>",
      "ledger_id": 1
    }
  ],
  "commitment_txs": [
    { "id": "<base58 H256>", "address": "<base58 address>" }
  ],
  "data_tx_count": 1,
  "commitment_tx_count": 1,
  "pending_chunks_count": 0,
  "truncated": false
}
```

When `truncated` is true, also:

```json
{
  "truncated": true,
  "next_cursor": "v1.<data_id|_>.<commitment_id|_>",
  "total_data_tx_count": 250,
  "total_commitment_tx_count": 12
}
```

### Cursor semantics

- Data and commitment lists page **independently**. The cursor stores a separate
  forward bound for each list (`after_data_id`, `after_commitment_id`).
- Wire format: `v1.<data_base58|_>.<commitment_base58|_>` (`_` = no bound yet).
- On each truncated page, `next_cursor` advances each side to the last id
  returned in that list (keeps the prior bound if that list’s page was empty).
- Client loop: request without `cursor` → while `truncated`, call again with
  `cursor=<next_cursor>`. No duplicates or omissions across pages for either
  list (IDs are strictly greater than that list’s bound).
- Prefer a large enough `limit` for simple pollers; use `cursor` for full dumps.

`pending_chunks_count` matches `/v1/mempool/status` (not truncated).

### Data-tx fields

| Field | Meaning |
|-------|---------|
| `id` | Tx id (`GET /v1/tx/{id}`, block ledger match) |
| `byte_size` | Header `data_size` |
| `chunks` | `byte_size.div_ceil(chunk_size)` |
| `data_root` | Payload merkle root |
| `ledger_id` | Destination ledger |

Full headers via `GET /v1/tx/{id}`.

## Example

```bash
curl -sS "http://127.0.0.1:1984/v1/mempool/txs" | jq .
curl -sS "http://127.0.0.1:1984/v1/mempool/txs?limit=50" | jq .

# Page through a large pool
cursor=$(curl -sS "http://127.0.0.1:1984/v1/mempool/txs?limit=50" | jq -r '.next_cursor // empty')
while [ -n "$cursor" ]; do
  resp=$(curl -sS "http://127.0.0.1:1984/v1/mempool/txs?limit=50&cursor=$cursor")
  echo "$resp" | jq '{data: .data_tx_count, commit: .commitment_tx_count, truncated}'
  cursor=$(echo "$resp" | jq -r '.next_cursor // empty')
done

curl -sS "http://127.0.0.1:1984/v1/mempool/status" \
  | jq '{data_tx_count, commitment_tx_count, pending_chunks_count}'
```

## Out of scope (v1)

- WebSocket streaming
- Full tx body in the list response
- Historical / departed mempool entries
