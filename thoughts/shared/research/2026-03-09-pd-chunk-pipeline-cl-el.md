---
date: 2026-03-09T12:18:51+0000
researcher: rob
git_commit: 34a9989e216d042cad9db472ba695078ecf203e1
branch: rob/reth-pd-mempool-2
repository: irys
topic: "PD Chunk Pipeline: CL-EL Communication, Block Building, Mempool, and Validation"
tags: [research, codebase, pd-chunks, reth, mempool, block-building, validation, cl-el-bridge]
status: complete
last_updated: 2026-03-09
last_updated_by: rob
---

# Research: PD Chunk Pipeline — CL/EL Architecture

**Date**: 2026-03-09T12:18:51+0000
**Researcher**: rob
**Git Commit**: 34a9989e
**Branch**: rob/reth-pd-mempool-2
**Repository**: irys

## Research Question

How are PD (Programmable Data) chunks connected across the Irys CL and Reth EL? What primitives are used, what are the communication patterns, and how does the pipeline flow through the Reth block building, Reth mempool, PD chunk requesting, and block validation on both sides?

## Summary

The PD chunk system spans three layers: the **Irys CL** (PdService with chunk cache + provisioning tracker), the **Reth EL** (mempool monitor, payload builder with PD budget, PD precompile), and a **bridge layer** (PdChunkMessage channel, RethChunkProvider trait, PdContext dual-mode accessor). A single `mpsc::unbounded_channel<PdChunkMessage>` is the backbone — the Reth mempool monitor, payload builder, and CL validation all send messages through it; the PdService on the CL side receives and processes them using an LRU chunk cache backed by storage modules.

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────┐
│                         IRYS CL (Consensus Layer)                   │
│                                                                     │
│  ┌──────────────┐    ┌───────────────────┐    ┌──────────────────┐ │
│  │ BlockProducer │───>│  ShadowTxGenerator │───>│  RethService     │ │
│  │  Service      │    │                   │    │  (FCU + payload) │ │
│  └──────────────┘    └───────────────────┘    └──────────────────┘ │
│                                                                     │
│  ┌──────────────────────────────────────────────────────────┐      │
│  │                    PdService                              │      │
│  │  ┌──────────┐  ┌──────────────────┐  ┌────────────────┐ │      │
│  │  │ChunkCache│  │ProvisioningTracker│  │  BlockTracker  │ │      │
│  │  │  (LRU)   │  │ (per-tx FSM)     │  │ (per-block)    │ │      │
│  │  └──────────┘  └──────────────────┘  └────────────────┘ │      │
│  │       ↑                                                   │      │
│  │  Arc<dyn RethChunkProvider> (storage modules → unpack)    │      │
│  └──────────────────────────────────────────────────────────┘      │
│              ↑ PdChunkReceiver                                      │
└──────────────┼──────────────────────────────────────────────────────┘
               │  mpsc::unbounded_channel<PdChunkMessage>
               │
┌──────────────┼──────────────────────────────────────────────────────┐
│              ↓ PdChunkSender                                        │
│                         RETH EL (Execution Layer)                   │
│                                                                     │
│  ┌─────────────────┐   ┌──────────────────────┐                   │
│  │ Mempool Monitor  │──>│  PdChunkMessage::     │                   │
│  │ (100ms poll)     │   │  NewTransaction /     │                   │
│  │                  │   │  TransactionRemoved   │                   │
│  └─────────────────┘   └──────────────────────┘                   │
│                                                                     │
│  ┌─────────────────────────────────────────────────────────┐      │
│  │              IrysPayloadBuilder                          │      │
│  │  ┌──────────────────────┐   ┌────────────────────────┐ │      │
│  │  │CombinedTransactionItr│   │  PdChunkBudget          │ │      │
│  │  │ (shadow → pool txs)  │   │  (max 7500 chunks/blk) │ │      │
│  │  └──────────────────────┘   └────────────────────────┘ │      │
│  │       │ IsReady? ──────────────────> PdChunkSender      │      │
│  └─────────────────────────────────────────────────────────┘      │
│                                                                     │
│  ┌─────────────────────────────────────────────────────────┐      │
│  │              PD Precompile (0x500)                        │      │
│  │  PdContext::get_chunk() ──> Manager mode: GetChunk msg   │      │
│  │                         ──> Storage mode: direct provider │      │
│  └─────────────────────────────────────────────────────────┘      │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Detailed Findings

