# Mempool pending transaction list API

## Endpoint

```http
GET /v1/mempool/txs
GET /v1/mempool/txs?limit=100
```

Public, unauthenticated HTTP (same CORS model as `/v1/tip` and `/v1/mempool/status`).

| Param   | Default | Cap | Description                                      |
|---------|---------|-----|--------------------------------------------------|
| `limit` | `100`   | `500` | Max entries **per list** (`data_txs` and `commitment_txs` each) |

Related summary-only endpoint (unchanged count fields):

```http
GET /v1/mempool/status
```

## Semantics

- **Pending** = present in this node’s mempool **and** not yet confirmed
  (`included_height` / `promoted_height` unset). Confirmed txs may remain in
  internal mempool state for reorg handling; they are omitted from this list.
- **Order**: stable by transaction id (`H256` byte order).
- **Empty pool**: HTTP **200** with empty arrays and zero counts — never 404.
- **Ids**: base58-encoded `H256`, same encoding as block ledger `txIds` and
  `GET /v1/tx/{id}`.

## Response schema

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
    {
      "id": "<base58 H256>",
      "address": "<base58 address>"
    }
  ],
  "data_tx_count": 1,
  "commitment_tx_count": 1,
  "pending_chunks_count": 0,
  "truncated": false
}
```

### Truncation

When either full unconfirmed list exceeds `limit`:

- `truncated` is `true`
- `data_txs` / `commitment_txs` are truncated independently to `limit`
- `data_tx_count` / `commitment_tx_count` equal the **returned array lengths**
- `total_data_tx_count` / `total_commitment_tx_count` are set to the full
  unconfirmed totals

When not truncated, the `total_*` fields are omitted and counts match arrays
and the full pool.

`pending_chunks_count` is the same chunk-ingress metric as `/v1/mempool/status`
(not truncated).

### Field notes

| Field (data tx) | Meaning |
|-----------------|--------|
| `id`            | Tx id; matches `GET /v1/tx/{id}` and block ledger `txIds` |
| `byte_size`     | Header `data_size` |
| `chunks`        | `byte_size.div_ceil(chunk_size)` using consensus chunk size |
| `data_root`     | Merkle root of payload chunks |
| `ledger_id`     | Destination ledger (pre-inclusion) |

Full headers are **not** embedded; fetch `GET /v1/tx/{id}` for the full body.

## Example

```bash
# Local / devnet node (default port from node config)
curl -sS "http://127.0.0.1:1984/v1/mempool/txs" | jq .

# Cap list size for large pools
curl -sS "http://127.0.0.1:1984/v1/mempool/txs?limit=50" | jq .

# Aggregate counts still come from status
curl -sS "http://127.0.0.1:1984/v1/mempool/status" | jq '{data_tx_count, commitment_tx_count, pending_chunks_count}'
```

## Out of scope (v1)

- WebSocket streaming
- Full tx body in the list response
- Historical / departed mempool entries
