# Mempool pending transaction list API

## Endpoint

```http
GET /v1/mempool/txs
GET /v1/mempool/txs?limit=100
GET /v1/mempool/txs?limit=100&after_id=<base58 H256>
```

Public, unauthenticated HTTP (same CORS model as `/v1/tip` and `/v1/mempool/status`).

| Param      | Default | Cap   | Description |
|------------|---------|-------|-------------|
| `limit`    | `100`   | `500` | Max entries **per list** (`data_txs` and `commitment_txs`) |
| `after_id` | —       | —     | Forward cursor: ids strictly after this base58 `H256`. Malformed → **400**. |

Related count-only endpoint (unchanged): `GET /v1/mempool/status`.

## Semantics

- **Pending** = in this node’s mempool and unconfirmed (`included_height` /
  `promoted_height` unset). Includes out-of-order `pending_pledges`.
- **Order**: ascending by raw `H256` bytes (not base58 string order).
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

### Truncation / paging

When more unconfirmed txs remain after `after_id` + `limit`:

- `truncated: true` — next page: `after_id=<last returned id for that list>`
- array counts = returned lengths; `total_*_tx_count` = full unconfirmed totals
- `truncated` is shared across both lists (true until both are fully paged)

`after_id` filters **both** lists by the same id order. Prefer a high enough
`limit` for simple pollers; use the cursor when you need the full set.

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

last=$(curl -sS "http://127.0.0.1:1984/v1/mempool/txs?limit=50" | jq -r '.data_txs[-1].id')
curl -sS "http://127.0.0.1:1984/v1/mempool/txs?limit=50&after_id=$last" | jq .

curl -sS "http://127.0.0.1:1984/v1/mempool/status" \
  | jq '{data_tx_count, commitment_tx_count, pending_chunks_count}'
```

## Out of scope (v1)

- WebSocket streaming
- Full tx body in the list response
- Historical / departed mempool entries
