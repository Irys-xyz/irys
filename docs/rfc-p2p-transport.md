# RFC: P2P Transport Layer Redesign — HTTP to QUIC

## 1. Current Architecture

### Overview

The `irys-p2p` crate implements all peer-to-peer communication over **HTTP POST/GET** using actix-web (server) and reqwest (client) with JSON serialization. This is fundamentally unlike any production blockchain P2P system.

```
┌─────────────────────────────────────────────────────────────────────┐
│                         P2PService                                  │
│                ˚    (gossip_service.rs)                               │
│  Receives: UnboundedReceiver<GossipBroadcastMessageV2>              │
│  Owns: GossipCache, GossipClient, ChainSyncState                   │
│  Spawns: broadcast_task, server_watcher_task                        │
└────────────────────────────┬────────────────────────────────────────┘
                             │
           ┌─────────────────┼──────────────────┐
           ▼                 ▼                   ▼
┌───────────────────┐ ┌──────────────┐ ┌──────────────────────┐
│   GossipServer    │ │ Broadcast    │ │   PeerNetworkService │
│ (actix-web HTTP)  │ │ Task         │ │ (peer_network_       │
│  server.rs        │ │              │ │  service.rs)         │
│  1738 lines       │ │ Receives     │ │                      │
│                   │ │ validated    │ │ Manages:             │
│ 17 HTTP routes    │ │ data from    │ │ - Peer handshakes    │
│ /gossip/v{1,2}/* │ │ mempool/     │ │ - Health checks      │
│                   │ │ block prod   │ │ - Peer scoring       │
│ Dispatches to     │ │ and sends    │ │ - Blocklist/backoff  │
│ GossipDataHandler │ │ to peers     │ │ - DB persistence     │
└────────┬──────────┘ └──────────────┘ └──────────────────────┘
         │
         ▼
┌───────────────────────┐
│  GossipDataHandler    │
│ (gossip_data_handler  │
│  .rs, 1402 lines)     │
│                       │
│ Validates & routes:   │     ┌─────────────────────────────────┐
│ • Chunks ──────────────────→│ ChunkIngressService             │
│ • Transactions ────────────→│ MempoolService                  │
│ • Commitment Txs ──────────→│ MempoolService                  │
│ • Ingress Proofs ──────────→│ ChunkIngressService             │
│ • Block Headers ───────────→│ BlockPool                       │
│ • Block Bodies ────────────→│ BlockPool                       │
│ • Execution Payloads ──────→│ ExecutionPayloadCache           │
└───────────────────────┘     └─────────────────────────────────┘
         │
         ▼
┌───────────────────────┐         ┌──────────────────────┐
│     BlockPool         │────────→│  ChainSyncService    │
│ (block_pool.rs        │ Sends:  │ (chain_sync.rs       │
│  1972 lines)          │ Sync    │  2051 lines)         │
│                       │ msgs    │                      │
│ - Orphan management   │         │ - Initial sync       │
│ - Block validation    │         │ - Periodic checks    │
│ - LRU cache (250)     │         │ - Block fetching     │
│ - Parent requests     │         │ - VDF control        │
└───────────────────────┘         └──────────────────────┘
```

### Service Wiring at Startup

Initialization happens in `IrysNode::start()` (`crates/chain/src/chain.rs`):

```
1. ServiceSenders::new()
   Creates all inter-service channels including:
   - gossip_broadcast: UnboundedSender<GossipBroadcastMessageV2>
   - peer_network: PeerNetworkSender
   - chain_sync: (tx, rx) for SyncChainServiceMessage

2. P2PService::new(miner_address, peer_id, gossip_broadcast_rx)
   Takes ownership of the gossip broadcast receiver

3. spawn_peer_network_service(config, db, peer_network_rx, ...)
   Spawns PeerNetworkService as a tokio task
   Returns PeerList (shared Arc)

4. P2PService::run(mempool, block_discovery, peer_list, chain_sync_tx, ...)
   Creates:
   - BlockPool (holds chain_sync_tx sender)
   - GossipDataHandler (holds mempool, chunk_ingress, block_pool refs)
   - GossipServer (actix-web, holds GossipDataHandler)
   - Broadcast task (listens on gossip_broadcast_rx)

5. ChainSyncService::spawn_service(block_pool, gossip_data_handler, chain_sync_rx)
   Spawns chain sync as a tokio task
   Shares BlockPool and GossipDataHandler with P2PService
```