### 1. Core Primitives

#### 1.1 PdChunkMessage — The Communication Backbone

**File**: `crates/types/src/chunk_provider.rs:37-81`

An enum with 8 variants covering the full PD chunk lifecycle:

| Message | Direction | Pattern | Purpose |
|---------|-----------|---------|---------|
| `NewTransaction { tx_hash, chunk_specs }` | EL→CL | fire-and-forget | Mempool detected PD tx, start provisioning |
| `TransactionRemoved { tx_hash }` | EL→CL | fire-and-forget | Tx left mempool, release references |
| `IsReady { tx_hash, response }` | EL→CL | request-reply (oneshot) | Payload builder checks chunk availability |
| `Lock { tx_hash, response }` | EL→CL | request-reply (oneshot) | Pin chunks during EVM execution |
| `Unlock { tx_hash }` | EL→CL | fire-and-forget | Release pins after execution |
| `GetChunk { ledger, offset, response }` | EL→CL | request-reply (oneshot) | PD precompile fetches chunk data |
| `ProvisionBlockChunks { block_hash, chunk_specs, response }` | CL→CL | request-reply (oneshot) | Validation pre-loads chunks |
| `ReleaseBlockChunks { block_hash }` | CL→CL | fire-and-forget | Cleanup after validation |

**Channel types** (`chunk_provider.rs:84-87`):
- `PdChunkSender = mpsc::UnboundedSender<PdChunkMessage>`
- `PdChunkReceiver = mpsc::UnboundedReceiver<PdChunkMessage>`

#### 1.2 RethChunkProvider — Storage Backend Trait

**File**: `crates/types/src/chunk_provider.rs:21-31`

```rust
pub trait RethChunkProvider: Send + Sync + std::fmt::Debug {
    fn get_unpacked_chunk_by_ledger_offset(&self, ledger: u32, ledger_offset: u64)
        -> eyre::Result<Option<Bytes>>;
    fn config(&self) -> ChunkConfig;
}
```

**Implementation**: `crates/domain/src/models/chunk_provider.rs:129-165`
- `ChunkProvider` fetches packed chunks from storage modules, unpacks via `irys_packing::unpack()`, returns raw `Bytes`

#### 1.3 ChunkRangeSpecifier — Addressing Chunks

**File**: `crates/types/src/range_specifier.rs:103-108`

```rust
pub struct ChunkRangeSpecifier {
    pub partition_index: U200,  // 25 bytes
    pub offset: u32,            // chunk offset within partition
    pub chunk_count: u16,       // number of chunks in range
}
```

Encoded into EIP-2930 access list items under `PD_PRECOMPILE_ADDRESS`. Two access list argument types:
- `ChunkRead` — references N chunks, counted toward budget
- `ByteRead` — references byte ranges within already-declared chunks, counted as 0

#### 1.4 PD Transaction Header

**File**: `crates/irys-reth/src/pd_tx.rs:78-113`

Appended to EVM transaction calldata:
- Magic: `b"irys-pd-meta"` (12 bytes)
- Version: `1` (2 bytes big-endian)
- `max_priority_fee_per_chunk: U256` (32 bytes)
- `max_base_fee_per_chunk: U256` (32 bytes)

Detected via `detect_and_decode_pd_header()` (`pd_tx.rs:134-159`).

#### 1.5 PdContext — Dual-Mode Chunk Access

**File**: `crates/irys-reth/src/precompiles/pd/context.rs:10-25`

```rust
enum ChunkSource {
    Manager(PdChunkSender),   // payload building — cached chunks
    Storage(Arc<dyn RethChunkProvider>),  // block validation — direct storage
}
```

