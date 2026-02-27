# KZG Commitments, Ingress Proofs, Custody Proofs, and Blobs

A technical reference for engineers working on the Irys protocol.

---

## Table of Contents

1. [Introduction and Motivation](#1-introduction-and-motivation)
2. [KZG Commitments — Conceptual Primer](#2-kzg-commitments--conceptual-primer)
3. [Irys Chunks and KZG Blobs](#3-irys-chunks-and-kzg-blobs)
4. [Ingress Proofs — V1 and V2](#4-ingress-proofs--v1-and-v2)
5. [Ingress Proof Lifecycle](#5-ingress-proof-lifecycle)
6. [EIP-4844 Blob Extraction](#6-eip-4844-blob-extraction)
7. [Custody Proofs](#7-custody-proofs)
8. [Configuration and Rollout](#8-configuration-and-rollout)
9. [Data Flow Diagrams](#9-data-flow-diagrams)
10. [Glossary](#10-glossary)

---

## 1. Introduction and Motivation

Irys is a decentralised data storage network. Miners store 256 KB data chunks, packed with entropy such that mining is data-dependent (Proof of Access). When a user submits a transaction, miners generate **ingress proofs** — cryptographic attestations confirming receipt and storage of the data.

### Why move beyond SHA-256 Merkle proofs?

The original ingress proof (V1) is a SHA-256 Merkle tree root. It demonstrates that all chunks were hashed together to produce that root, but it provides no mechanism to subsequently challenge a miner about *individual* chunks without re-downloading the entire transaction.

KZG commitments address this limitation. A KZG commitment is a compact fingerprint of data that supports **point evaluation**: a verifier may select any position in the data and require the prover to reveal the value at that position, together with a short proof that the value is consistent with the original commitment. The prover cannot fabricate a valid response without breaking a hard cryptographic assumption.

### Why KZG specifically?

- **Established implementation with no custom trusted setup** — KZG commitments are already employed in Ethereum (EIP-4844) and are built into Reth. Irys reuses the same `c-kzg` library and the Ethereum trusted setup ceremony data, thereby avoiding the need to conduct a custom ceremony or maintain a separate cryptographic library.
- **Verification without the original data** — A KZG commitment can be verified against a claimed value at any evaluation point using only the commitment and the opening proof. The verifier never requires access to the original data. This property is what makes custody proofs feasible: a validator can confirm that a miner holds a specific chunk without downloading it.

These properties enable two capabilities:

- **Custody proofs** — Challenge a miner to demonstrate continued storage of specific chunks at random positions, without downloading any data.
- **EIP-4844 blob support** — Ethereum blobs arrive with KZG commitments already attached, permitting Irys to ingest them natively.

### The three subsystems

```
                        ┌─────────────────┐
   User Transaction ──> │  Ingress Proofs  │ ── V1 (Merkle) or V2 (KZG)
                        └────────┬────────┘
                                 │ V2 stores per-chunk commitments
                                 v
                        ┌─────────────────┐
   VDF Challenge ────>  │  Custody Proofs  │ ── "prove you still hold chunk N"
                        └─────────────────┘

   EIP-4844 Blob ────> ┌─────────────────┐
   (from Ethereum)      │  Blob Extraction │ ── converts blobs to Irys transactions
                        └─────────────────┘
```

---

## 2. KZG Commitments — Conceptual Primer

> This section contains no code — only the underlying concepts.

### Polynomials as data containers

Consider a list of N numbers representing your data. One may construct a polynomial p(x) of degree N−1 that passes through all of them: p(0) = data[0], p(1) = data[1], …, p(N−1) = data[N−1]. This polynomial *is* the data, merely expressed in a different form.

### Committing to a polynomial

A **KZG commitment** is a single elliptic curve point that serves as a fingerprint of a polynomial. Given a polynomial p(x), the commitment is computed as:

```
C = p(s) · G
```

where `s` is a secret number from a one-time trusted setup ceremony and `G` is a generator point on the BLS12-381 elliptic curve. No party knows `s` — it was destroyed after the ceremony — but the ceremony produced precomputed "powers of s" that permit anyone to commit without knowledge of `s` itself.

The commitment `C` is 48 bytes (a compressed BLS12-381 G1 point).

### Opening proofs (point evaluation)

Given commitment `C`, a verifier selects a challenge point `z` and requests: "What is p(z)?"

The prover responds with:

- `y = p(z)` — the evaluation value (32 bytes)
- `π` — an opening proof (48 bytes)

The verifier checks the proof using a pairing equation on the elliptic curve. If the proof is valid, the verifier is satisfied that `p(z) = y` for the polynomial committed by `C`, without having seen the polynomial itself.

This is the core property upon which custody proofs depend.

### The BLS12-381 curve

KZG employs the BLS12-381 elliptic curve. Its scalar field has a modulus beginning with `0x73eda753…`. When converting data bytes into field elements (the "numbers" through which the polynomial passes), each 32-byte element must be numerically less than the field modulus. Since the modulus begins with `0x73` (115), any element whose first byte is ≥ `0x74` (116) is guaranteed to exceed it. Elements beginning with `0x73` may or may not exceed it depending on subsequent bytes. For uniform-fill blobs in tests, the safe upper bound for the fill byte is 114 (`MAX_VALID_SEED` in `kzg.rs`). In practice, the `c-kzg` library handles this encoding internally for blob data.

### Trusted setup

The precomputed ceremony data is loaded once and cached as a static reference (`&'static KzgSettings`). It originates from Ethereum's KZG ceremony (the same one used for EIP-4844). In the codebase, it is accessed via `default_kzg_settings()`, which lazily initialises from `EnvKzgSettings::Default.get()`.

---

## 3. Irys Chunks and KZG Blobs

### The size disparity

| Concept | Size |
|---------|------|
| Irys chunk | 256 KB (`CHUNK_SIZE_FOR_KZG = 262_144`) |
| EIP-4844 blob | 128 KB (`BLOB_SIZE = 131_072`) |
| KZG commitment | 48 bytes (`COMMITMENT_SIZE`) |
| KZG opening proof | 48 bytes (`PROOF_SIZE`) |
| Field element / scalar | 32 bytes (`SCALAR_SIZE`) |

An Irys chunk is twice the size of a KZG blob. The solution is to **split each chunk into two blob-sized halves**.

### How a chunk becomes a commitment

```
                    256 KB chunk (zero-padded if shorter)
                    ┌────────────────────────────────────┐
                    │  first 128 KB   │  second 128 KB   │
                    └────────┬────────┴────────┬─────────┘
                             │                 │
                    blob_to_kzg_commitment     blob_to_kzg_commitment
                             │                 │
                             v                 v
                            C1                C2
                             │                 │
                             └──────┬──────────┘
                                    │
                           r = SHA256(C1 || C2)
                           C = C1 + r·C2
                                    │
                                    v
                        Single chunk commitment (48 bytes)
```

**Step by step:**

1. **Pad** — If the chunk is shorter than 256 KB, zero-pad it to exactly 256 KB.
2. **Split** — Divide at the 128 KB boundary into two halves.
3. **Commit each half** — Each half is treated as an EIP-4844 blob and committed separately using `blob_to_kzg_commitment` from the `c-kzg` library.
4. **Aggregate** — Combine `C1` and `C2` into a single commitment using a random linear combination: compute `r = SHA256(C1 || C2)`, then `C = C1 + r·C2`.

The aggregation derives `r` from the commitments themselves so that no party can craft two different pairs of halves that produce the same aggregated commitment (by the Schwartz–Zippel lemma, the collision probability is negligible over the BLS12-381 scalar field).

### Multi-chunk transactions

A transaction may comprise many chunks. Each chunk receives its own commitment as described above, then all chunk commitments are aggregated into a **single transaction-level commitment** via iterative pairwise aggregation:

```
C_tx = aggregate(aggregate(aggregate(C_chunk0, C_chunk1), C_chunk2), C_chunk3)
```

This is left-associative — **ordering matters**.

### Code references

| Function | File | Purpose |
|----------|------|---------|
| `pad_and_split_chunk` (private) | `crates/types/src/kzg.rs` | Zero-pad and split into two halves |
| `compute_blob_commitment` | `crates/types/src/kzg.rs` | KZG commitment for one 128 KB blob |
| `compute_chunk_commitment` | `crates/types/src/kzg.rs` | Full pipeline: pad → split → commit → aggregate |
| `aggregate_commitments` | `crates/types/src/kzg.rs` | C = C1 + r·C2 for two commitments |
| `aggregate_all_commitments` | `crates/types/src/kzg.rs` | Iterative pairwise aggregation of N commitments |
| `g1_add_scaled` | `crates/types/src/kzg.rs` | Low-level BLS12-381 G1 point arithmetic via blst FFI |

---

## 4. Ingress Proofs — V1 and V2

An **ingress proof** is a signed attestation from a miner confirming receipt and storage of a transaction's data. It is included in blocks and gossiped across the network.

### V1 — SHA-256 Merkle Proof (legacy)

```rust
// crates/types/src/ingress.rs
pub struct IngressProofV1 {
    pub signature: IrysSignature,  // excluded from RLP (recomputed during verification)
    pub data_root: H256,           // Merkle root of signer-dependent ingress leaves
    pub proof: H256,               // Merkle tree node ID
    pub chain_id: u64,             // replay protection
    pub anchor: H256,              // block hash for expiry
}
```

The `data_root` is computed from **signer-dependent leaves** — the Merkle tree includes the signer's address in each leaf hash. This binds the proof to a specific miner but provides no mechanism to query individual chunks.

### V2 — KZG Commitment

```rust
// crates/types/src/ingress.rs
pub struct IngressProofV2 {
    pub signature: IrysSignature,
    pub data_root: H256,                    // Merkle root of regular leaves (signer-independent)
    pub kzg_commitment: KzgCommitmentBytes, // aggregated KZG commitment over all chunks (48 bytes)
    pub composite_commitment: H256,         // SHA256(DOMAIN || kzg || signer_address)
    pub chain_id: u64,                      // replay protection
    pub anchor: H256,                       // block hash for expiry
    pub source_type: DataSourceType,        // NativeData(0) or EvmBlob(1)
}
```

Key differences from V1:

| Aspect | V1 | V2 |
|--------|----|----|
| Data fingerprint | SHA-256 Merkle root | KZG commitment (elliptic curve point) |
| Point evaluation | Not possible | Supported — enables custody proofs |
| Signer binding | Incorporated into data_root leaves | Separate composite_commitment field |
| Data source tracking | Not applicable | NativeData or EvmBlob |
| data_root computation | Signer-dependent leaves | Regular (signer-independent) leaves |

### Composite commitment — rationale

The KZG commitment `C` depends solely on the data, not on who computed it. Two miners storing the same transaction would produce identical KZG commitments. Without an additional binding step, one miner could replicate another's commitment.

The **composite commitment** prevents this:

```
composite = SHA256("IRYS_KZG_INGRESS_V1" || kzg_commitment || signer_address)
```

The domain separator (`IRYS_KZG_INGRESS_V1`) prevents cross-protocol confusion. The signer address binds the commitment to a specific miner. Together, they ensure that each miner's proof is unique even for identical data.

### Version gating

The `IngressProof` enum employs the `IntegerTagged` macro for versioning — V1 carries discriminant 1, V2 carries discriminant 2. Acceptance is governed by configuration flags:

```
check_version_accepted(accept_kzg, require_kzg):
  - V2 proof + !accept_kzg → rejected
  - V1 proof + require_kzg → rejected
  - otherwise → accepted
```

---

## 5. Ingress Proof Lifecycle

### Generation

When a miner receives a new data transaction, it generates an ingress proof:

```
crates/actors/src/mempool_service/ingress_proofs.rs  (orchestration)
  │
  └─> crates/actors/src/mempool_service/chunks.rs     (core logic)
        │
        └─> crates/types/src/ingress.rs                (proof construction)
              │
              └─> crates/types/src/kzg.rs              (KZG primitives)
```

**Detailed flow (`generate_ingress_proof` in `chunks.rs`):**

1. **Collect chunks** — Read all chunks for the transaction from the cache database (`CachedChunksIndex` table), verifying uniqueness and ordering.
2. **Branch on configuration** — If `use_kzg_ingress_proofs` is true, generate V2; otherwise V1.
3. **V2 path**:
   - Compute per-chunk KZG commitments (each chunk → `compute_chunk_commitment`).
   - Aggregate all chunk commitments → single `kzg_commitment`.
   - Compute `composite_commitment` binding to the signer.
   - Construct `IngressProofV2` and sign it.
4. **Store** — Write the proof and per-chunk commitments to the database.

### Storage

Two database tables are involved:

| Table | Key | Value | Purpose |
|-------|-----|-------|---------|
| `IngressProofs` | (DataRoot, IrysAddress) | CompactCachedIngressProof | Store the complete proof |
| `PerChunkKzgCommitments` | (DataRoot, chunk_index) | CompactPerChunkCommitment (wraps `PerChunkCommitment { chunk_index, commitment: KzgCommitmentBytes }`) | Store individual chunk commitments for subsequent custody verification |

The per-chunk commitments are stored separately because custody proofs require the ability to look up the commitment for a specific chunk by index, not the aggregated commitment.

### Gossip

Following generation, the proof is broadcast to the network:

```
gossip_ingress_proof()
  │
  └─> GossipBroadcastMessageV2 {
        key: GossipCacheKey::IngressProof(proof.proof_id()),
        data: GossipDataV2::IngressProof(proof)
      }
```

The `proof_id()` method is used for deduplication in the gossip cache:

- V1: the Merkle proof hash
- V2: the composite commitment

Peers receive proofs via the `/gossip/v2/ingress_proof` HTTP endpoint (`crates/p2p/src/server.rs`), which delegates to the gossip data handler for validation and forwarding to the mempool.

### Validation (in blocks)

When validating a block, each ingress proof is checked:

1. **Version acceptance** — `check_version_accepted(accept_kzg, require_kzg)`
2. **Full data availability verification** (if enabled) — Reconstruct chunks from the database and invoke `verify_ingress_proof`, which recomputes the KZG commitments from scratch and verifies they match.
3. **Unique signer enforcement** — Each signer may only have one proof per transaction per block.

---

## 6. EIP-4844 Blob Extraction

### What are EIP-4844 blobs?

EIP-4844 (Proto-Danksharding) introduces a new transaction type to Ethereum that carries large data payloads called **blobs**. Each blob is 128 KB and arrives with a KZG commitment in a **sidecar** — metadata attached to the transaction but not executed by the EVM.

Irys employs its Reth-based execution layer to process Ethereum transactions. When blob transactions arrive, Irys can extract the blob data and ingest it as native Irys data.

### BlobExtractionService

```
crates/actors/src/blob_extraction_service.rs
```

This actor receives `ExtractBlobs` messages containing block hashes and blob transaction hashes. For each blob:

```
EIP-4844 Blob (128 KB + KZG commitment from sidecar)
  │
  ├─ Take the KZG commitment directly from the sidecar (no recomputation)
  ├─ Zero-pad blob data from 128 KB → 256 KB (Irys chunk size)
  ├─ Compute data_root from the padded data (regular leaves, not signer-dependent)
  ├─ Compute composite_commitment binding KZG to the signer
  ├─ Construct IngressProofV2 with source_type = EvmBlob
  │
  └─> Create synthetic DataTransactionHeader
      │
      └─> Send IngestBlobDerivedTx to the mempool
```

**Key observation:** Following extraction, blob-derived data is indistinguishable from native data within the mempool and storage systems. It possesses a regular transaction header, an ingress proof, and chunk data — the same structures used for native transactions.

**Why not recompute the KZG commitment?** The blob sidecar already contains a KZG commitment computed using the same trusted setup (Ethereum's KZG ceremony). Recomputing it would be redundant and computationally expensive.

### Configuration

Blob extraction is gated by the `enable_blobs` flag. Enabling it automatically implies `accept_kzg_ingress_proofs` (since blob-derived proofs are always V2).

---

## 7. Custody Proofs

### Purpose

Once data is stored, how does the network verify that miners are *still* storing it? Custody proofs answer this question through a challenge-response protocol.

A verifier selects random chunk positions in a miner's partition and demands: "At position N, what does your data evaluate to?" The miner must produce a KZG opening proof for each challenged position. If the proofs match the stored per-chunk commitments, the miner is verified. If they do not, the miner is penalised.

### Challenge derivation

Challenges are deterministic — any party can compute the same challenges given the same inputs:

```
challenge_seed = SHA256(vdf_output || partition_hash)
```

The VDF output provides unpredictable timing (miners cannot prepare in advance), and the partition hash identifies which partition is being challenged.

From the seed, **k** chunk offsets are selected (default k = 20):

```
For j = 0..k:    (j is u32)
    hash = SHA256(challenge_seed || j.to_le_bytes())    // 4-byte LE
    offset_j = u32(first_8_bytes_as_u64(hash) % num_chunks_in_partition)
```

For each offset, an evaluation point is derived:

```
z_j = SHA256(challenge_seed || offset_j.to_le_bytes()) mod BLS12-381_r    // offset_j is u32, 4-byte LE
```

The `mod BLS12-381_r` step ensures the point is a valid scalar in the BLS12-381 field.

### Proof generation

The `CustodyProofService` (`crates/actors/src/custody_proof_service.rs`) is responsible for handling challenges.

When a block reaches `ChainState::Onchain`, `block_tree_service.rs` sends `CustodyProofMessage::NewBlock { vdf_output: H256, block_height: u64 }` to the service (gated by `enable_custody_proofs`). The service's `handle_new_block` method iterates all local storage modules, derives a challenge seed for each partition, and invokes `handle_challenge` internally — thereby self-challenging on every confirmed block.

```
CustodyChallenge received (via NewBlock self-challenge or peer gossip)
  │
  ├─ Locate storage module matching partition_hash
  │   (if none found, this node does not own the partition — skip)
  │
  └─ For each of k challenged offsets:
      │
      ├─ Read packed chunk from storage module
      ├─ Unpack it (reverse multi-iteration entropy packing → plaintext)
      ├─ Derive evaluation point: z = derive_challenge_point(seed, offset)
      ├─ Compute opening proof on the plaintext:
      │    (proof_bytes, y_bytes) = compute_chunk_opening_proof(data, z, settings)
      │
      │   Internally, this:
      │     1. Pads the chunk to 256 KB and splits into two 128 KB halves
      │     2. Computes a blob proof for each half: (π1, y1) and (π2, y2)
      │     3. Aggregates: π = π1 + r·π2, y = y1 + r·y2
      │        where r = SHA256(C1 || C2)
      │
      └─ Construct CustodyOpening {
           chunk_offset,
           data_root,          // identifies the transaction
           tx_chunk_index,     // position within the transaction
           evaluation_point,   // z (32 bytes)
           evaluation_value,   // y (32 bytes)
           opening_proof       // π (48 bytes)
         }
  │
  └─ Assemble CustodyProof with all openings
     └─ Gossip to network via GossipDataV2::CustodyProof
```

The gossip sending and receiving paths are fully wired. On the receiving side, `handle_custody_proof_v2` in `server.rs` caches the proof for deduplication, then forwards it via `CustodyProofMessage::ReceivedProof` to the `CustodyProofService`. The service's `handle_received_proof` method verifies the proof against stored per-chunk commitments and, if valid, adds it to the pending proofs list. The block producer drains pending proofs via `TakePendingProofs` when assembling blocks.

### Verification

The verification function `validate_custody_proofs` in `block_validation.rs` is wired into `validate_block()` as a parallel `tokio::join!` task in `block_validation_task.rs`, alongside recall, PoA, shadow transaction, seeds, commitment ordering, and data transaction validation.

```rust
// crates/types/src/custody.rs
pub fn verify_custody_proof(
    proof: &CustodyProof,
    get_commitment: impl Fn(H256, u32) -> eyre::Result<Option<KzgCommitmentBytes>>,
    kzg_settings: &KzgSettings,
    expected_challenge_count: u32,
    num_chunks_in_partition: u64,
) -> eyre::Result<CustodyVerificationResult>
```

Verification proceeds as follows:

1. **Check opening count** — There must be exactly `expected_challenge_count` openings.
2. **Recompute expected offsets** — Derived from the challenge seed (deterministic, publicly verifiable).
3. **For each opening:**
   - Verify that `chunk_offset` matches the expected offset.
   - Look up the stored per-chunk KZG commitment via `get_commitment(data_root, tx_chunk_index)` — these were stored during ingress proof generation.
   - Invoke `verify_chunk_opening_proof(commitment, z, y, π)` — the core KZG verification.
4. **Return a result** — one of:

| Result | Meaning |
|--------|---------|
| `Valid` | All openings verified successfully |
| `InvalidOpeningCount` | Incorrect number of openings |
| `InvalidOffset` | Opening at an unexpected chunk position |
| `MissingCommitment` | No stored commitment for this chunk (data was never ingested) |
| `InvalidProof` | KZG verification failed — the miner does not hold the correct data |

### Penalties

If verification fails, a **CustodyPenalty** shadow transaction is generated. Shadow transactions are protocol-level actions encoded as EVM transactions:

```
CustodyPenaltyPacket {
    amount: U256,              // tokens to deduct
    target: Address,           // penalised miner
    partition_hash: FixedBytes<32>,  // which partition failed (alloy_primitives, not irys H256)
}
```

The penalty is Borsh-encoded, prefixed with the `IRYS_SHADOW_EXEC` marker (`b"irys-shadow-exec"`), and sent to `SHADOW_TX_DESTINATION_ADDR`. The Irys EVM extension detects this prefix in the transaction input and executes the encoded action (deducting funds from the miner's account).

### Code references

| Component | File |
|-----------|------|
| Challenge types | `crates/types/src/custody.rs` |
| Proof generation service | `crates/actors/src/custody_proof_service.rs` |
| KZG opening primitives | `crates/types/src/kzg.rs` |
| Verification | `crates/types/src/custody.rs` — `verify_custody_proof` |
| Penalty shadow transaction | `crates/irys-reth/src/shadow_tx.rs` |

---

## 8. Configuration and Rollout

### Configuration flags

All flags reside in `ConsensusConfig` (`crates/types/src/config/consensus.rs`):

| Flag | Default | Description |
|------|---------|-------------|
| `enable_shadow_kzg_logging` | false | Compute V2 commitments alongside V1 proofs and log for comparison. Non-consensus — purely for testing. |
| `use_kzg_ingress_proofs` | false | Generate V2 (KZG) proofs for new transactions instead of V1. |
| `accept_kzg_ingress_proofs` | false | Accept V2 proofs from peers during validation. |
| `require_kzg_ingress_proofs` | false | Reject V1 proofs entirely. Implies `accept_kzg_ingress_proofs`. |
| `enable_blobs` | false | Extract EIP-4844 blobs and ingest as Irys transactions. Implies `accept_kzg_ingress_proofs`. |
| `enable_custody_proofs` | false | Run the custody proof challenge-response protocol. Requires `accept_kzg_ingress_proofs`. |
| `custody_challenge_count` | 20 | Number of random chunk positions challenged per custody proof. |
| `custody_response_window` | 10 | Number of blocks a miner has to respond to a custody challenge. |

### Flag dependencies

The `normalize()` method enforces logical implications at startup:

```
enable_blobs=true          ──┐
require_kzg_proofs=true    ──┤
use_kzg_proofs=true        ──┼──> accept_kzg_ingress_proofs = true
enable_custody_proofs=true ──┘
```

If any of these flags is true but `accept_kzg_ingress_proofs` is false, `normalize()` auto-enables it with a warning log.

The `validate()` method (`crates/types/src/config/mod.rs`) performs the same checks but returns hard errors — catching contradictions that survive after normalisation.

### Rollout strategy

The flags enable a phased rollout:

```
Phase 1: Shadow Mode
  enable_shadow_kzg_logging = true
  (V1 proofs remain in use; V2 computed and logged for comparison)

Phase 2: Accept
  accept_kzg_ingress_proofs = true
  (V2 proofs accepted from peers; V1 still generated locally)

Phase 3: Use
  use_kzg_ingress_proofs = true
  (This node generates V2 proofs; V1 still accepted from others)

Phase 4: Require
  require_kzg_ingress_proofs = true
  (V1 proofs rejected — full network migration complete)

Phase 5: Custody and Blobs
  enable_custody_proofs = true
  enable_blobs = true
  (Full end-to-end wiring: challenge issuance on confirmed blocks,
   proof generation, gossip broadcast, peer receipt and verification,
   pending proof collection by the block producer, and block validation.)
```

---

## 9. Data Flow Diagrams

### Native data transaction — end to end

```
User submits transaction with N chunks
  │
  v
MempoolService receives tx + chunks
  │
  ├─ Store chunks in CachedChunks DB table
  │
  └─ generate_ingress_proof()
      │
      ├─ For each chunk:
      │    compute_chunk_commitment(chunk_data)
      │      └─ pad → split → commit_half_1 → commit_half_2 → aggregate
      │
      ├─ aggregate_all_commitments([C0, C1, ..., CN-1])
      │    └─ iterative pairwise: C = aggregate(C_prev, C_next)
      │
      ├─ compute_composite_commitment(C_tx, signer_address)
      │    └─ SHA256(DOMAIN || C_tx || address)
      │
      ├─ Sign proof
      │
      └─ store_proof_and_commitments()
          ├─ IngressProofs table: (data_root, address) → proof
          └─ PerChunkKzgCommitments table: (data_root, i) → C_i
  │
  v
Gossip IngressProof to peers
  │
  v
Block producer includes proof in block
  │
  v
Block validators check proof
  │                                 Subsequently...
  v                                   │
Data stored in partitions             v
                              Block confirmed on-chain
                              (ChainState::Onchain)
                                      │
                                      v
                              BlockTreeService sends NewBlock
                              (VDF output from block's vdf_limiter_info)
                                      │
                                      v
                              CustodyProofService::handle_new_block
                                ├─ Derive challenge_seed per partition
                                ├─ Locate storage module
                                ├─ For each offset:
                                │    unpack chunk
                                │    compute_chunk_opening_proof
                                └─ Gossip CustodyProof
                                      │
                                      v
                              Peers receive via gossip
                                ├─ Verify proof (handle_received_proof)
                                └─ Store as pending
                                      │
                                      v
                              Block producer includes proofs
                                      │
                                      v
                              validate_custody_proofs (in validate_block)
                                ├─ Recompute offsets
                                ├─ Look up per-chunk commitments
                                ├─ Verify each opening
                                └─ Valid → OK  /  Invalid → CustodyPenalty
```

### EIP-4844 blob path

```
Ethereum blob transaction (128 KB blob + KZG commitment in sidecar)
  │
  v
BlobExtractionService::process_single_blob()
  │
  ├─ Take KZG commitment from sidecar (no recomputation)
  ├─ Zero-pad blob: 128 KB → 256 KB
  ├─ Compute data_root from padded data
  ├─ Compute composite_commitment
  ├─ Create IngressProofV2 (source_type = EvmBlob)
  │
  └─ Create synthetic DataTransactionHeader
      │
      └─ IngestBlobDerivedTx → MempoolService
          │
          └─ (same flow as native data from this point)
```

---

## 10. Glossary

| Term | Definition |
|------|-----------|
| **BLS12-381** | The elliptic curve employed for KZG commitments. Provides approximately 128-bit security. |
| **Blob** | A 128 KB data payload (EIP-4844). Irys chunks are 256 KB, equivalent to two blobs. |
| **Chunk** | A 256 KB unit of data in Irys storage. |
| **Commitment (KZG)** | A 48-byte elliptic curve point that uniquely fingerprints a polynomial (and thus the data it represents). |
| **Composite commitment** | SHA256(DOMAIN \|\| KZG commitment \|\| signer address). Binds a KZG commitment to a specific miner. |
| **Custody challenge** | A request for a miner to prove continued storage of specific chunks in a partition. |
| **Custody proof** | A miner's response: KZG opening proofs at the challenged positions. |
| **Domain separator** | The bytes `IRYS_KZG_INGRESS_V1` prepended to hashes to prevent cross-protocol confusion. |
| **Evaluation point (z)** | A 32-byte scalar at which the polynomial is evaluated during a custody challenge. |
| **Field element** | A 32-byte number in the BLS12-381 scalar field. Must be less than the field modulus (which begins with `0x73`); a first byte ≥ `0x74` is always invalid. |
| **G1 point** | A point on the BLS12-381 G1 curve (48 bytes compressed). Commitments and proofs are G1 points. |
| **Ingress proof** | A signed attestation that a miner has received and stored a transaction's data. |
| **KZG** | Kate–Zaverucha–Goldberg — the authors of the polynomial commitment scheme. |
| **Opening proof (π)** | A 48-byte proof that a polynomial evaluates to a specific value at a specific point. |
| **Partition** | A logical storage unit that a miner manages. Contains many chunks. |
| **Per-chunk commitment** | The KZG commitment for a single chunk, stored in the database for custody verification. |
| **Shadow transaction** | A protocol-level action (such as custody penalties) encoded as an EVM transaction. |
| **Sidecar** | Metadata attached to an EIP-4844 blob transaction, including the KZG commitment. |
| **Trusted setup** | A one-time ceremony that produces the cryptographic parameters (`KzgSettings`) required for KZG. |
| **VDF** | Verifiable Delay Function. Provides unpredictable timing for custody challenges. |