### Message Flows

**Inbound (peer sends us data):**
```
Peer HTTP POST /gossip/v2/block
  → GossipServer.handle_block_header_v2()
    → check_peer_v2() — verify peer_id, IP, miner_address
    → GossipDataHandler.handle_block_header()
      → GossipCache.seen_block_from_any_peer() — dedup check
      → validate block signature
      → pull block body from peer if needed (GossipClient)
      → BlockPool.process_block()
        → BlockDiscoveryService.handle_block() — full validation
        → if orphan: store, request parent from network
        → SyncChainServiceMessage::BlockProcessedByThePool
    → GossipCache.record_seen(peer_id, block_hash)
    → PeerList.increase_score() / decrease_score()
  → HTTP 200 GossipResponse::Accepted(())
```

**Outbound (we broadcast data to peers):**
```
MempoolService / BlockProducer validates data
  → ServiceSenders.gossip_broadcast.send(GossipBroadcastMessageV2)
    → P2PService broadcast_task receives message
      → P2PService.broadcast_data()
        → peer_list.all_peers_sorted_by_score()
        → cache.peers_that_have_seen() — skip peers that already saw it
        → for each peer (in batches of broadcast_batch_size=50):
            → GossipClient.send_preserialized_detached() — tokio::spawn per peer
              → HTTP POST /gossip/v2/<route> with JSON body
              → handle_score() — update peer reputation
              → cache.record_seen(peer_id, key)
```

**Chain sync (catching up to network):**
```
Periodic timer (every 30s) or BlockPool detects missing parent
  → SyncChainServiceMessage → ChainSyncService
    → GossipClient.get_info() to trusted peers — compare heights
    → if behind by >2*migration_depth:
        set is_syncing=true, disable VDF mining
        for each missing height:
          → GossipClient.pull_block_header_from_network()
          → GossipDataHandler.handle_block_header()
          → BlockPool.process_block()
        set is_syncing=false, re-enable VDF mining
```

**Peer management:**
```
PeerNetworkService main loop:
  → Every 10s: health check inactive peers (GET /health)
  → On PeerNetworkServiceMessage::Handshake:
      → GossipClient: POST /gossip/v2/handshake
      → On success: add peer to PeerList, persist to DB
      → On failure: exponential backoff, blocklist after max retries
  → Every 5s: flush PeerList to database
```

### Key Components

| Component | File | Lines | Purpose |
|-----------|------|-------|---------|
| P2PService | `gossip_service.rs` | 460 | Orchestrates broadcast, owns cache + client |
| GossipServer | `server.rs` | 1738 | Actix-web HTTP server, 17 routes, peer verification |
| GossipClient | `gossip_client.rs` | 3157 | Reqwest HTTP client, circuit breakers, scoring |
| GossipDataHandler | `gossip_data_handler.rs` | 1402 | Validates inbound data, routes to downstream services |
| BlockPool | `block_pool.rs` | 1972 | Orphan management, block caching, sync coordination |
| ChainSyncService | `chain_sync.rs` | 2051 | Network sync, block fetching, VDF control |
| PeerNetworkService | `peer_network_service.rs` | 1709 | Handshakes, health checks, peer persistence |
| GossipCache | `cache.rs` | 160 | TTL-based dedup (5 min), tracks per-peer visibility |
| DataRequestTracker | `rate_limiting.rs` | 381 | Per-peer rate limiting, score capping |

### Shared State

| State | Type | Shared Between |
|-------|------|----------------|
| `GossipCache` | `Arc<GossipCache>` | P2PService, GossipDataHandler, broadcast task |
| `PeerList` | `Arc<PeerListServiceInner>` | All P2P services, ChainSyncService |
| `ChainSyncState` | `ChainSyncState` (Arc + AtomicBool) | P2PService, BlockPool, ChainSyncService, VDF thread |
| `BlockPool` | `Arc<BlockPool>` | GossipDataHandler, ChainSyncService |
| `GossipClient` | `GossipClient` (Clone) | P2PService, GossipDataHandler, PeerNetworkService |
| `ExecutionPayloadCache` | `ExecutionPayloadCache` | GossipDataHandler, BlockPool |