- **Manager mode** (`new_with_manager()`): sends `PdChunkMessage::GetChunk`, waits via oneshot
- **Storage mode** (`new()`): calls `provider.get_unpacked_chunk_by_ledger_offset()` directly
- `clone_for_new_evm()`: creates independent clone per EVM instance to prevent access list contamination

---

### 2. PdService — CL Chunk Manager

**File**: `crates/actors/src/pd_service.rs:18-34`

```rust
pub struct PdService {
    shutdown: Shutdown,
    msg_rx: PdChunkReceiver,
    cache: ChunkCache,                     // LRU with ref counting
    tracker: ProvisioningTracker,           // per-tx FSM
    storage_provider: Arc<dyn RethChunkProvider>,
    block_state_rx: broadcast::Receiver<BlockStateUpdated>,
    current_height: Option<u64>,
    block_tracker: HashMap<B256, Vec<ChunkKey>>,  // per-block validation chunks
}
```

**Spawned at**: `crates/chain/src/chain.rs:1816-1821`, after Reth initialization.

**Main loop** (`pd_service.rs:71-114`): `tokio::select!` over shutdown, `msg_rx`, and `block_state_rx`.

#### Chunk Cache (`pd_service/cache.rs`)

- LRU-backed, 16,384 entries default
- Reference counting: chunks pinned while referenced by transactions
- Lock counting: prevents eviction during EVM execution
- Auto-grows if all entries pinned

```rust
pub struct CachedChunkEntry {
    pub data: Arc<Bytes>,
    pub referencing_txs: HashSet<B256>,
    pub lock_count: u32,
    pub cached_at: Instant,
}
```

#### Provisioning Tracker (`pd_service/provisioning.rs`)

Per-transaction state machine:

```
Provisioning → Ready → Locked
                 ↑        ↓
                 └────────┘ (Unlock)
```

- `PartiallyReady { found, total }`: placeholder for future P2P chunk fetching
- TTL expiration: 20 blocks after registration; locked entries never expire

---

### 3. Reth Mempool — PD Transaction Monitoring

**File**: `crates/irys-reth/src/mempool.rs`

#### Custom Pool Components

| Component | Lines | Purpose |
|-----------|-------|---------|
| `IrysPoolBuilder` | 38-73 | Builds pool with PD monitoring task |
| `IrysShadowTxValidator` | 222-362 | Rejects shadow txs, gates PD on Sprite hardfork, validates min PD fees |
| `PdAwareCoinbaseTipOrdering` | 364-443 | Ranks txs by gas tip + PD tip (`max_priority_fee_per_chunk * chunk_count`) |

#### pd_transaction_monitor Task

**File**: `mempool.rs:480-573`

- Spawned by `IrysPoolBuilder` when pool is built
- Polls pending+queued transactions every **100ms** (`line 466`)
- Detects PD transactions via `detect_and_decode_pd_header()` on calldata
- Extracts chunk specs from access list via `extract_pd_chunk_specs()`
- Sends `PdChunkMessage::NewTransaction` for new PD txs
- Sends `PdChunkMessage::TransactionRemoved` for removed PD txs
- Only operates when Sprite hardfork is active

#### Two Separate Mempool Systems

- **CL Mempool** (`crates/actors/src/mempool_service.rs`): tracks Irys protocol txs (DataTransactionHeader, CommitmentTransaction). Queried by BlockProducer for block assembly. Does NOT interact with Reth pool.
- **EL Mempool** (Reth transaction pool): holds user EVM transactions via `eth_sendRawTransaction`. Custom validation + PD monitoring. Does NOT receive CL transactions.
- **Bridge**: Shadow transactions generated by CL are injected directly into payload attributes, not into the Reth pool.

---

### 4. Reth Block Building Pipeline

#### 4.1 CL Triggers Payload Building

**File**: `crates/actors/src/block_producer.rs`

Flow: `SolutionFound` → `fully_produce_new_block()` → `create_evm_block()` → `build_and_submit_reth_payload()`

**build_and_submit_reth_payload()** (`block_producer.rs:926-1030`):
1. Generate shadow txs via `ShadowTxGenerator` (rewards, fees, staking encoded as EVM txs)
2. Create `IrysPayloadAttributes` with `shadow_txs` field
3. Convert to `IrysPayloadBuilderAttributes` with custom payload ID (includes SHA256 of shadow tx encodings)
4. Submit to Reth: `payload_builder.send_new_payload(attributes)`
5. Wait: `payload_builder.resolve_kind(payload_id, WaitForPending)`
6. Submit to Engine API: `consensus_engine_handle.new_payload()`
7. Verify payload status is Valid

#### 4.2 Payload Builder — Transaction Selection with PD Budget

**File**: `crates/irys-reth/src/payload.rs`

**CombinedTransactionIterator** (`payload.rs:371-625`):

```
1. Yield shadow transactions first (guaranteed inclusion)
2. Check deferred PD txs (may now fit budget)
3. Yield pool transactions:
   - For PD tx: query IsReady → check budget → include/defer/skip
   - For non-PD tx: always include
```

**PdChunkBudget** (`payload.rs:416-482`):
- `max: u64` — hard limit (7,500 chunks per block from Sprite hardfork config)
- `used: u64` — consumed so far
- `deferred: VecDeque` — PD txs that don't fit now
- `accounted: HashMap<B256, u64>` — per-tx chunk counts
- Budget restored when EVM rejects a tx (`on_invalid()`)

**IsReady check** (`payload.rs:559-582`):
- Sends `PdChunkMessage::IsReady` via `pd_chunk_sender`
- Uses `tokio::task::block_in_place()` for blocking recv in sync iterator context
- Skips tx if chunks not ready, continues to next candidate

#### 4.3 Payload Types

**File**: `crates/irys-reth/src/engine.rs`

| Type | Purpose |
|------|---------|
| `IrysPayloadAttributes` (32-54) | RPC-level attributes + `shadow_txs: Vec<EthPooledTransaction>` |
| `IrysPayloadBuilderAttributes` (99-170) | Builder attributes with custom payload ID |
| `IrysBuiltPayload` (179-239) | Wraps `EthBuiltPayload` + `treasury_balance: U256` |

Payload ID computation (`engine.rs:85-91`): SHA256 hash including all shadow tx encodings ensures different shadow txs = different payload IDs.

---

### 5. PD Precompile — Chunk Reading During EVM Execution

**File**: `crates/irys-reth/src/precompiles/pd/precompile.rs`

Registered at address `0x500`. During EVM execution:
1. Validates input data and gas
2. Reads access list from PdContext
3. Dispatches to `ReadFullByteRange` or `ReadPartialByteRange`
4. Calls `context.get_chunk(ledger, offset)` for each needed chunk
5. Returns bytes to EVM

**Byte range reading** (`precompiles/pd/read_bytes.rs:232-349`):
- Converts `ChunkRangeSpecifier` → absolute ledger offsets: `partition_index * num_chunks_in_partition + offset`
- Iterates offsets, fetches each chunk via `context.get_chunk(0, ledger_offset)`
- Extracts requested byte range from concatenated chunk data

---

### 6. Block Validation — Both Sides

#### 6.1 Irys CL Validation

**File**: `crates/actors/src/validation_service.rs`, `crates/actors/src/block_validation.rs`

**Two-stage pipeline:**

**Stage 1 — VDF Validation** (preemptible, single slot):
- Validates VDF step continuity and seed data

**Stage 2 — Concurrent Validation** (6 parallel tasks via `tokio::join!`):
1. Recall range validation
2. PoA validation (blocking, thread pool)
3. **Shadow transactions validation** ← PD critical path
4. Seeds validation
5. Commitment tx ordering
6. Data tx fee validation

#### 6.2 Shadow Transaction Validation — PD Integration Point

**File**: `crates/actors/src/block_validation.rs:1320-1559`

1. **Wait for Reth execution payload** from `ExecutionPayloadCache`
2. **Payload integrity checks**: reject non-V3, withdrawals, blobs, EIP-4844/7685
3. **PD chunk budget validation** (`block_validation.rs:1434-1469`):
   - Only when Sprite hardfork active
   - For each EVM tx: detect PD header → count chunks from access list
   - Reject block if `total_pd_chunks > max_pd_chunks_per_block`