### What's Wrong with HTTP

| Problem | Where | Impact |
|---------|-------|--------|
| **Keep-alive disabled** | `server.rs:1663` | Every message opens a new TCP connection |
| **JSON + base64** | All messages use `serde_json`, `Base64` type in `crates/types/src/serialization.rs:561` | 256KB chunks become ~349KB on wire (~33% overhead) |
| **No multiplexing** | HTTP/1.1 request-response | Can't pipeline transfers or interleave data types |
| **No encryption** | Plain HTTP | Messages readable on network |
| **O(N) broadcast** | `gossip_service.rs:286` | Sends to every peer. TODO at line 283 acknowledges this |
| **No anti-entropy** | Chain sync only checks every 30s, threshold of `>2*migration_depth` | Small gaps (1-5 blocks) go undetected |
| **Throttle bug** | `gossip_service.rs:341` | Sleep is after the entire loop, not between batches |
| **V1/V2 duplication** | `server.rs` has parallel handlers for every route | ~800 lines of near-duplicate code |

---

## 2. Why QUIC (Not TCP, Not libp2p)

### Why not raw TCP?

QUIC provides everything TCP does plus encryption, multiplexing, and congestion control natively. Building equivalent features on raw TCP would be reinventing QUIC:

| Feature | Raw TCP | QUIC |
|---------|---------|------|
| Encryption | Must add TLS or custom (ECIES) | Built-in TLS 1.3, mandatory |
| Multiplexing | Must build (e.g., yamux, length-prefixed framing) | Native streams, no head-of-line blocking |
| Congestion control | Kernel TCP congestion control (less tunable) | Pluggable, application-level congestion control |
| Connection migration | IP change = broken connection | Connection IDs survive NAT rebinding |
| 0-RTT resumption | Not available | Supported for repeat connections |

The only advantage of raw TCP is wider firewall compatibility (some corporate/cloud firewalls block UDP). For Irys nodes running in datacenter/cloud environments, this is rarely an issue.

### Why not libp2p?

libp2p is a comprehensive P2P framework used by Ethereum CL (Lighthouse), Substrate, and Filecoin. It provides gossip (GossipSub), peer discovery (Kademlia), NAT traversal, and encryption out of the box. However:

**1. Framework lock-in and complexity.**

libp2p's `Swarm` + `NetworkBehaviour` + `ConnectionHandler` is a large, opinionated API surface. Common complaints from blockchain developers:
- Steep learning curve
- Debugging is difficult (many abstraction layers between code and network)
- Heavy dependency tree (significantly increases compile times)
- Deviating from libp2p's patterns is painful
- When things go wrong, diagnosing root causes through the abstraction layers is hard

**2. We already have the application-layer logic.**

Irys has working implementations of: fan-out/broadcast, deduplication (GossipCache), peer scoring, circuit breakers, rate limiting, connection management, health checking, and chain sync. These work — they just run over HTTP.

What we need is a **transport replacement**, not a framework replacement. QUIC lets us keep our application logic and swap the transport.

### Why QUIC (quinn)?

| Factor | Assessment |
|--------|-----------|
| **Lightweight** | quinn is just a transport. Small dependency, fast compile |
| **Clean API** | Open connection → create stream → send/receive bytes. Straightforward |
| **Native multiplexing** | Multiple independent bidirectional streams per connection. No HOL blocking |
| **Built-in encryption** | TLS 1.3, mandatory. Authenticated peer identities |
| **Full control** | Framing, gossip logic, peer management — our code, easy to debug |
| **Incremental migration** | Adapt existing gossip logic to QUIC streams. Application layer preserved |
| **Optimal for chunks** | No pubsub overhead. Each 256KB chunk sent exactly once per target peer |
| **Production-proven** | Solana migrated TPU from UDP to QUIC. Quinn has 86M+ crates.io downloads |
| **Maturity** | quinn is production-ready, actively maintained |