4. **PD chunk provisioning** (`block_validation.rs:1471-1508`):
   - Send `ProvisionBlockChunks { block_hash, chunk_specs }` to PdService
   - All-or-nothing: succeed only if all chunks found locally
   - Fail validation if any chunks missing (returns missing list for diagnostics)
5. **Shadow tx extraction**: verify each shadow tx signer is the miner
6. **Expected shadow tx generation**: includes `compute_pd_base_fee_for_block()`
7. **Shadow tx matching**: actual vs expected must match exactly
8. **Cleanup**: `ReleaseBlockChunks` sent on both success and failure paths

#### 6.3 Reth EL Validation

**File**: `crates/irys-reth/src/validator.rs`, `crates/reth-node-bridge/src/adapter.rs`

- `IrysEngineValidator` wraps Reth's `EthereumExecutionPayloadValidator`
- `submit_payload_to_reth()` (`block_validation.rs:1610-1661`): submits via Engine API `new_payload_v4`, polls until Valid/Accepted
- Fork choice updates sent via `RethService` → `IrysRethNodeAdapter::update_forkchoice_full()`

#### 6.4 EVM Factory — Validation vs Building Mode

**File**: `crates/irys-reth/src/evm.rs:380-512`

| Mode | Construction | Chunk Source |
|------|-------------|-------------|
| **Payload building** | `IrysEvmFactory::with_pd_chunk_sender(sender)` | `PdContext::new_with_manager()` — cached via PdService |
| **Block validation** | `IrysEvmFactory::new(chunk_provider, hardfork)` | `PdContext::new()` — direct storage via RethChunkProvider |

The payload builder's copy gets the sender configured via `ConfigurePdChunkSender` trait; the executor's copy retains `None` (storage mode).

---

### 7. Initialization & Wiring

**File**: `crates/chain/src/chain.rs:1136-1821`

```rust
// 1. Create channel
let (pd_chunk_tx, pd_chunk_rx) = tokio::sync::mpsc::unbounded_channel();

// 2. Start Reth node with sender
//    → flows into IrysEthereumNode → IrysPoolBuilder, IrysPayloadBuilderBuilder, IrysEvmFactory
start_reth_node(chunk_provider, pd_chunk_tx.clone(), ...)

// 3. Spawn PdService with receiver
PdService::spawn_service(pd_chunk_rx, chunk_provider, block_state_rx, runtime_handle)
```

**IrysEthereumNode** (`crates/irys-reth/src/lib.rs:95-104`):
```rust
pub struct IrysEthereumNode {
    pub max_pd_chunks_per_block: u64,
    pub chunk_provider: Arc<dyn RethChunkProvider>,
    pub hardfork_config: Arc<IrysHardforkConfig>,
    pub pd_chunk_sender: PdChunkSender,
}
```

Flows into three component builders:
- `IrysPoolBuilder` — gets `pd_chunk_sender` for mempool monitoring
- `IrysExecutorBuilder` — gets `chunk_provider` for block validation
- `IrysPayloadBuilderBuilder` — gets `pd_chunk_sender` + `max_pd_chunks_per_block`

---

### 8. End-to-End Flow: PD Transaction Lifecycle

```
1. USER submits EVM tx with PD header + access list via eth_sendRawTransaction
        ↓
2. RETH MEMPOOL validates (IrysShadowTxValidator: hardfork gate, min fee check)
        ↓
3. MEMPOOL MONITOR detects PD tx (100ms poll), extracts ChunkRangeSpecifiers
        ↓
4. PdChunkMessage::NewTransaction { tx_hash, chunk_specs } → PdService
        ↓
5. PdService PROVISIONS: converts specs → ledger offsets, fetches from storage, caches
        ↓
6. PAYLOAD BUILDER iterates pool txs:
   a. CombinedTransactionIterator yields shadow txs first
   b. For PD tx: sends IsReady? → PdService responds true/false
   c. If ready: check PdChunkBudget (max 7500 chunks/block)
   d. If fits: include; if not: defer to queue
        ↓
7. EVM EXECUTION: PD precompile calls PdContext::get_chunk()
   → Manager mode: GetChunk msg → PdService → returns Arc<Bytes> from cache
        ↓
8. BLOCK ASSEMBLED: CL wraps EVM payload + Irys metadata
        ↓
9. PEER VALIDATION of block:
   a. Detect PD txs in payload, count chunks, check budget
   b. ProvisionBlockChunks → PdService loads from storage
   c. EVM re-execution with Storage-mode PdContext
   d. Shadow tx matching
   e. Submit to Reth Engine API new_payload_v4
   f. ReleaseBlockChunks cleanup
```

---

## Code References

### Core Types & Traits
- `crates/types/src/chunk_provider.rs:21-31` — `RethChunkProvider` trait
- `crates/types/src/chunk_provider.rs:37-87` — `PdChunkMessage` enum + channel types
- `crates/types/src/range_specifier.rs:103-108` — `ChunkRangeSpecifier`
- `crates/irys-reth/src/pd_tx.rs:78-113` — `PdHeaderV1`

### PdService (CL)
- `crates/actors/src/pd_service.rs:18-34` — PdService struct
- `crates/actors/src/pd_service.rs:38-69` — spawn_service
- `crates/actors/src/pd_service.rs:71-114` — main event loop
- `crates/actors/src/pd_service/cache.rs:19-28` — CachedChunkEntry
- `crates/actors/src/pd_service/provisioning.rs:11-20` — ProvisioningState FSM

### Reth Mempool
- `crates/irys-reth/src/mempool.rs:38-73` — IrysPoolBuilder
- `crates/irys-reth/src/mempool.rs:222-362` — IrysShadowTxValidator
- `crates/irys-reth/src/mempool.rs:364-443` — PdAwareCoinbaseTipOrdering
- `crates/irys-reth/src/mempool.rs:480-573` — pd_transaction_monitor

### Payload Building
- `crates/actors/src/block_producer.rs:926-1030` — build_and_submit_reth_payload
- `crates/irys-reth/src/payload.rs:371-625` — CombinedTransactionIterator + PdChunkBudget
- `crates/irys-reth/src/payload.rs:692-738` — IrysPayloadBuilder::try_build
- `crates/irys-reth/src/engine.rs:32-54` — IrysPayloadAttributes
- `crates/irys-reth/src/payload_builder_builder.rs:57-81` — builder configuration

### PD Precompile
- `crates/irys-reth/src/precompiles/pd/context.rs:10-146` — PdContext + ChunkSource
- `crates/irys-reth/src/precompiles/pd/precompile.rs:19-132` — precompile at 0x500
- `crates/irys-reth/src/precompiles/pd/read_bytes.rs:232-349` — read_bytes_range

### Block Validation
- `crates/actors/src/block_validation.rs:1320-1559` — shadow_transactions_are_valid (PD path)
- `crates/actors/src/block_validation.rs:1434-1469` — PD budget validation
- `crates/actors/src/block_validation.rs:1471-1508` — PD chunk provisioning for validation
- `crates/actors/src/validation_service/block_validation_task.rs:414-464` — shadow tx stage
- `crates/irys-reth/src/validator.rs:29-99` — IrysEngineValidator

### EVM Integration
- `crates/irys-reth/src/evm.rs:380-512` — IrysEvmFactory dual-mode creation
- `crates/irys-reth/src/evm.rs:369-378` — ConfigurePdChunkSender trait

### Wiring
- `crates/chain/src/chain.rs:1141` — channel creation
- `crates/chain/src/chain.rs:1816-1821` — PdService spawn
- `crates/irys-reth/src/lib.rs:95-104` — IrysEthereumNode

### Storage Backend
- `crates/domain/src/models/chunk_provider.rs:129-165` — ChunkProvider impl

## Open Questions

- The mempool monitor uses 100ms polling — is this adequate or would a subscription-based approach be considered?
- `PartiallyReady` state exists in the provisioning tracker but P2P chunk fetching is not yet implemented
- `MockChunkProvider` is used during early Reth init (`chain.rs:1146`); real provider is set later — potential gap?