**What we give up vs libp2p:**
- Must build peer discovery ourselves (can use Discv5 crate or adapt existing handshake-based exchange)
- NAT traversal is less comprehensive (QUIC hole punching works but no relay fallback like libp2p)
- No off-the-shelf GossipSub (but we don't want it for 256KB chunks anyway)

**What we gain:**
- Full control over the protocol — easy to optimize for our specific workload
- Simpler debugging — fewer abstraction layers
- Lighter dependency footprint
- Faster time to ship (lower learning curve)
- No duplicate message amplification for chunks

---

## 3. Migration Plan — Incremental Atomic Changes

The migration is designed so that HTTP and QUIC coexist during a transition period. Each step is an atomic, independently testable change. If something breaks, `git bisect` will identify the exact commit.

### Phase 1: QUIC Transport Foundation

#### Step 1.1: Add quinn Dependency and Transport Module

**What:** Add `quinn` and `rustls` dependencies. Create a new `transport` module in `irys-p2p` with connection management primitives.

**Contents of new module:**
- `QuicTransport` struct — manages a `quinn::Endpoint` (both client and server on the same UDP socket)
- TLS configuration using `rustls` with self-signed certificates (peer identity derived from key pair)
- Connection establishment and acceptance
- Connection pool (map of `IrysPeerId → quinn::Connection`)

**Files:**
- `crates/p2p/Cargo.toml` — add `quinn`, `rustls`, `rcgen` (for self-signed certs)
- `crates/p2p/src/transport/mod.rs` — new module
- `crates/p2p/src/transport/quic.rs` — QuicTransport implementation
- `crates/p2p/src/transport/tls.rs` — TLS config and certificate generation

**Test:** Unit test: two QuicTransport instances connect to each other, send bytes back and forth.

#### Step 1.2: Binary Message Framing

**What:** Define a simple binary framing protocol for messages over QUIC streams.

**Frame format:**
```
┌──────────────┬──────────────┬────────────────┬──────────────────┐
│ message_type │ payload_len  │  peer_id       │  payload         │
│ (1 byte)     │ (4 bytes LE) │  (20 bytes)    │  (payload_len)   │
└──────────────┴──────────────┴────────────────┴──────────────────┘
```

**Message types:**
```
0x01 = BlockHeader
0x02 = BlockBody
0x03 = Transaction
0x04 = CommitmentTransaction
0x05 = Chunk
0x06 = ExecutionPayload
0x07 = IngressProof
0x08 = DataRequest (pull)
0x09 = DataResponse (pull response)
0x0A = Handshake
0x0B = HandshakeResponse
0x0C = Health
0x0D = HealthResponse
0x0E = Info
0x0F = InfoResponse
0x10 = PeerListRequest
0x11 = PeerListResponse
0x12 = BlockIndexRequest
0x13 = BlockIndexResponse
```

**Serialization:** Use `postcard` (serde-compatible, compact binary format, handles `Vec<u8>` natively without base64). A 256KB chunk is ~262KB in postcard vs ~349KB in JSON.

**Files:**
- `crates/p2p/Cargo.toml` — add `postcard` with `use-std` feature
- `crates/p2p/src/transport/framing.rs` — frame read/write, message type enum
- `crates/p2p/src/transport/codec.rs` — postcard serialization/deserialization

**Test:** Round-trip test: serialize each message type, deserialize, verify equality.

#### Step 1.3: QUIC Connection Manager

**What:** Build a connection manager that maintains persistent QUIC connections to peers.

**Responsibilities:**
- Accept inbound connections on the QUIC endpoint
- Establish outbound connections to peers (with retry + backoff)
- Maintain a connection pool (`DashMap<IrysPeerId, quinn::Connection>`)
- Handle connection drops and reconnection
- Integrate with existing `PeerList` for peer address lookup
- Map TLS certificate identity to `IrysPeerId`

**Files:**
- `crates/p2p/src/transport/connection_manager.rs`

**Test:** Integration test: three connection managers form a mesh, verify all pairs connected.

### Phase 2: Dual Transport — QUIC Alongside HTTP

This is the critical coexistence phase. Both transports run simultaneously. Nodes prefer QUIC when both peers support it, fall back to HTTP otherwise.

#### Step 2.1: Protocol Version Negotiation

**What:** Extend the handshake to advertise QUIC support. Add a `TransportCapability` field.

**Changes:**
- Add `quic_address: Option<SocketAddr>` to `HandshakeRequestV2` / `HandshakeResponseV2`
- Add `transport_capabilities: Vec<TransportCapability>` (enum: `Http`, `Quic`)
- Store transport capability in `PeerListItem`

**Files:**
- `crates/types/src/peer_list.rs` — add `quic_address` to `PeerAddress`, add `TransportCapability`
- `crates/p2p/src/peer_network_service.rs` — populate QUIC address in handshake
- `crates/p2p/src/gossip_client.rs` — read QUIC capability from peer info

**Test:** Handshake between a QUIC-capable and HTTP-only node succeeds, capabilities stored correctly.

#### Step 2.2: QUIC Send Path (Outbound)

**What:** Add a QUIC send path alongside the existing HTTP send path in `GossipClient`.

**How the selection works:**
```rust
// In GossipClient, when sending to a peer:
if peer.supports_quic() && self.quic_transport.is_some() {
    self.send_via_quic(peer, message).await
} else {
    self.send_via_http(peer, message).await  // existing path
}
```

Each QUIC send:
1. Get or establish connection to peer from connection pool
2. Open a new unidirectional stream (for push) or bidirectional stream (for request-response)
3. Write framed message (Step 1.2 format)
4. For request-response: read framed response from same stream

**Files:**
- `crates/p2p/src/gossip_client.rs` — add `QuicTransport` field, add `send_via_quic()` methods alongside existing `send_data_internal()` / `send_preserialized()`
- `crates/p2p/src/gossip_service.rs` — pass QuicTransport to GossipClient

**Test:** Integration test: node A sends a block header to node B via QUIC, node B processes it.

#### Step 2.3: QUIC Receive Path (Inbound)

**What:** Add a QUIC listener alongside the actix-web HTTP server. Incoming QUIC streams are dispatched to the same `GossipDataHandler`.

**Architecture:**
```
quinn::Endpoint.accept() loop
  → for each new connection:
      spawn connection handler task
        → for each new stream on the connection:
            read framed message
            match message_type:
              BlockHeader → gossip_data_handler.handle_block_header()
              Transaction → gossip_data_handler.handle_transaction()
              Chunk → gossip_data_handler.handle_chunk()
              ... (same handlers as HTTP)
            write response frame
```

The key insight: `GossipDataHandler` is transport-agnostic. It validates and routes data regardless of how it arrived. Both HTTP handlers (`server.rs`) and the new QUIC listener call the same `GossipDataHandler` methods.

**Files:**
- `crates/p2p/src/transport/listener.rs` — QUIC accept loop, stream dispatch
- `crates/p2p/src/gossip_service.rs` — start QUIC listener alongside HTTP server in `P2PService::run()`

**Test:** Integration test: two nodes communicating entirely over QUIC — broadcast a block, verify receipt.

#### Step 2.4: QUIC-Based Peer Discovery and Health

**What:** Port handshake, health checks, and info queries to QUIC request-response streams.

The handshake is a natural request-response: open a bidirectional stream, send `HandshakeRequest`, receive `HandshakeResponse`. Same for `/health`, `/info`, `/peer-list`.

**Files:**
- `crates/p2p/src/peer_network_service.rs` — add QUIC handshake path
- `crates/p2p/src/gossip_client.rs` — add QUIC health/info/peer-list methods

**Test:** Node discovers peers and completes handshake over QUIC.

### Phase 3: Optimize and Harden

#### Step 3.1: Fan-Out Broadcast (Replace O(N))

**What:** Replace the broadcast-to-all-peers loop with data-type-aware fan-out.

| Strategy | Data Types | Behavior |
|----------|-----------|----------|
| Eager | BlockHeader, BlockBody, ExecutionPayload | All peers (consensus-critical, low frequency) |
| Standard | Transaction, CommitmentTx, IngressProof | `sqrt(N)` peers: top 3 by score + random sample |
| Relaxed | Chunk | `sqrt(N) * 0.5` peers (high volume, pull-recoverable) |

**Files:**
- `crates/p2p/src/gossip_service.rs` — add `BroadcastStrategy` enum, `select_broadcast_peers()`, rewrite `broadcast_data()`
- `crates/types/src/config/node.rs` — add `min_broadcast_peers`, `fanout_multiplier_percent`, `top_peers_count` to `P2PGossipConfig`

**Test:** Unit tests for `compute_fanout()` and `select_top_plus_random()`. Integration test: verify blocks reach all peers, chunks reach subset.

#### Step 3.2: Bounded Broadcast Concurrency

**What:** Add a `tokio::sync::Semaphore` to limit concurrent outbound broadcast tasks (currently unbounded `tokio::spawn` per peer).

**Files:**
- `crates/p2p/src/gossip_service.rs` — add `broadcast_semaphore` to `P2PService`
- `crates/p2p/src/gossip_client.rs` — add semaphore-aware send methods
- `crates/types/src/config/node.rs` — add `max_concurrent_broadcast_tasks: usize` (default 100)

**Test:** Verify broadcast completes with semaphore limit lower than peer count.

#### Step 3.3: Lightweight Block Tip Catch-Up

**What:** Add fast catch-up for small gaps (1-5 blocks) without triggering full sync.

Currently the periodic sync check (`chain_sync.rs:614`) only triggers full sync when `>2*migration_depth` blocks behind, which disables VDF mining. Small gaps go undetected for 30+ seconds.

**Changes:**
- Add `SyncChainServiceMessage::LightweightCatchUp { missing_block_hashes }` — pulls and processes blocks without setting `is_syncing`
- Extend health check to compare tip height/hash with each healthy peer
- If peer is 1-5 blocks ahead, trigger `LightweightCatchUp`

**Files:**
- `crates/p2p/src/chain_sync.rs` — add `LightweightCatchUp` handler
- `crates/p2p/src/peer_network_service.rs` — extend health check
- `crates/types/src/config/node.rs` — add `lightweight_catchup_max_blocks: usize` (default 5)

**Test:** Integration test: produce blocks on node A only, verify node B catches up within one health check cycle.

#### Step 3.4: Stream Prioritization

**What:** Use QUIC's native stream prioritization. Block headers and execution payloads get higher priority than chunks.

Quinn supports setting priority on streams. Higher priority streams get bandwidth preference during congestion.

**Files:**
- `crates/p2p/src/transport/quic.rs` — set stream priority based on message type
- `crates/p2p/src/gossip_client.rs` — use priority when creating streams

**Test:** Under simulated congestion, verify blocks arrive before chunks.

### Phase 4: Remove HTTP Transport

**When:** After all nodes in the network have upgraded to QUIC support (at least one release cycle of dual-transport operation).

#### Step 4.1: Deprecate HTTP Gossip Routes

**What:** Log warnings when HTTP gossip routes are used. Add config flag `enable_http_gossip: bool` (default true initially, then false).

#### Step 4.2: Remove HTTP Gossip Server

**What:** Remove actix-web server and all 17 gossip route handlers. Remove reqwest client for gossip. Remove V1/V2 protocol versioning.

**Impact:** Removes ~5000+ lines of HTTP-specific code from `server.rs` and `gossip_client.rs`.

#### Step 4.3: Remove JSON Serialization for P2P

**What:** Remove `serde_json` dependency for P2P messages. All P2P communication uses postcard binary encoding over QUIC.

---

## 4. Summary of Changes Per Phase

| Phase | Steps | Key Deliverable | HTTP Impact | Can Roll Back? |
|-------|-------|----------------|-------------|----------------|
| **1** | 1.1-1.3 | QUIC transport module | None (new code only) | Yes (remove module) |
| **2** | 2.1-2.4 | Dual transport | HTTP still works, QUIC added | Yes (disable QUIC) |
| **3** | 3.1-3.4 | Optimizations | Both benefit | Yes (revert individual) |
| **4** | 4.1-4.3 | HTTP removal | Removed | No (requires network coordination) |

### Network Upgrade Sequence

```
Release N:     Phase 1+2 (QUIC added, dual transport, HTTP still default)
Release N+1:   Phase 3 (optimizations, QUIC preferred)
Release N+2:   Phase 4 (HTTP deprecated, QUIC required)
Release N+3:   HTTP code removed
```

During releases N+1 through N+3, nodes communicate as follows:
- **Both QUIC-capable:** Use QUIC
- **One HTTP-only:** Use HTTP (fallback)
- **Both HTTP-only:** Use HTTP

The selection is per-peer based on the `transport_capabilities` exchanged during handshake (Step 2.1).
