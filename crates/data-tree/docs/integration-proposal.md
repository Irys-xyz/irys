# Data Tree v2 Integration Proposal — NOT ratified, NOT implemented

This is the planned L1 and gateway integration. It may change. It is not ratified and not implemented. On any conflict, [spec.md](spec.md) wins. The index is [README.md](README.md).

## 1. Summary

These rows are the integration half of the summary. The data-tree rows are in spec.md §1.

| Area | v1 (today) | v2 |
| --- | --- | --- |
| Tx header | `data_root`, `data_size` | `data_tree_root` in the same field, `data_size`; the commitment is derived; no new version tag |
| Chunk message | `data_root`, `data_size` (a claim), offset, `data_path`, bytes | `data_commitment`, `data_size` (checked by the proof), chunk index, proof, bytes |
| Key for data in caches and storage | `data_root` | `data_commitment` |
| Size handling at ingress | provisional sizes, chunk parking on larger claims, rightmost-chunk confirmation, size re-checks from storage | none: the size is final when the header arrives |
| Chunk of the wrong length | dropped silently | rejected as a peer fault |

| Goal | Change that gives it | Effect |
| --- | --- | --- |
| Simpler, stricter ingress | the size is final at the header; a wrong length is a peer fault | less cached state, no size races, no silent drops |

The price also includes a hardfork with a testnet reset (§6).

## 2. Consumer anchors

The anchoring rule is spec.md §6. This table is who anchors a commitment.

| Consumer | Anchor |
| --- | --- |
| L1 chunk ingress, PoA, mining, migration | the signed tx header |
| Gateway receipt verifier | a `tx_root` v2 leaf proven against a `tx_root` that is itself anchored. The receipt carries no signed block header, so that `tx_root` is the relay quorum attestation (Appendix A.2). A leaf proven against a `tx_root` the verifier computed itself is unauthenticated. The leaf binds `tx_id`, `data_tree_root`, the exact `data_size`, `prefix_hash`, `prefix_size` and `chunk_start`, so the commitment is `node(data_tree_root, size_witness(G, data_size))`. The count witness binds `N` (Appendix C) |
| Gateway staging ingest | the item envelope, which signs `data_tree_root` and `data_size`. `data_commitment` is derived (§10 Q7) |
| Gateway relay | its payload-quorum attestation of the tx |
| Gateway data service | tx records from the Indexer are not authenticated today. They need the signed header or a `tx_root` proof before the service deletes its size checks |

## 3. Transaction header and chunk message

The data tx header keeps its layout and gets no new version tag (decided). From activation, the 32-byte field that holds `data_root` today holds `data_tree_root`, and code names it `data_tree_root`.

| Header field | Size | Signed | Note |
| --- | --- | --- | --- |
| `data_tree_root` | 32 B | yes | same position as today's `data_root` |
| `data_size` | 8 B | yes | unchanged |
| `data_commitment` | — | — | derived on decode (one hash); never encoded |

- Both inputs are signed. The commitment is derived from them under the consensus geometry, so the size and the geometry are bound to the commitment. Only chunk proofs show that the data matches it.
- Some chunks of a tx whose `data_tree_root` was built for another size can verify under the signed size. A chunk can verify at its own index or at another index, including the last. Its chunks never all verify, so the tx never completes and expires like any unsupplied upload. Never treat a last-chunk accept, or any single chunk accept, as a size or completion signal. Completion means every chunk verified.
- From Estuary, `tx_root` is v2 (Appendix C). The leaf is `H(0x05 ‖ tx_id ‖ data_tree_root ‖ be64(data_size) ‖ prefix_hash ‖ be64(prefix_size) ‖ be64(chunk_start))` (121 B). A proven leaf binds those fields. A verifier re-derives `node(data_tree_root, size_witness(G, data_size))`.

| Chunk message field (gossip, `POST /v1/chunk`, gateway batches) | Size | Note |
| --- | --- | --- |
| `data_commitment` | 32 B | the anchor; the receiver looks it up (L1: from tx headers) |
| `data_size` | 8 B | a separate field; the proof's last hash is `size_witness(G, data_size)`, checked before the climb |
| `tx_offset` | 4 B | the chunk index `c` |
| `data_path` | `32 × (depth(c, C) + 1)` | chunk-level siblings, then the size witness; 32 B when `C = 1` |
| `bytes` | `chunk_len(data_size, c)` | any other length is invalid before hashing |

Ingress-proof rows use `data_commitment`, never `data_tree_root`.

## 4. Consensus rules on L1

**Chunk ingress.** Verify every chunk with `verify_chunk` against the commitment derived from the tx header.

- A chunk whose length is not `chunk_len(data_size, c)` is `InvalidChunkSize`: a peer fault. Today such a chunk is dropped silently.
- A chunk message whose `data_size` differs from the header is invalid.
- Deleted for v2: provisional size raises; parking on a larger size claim and its rescue; the `layout_data_size` fallback; the rightmost-chunk position check; confirmation on the rightmost chunk; size re-checks from storage modules; the consensus rule `InvalidPromotionDataSizeMismatch` and the `tx_selector` size filter (both read node-local size state); the `data_size_confirmed` field and `confirm_data_size_for_data_root`. Every v2 size is final when the header arrives.
- Kept: parking chunks that arrive before their header. Parking handles arrival order, not size. Once the header arrives, a parked chunk is verified in full.

**PoA offset mapping.** A data PoA names a partition and an offset. Those give the absolute ledger chunk index `b`: `slot_index × num_chunks_in_partition + partition_chunk_offset` (`poa_is_valid` in `crates/actors/src/block_validation.rs`). Txs fill a ledger in whole chunks, in order. Chunk placement comes only from `chunk_start`, never from sums of `data_size`. `chunk_start` is the absolute ledger chunk index where the tx begins (Appendix C). A byte offset, where one is needed, is `chunk_start × chunk_size`, with checked multiplication. The product can overflow `u64`. An unchecked multiply of `chunk_start` = 2^46 and mainnet `chunk_size` 262,144 wraps to 0. The `tx_root` v2 leaf binds `chunk_start`. Block validation checks the spec.md §2 bounds on every `data_size` and checks the absolute starts from the parent's `total_chunks` (Appendix C).

```
require b ≥ chunk_start                                          // before the subtraction
c = b − chunk_start                                              // checked arithmetic
require c < ceil(data_size / chunk_size)
```

A check that fails makes the PoA invalid. Checked subtraction rejects a `chunk_start` above `b`. An unsigned wrap of `b − chunk_start` is not a chunk index.

The owner of chunk `b` is the one tx whose range contains `b`: `chunk_start ≤ b < chunk_start + ceil(data_size / chunk_size)`. Every ledger tx has `1 ≤ data_size` and `ceil(data_size / chunk_size) ≤ max_data_tx_chunks` (Appendix C, the spec.md §2 bounds). Each tx covers at least one chunk, so `chunk_start` is strictly increasing. Each ledger chunk in this block has exactly one owner. A ledger chunk below `chunk_start_0` is not in this block. A lookup by `chunk_start` is unambiguous. A zero-size data tx is rejected. `data_tx_structural_defect` in `crates/actors/src/data_tx_validation.rs` returns `ZeroDataSize` when `data_size == 0`. Block prevalidation maps that defect to `ZeroSizeDataTx` (`crates/actors/src/block_validation.rs`). A zero-width leaf shares its start offset with the next tx, so those start offsets are no longer unique (`is_poa_owning_leaf`).

- Each tx starts on a chunk boundary, so segment `j` of the packed chunk at `b` is segment `j` of the tx's chunk `c`.
- The PoA verifies the owning tx with `verify_tx` (Appendix C). Which fields the PoA carries is open (Appendix C item 2).
- The whole-chunk offset mapping lands with v2. Today's mapping uses raw-size byte ranges and drifts after any tx that does not end on a chunk boundary.
- The PoA rule itself (which segment, the ticket, the forms) is in the segment-PoA spec.

**Segments per chunk.** The tree allows 1 ≤ SPC ≤ 256 (spec.md §2). Wherever the segment-PoA rule runs, the irys node's consensus config rejects SPC < 2 at load time (segment-PoA spec §6.3 rule 3, §10 Q9). That bound is on this use of the tree. The tree stays general. The original reason (an empty segment proof) no longer applies since spec.md §15 Q10. Whether segment-PoA needs SPC ≥ 2 for another reason is open. Settle it against the segment-PoA spec during integration. A segment proof is never empty: when `h = 0` and `C = 1` it is the size witness (spec.md §5, spec.md §15 Q10).

**Other verifiers.** Mining (winning chunks), chunk migration and storage-module serving use `verify_chunk` or the v2 builder against the header commitment. Ingress-proof checks follow §7. A storage module derives `c` from the placement (partition offset minus the tx's start), never from proof bytes. A root does not prove that every chunk has a preimage. PoA, ingress proofs and custody detect that, and the upload never completes.

**Activation.** Let E be Estuary's activation block: the first epoch block whose timestamp is at or after the activation time. The v2 rules apply from the first child of E; E itself uses the old rules.

- **Data txs up to activation.** No block up to and including E carries a data tx. Mainnet refuses data txs in those blocks, and test networks restart from a reset. The header's root field changes meaning at E and has no version tag. Refusing these txs means none can carry the v1 meaning into v2, so the fork release needs no v1 data code and no anchor rule. The release-freeze mainnet scan (§6) is the precondition.

## 5. Security properties

A wrong-length chunk is a peer fault, not a silent drop.

The other security properties are in spec.md §10.

## 6. Rollout

v2 ships at Estuary's activation, together with the partition resize, the segment-PoA rule and `tx_root` v2 (Appendix C). There is one switch and no period where v1 and v2 data coexist. `tx_root` has one format from that activation. No dual format (§10).

| Network | What happens |
| --- | --- |
| Testnets, devnets | Reset by the partition-resize migration. They restart in v2. No v1 data code path is kept |
| Mainnet | Hardfork at the Estuary activation. Nothing to migrate: mainnet stores no data |

**Preconditions.**

- At release freeze, scan mainnet for any block with a data-chunk PoA and any tx whose data was stored. Expect none. If one exists, nodes that sync from genesis need today's verifier for those blocks, gated by height.
- Keep it that way until activation (§4). The fork release keeps the old capacity PoA path bit-exact, because mainnet sync from genesis needs it.
- The no-version-tag header decision depends on the reset happening at this same activation. If v2 misses Estuary, it needs its own fork and another testnet reset.

**Order of work.**

1. The wire-format decisions are closed (spec.md §15 and §10). `tx_root` v2 is part of the Estuary release (Appendix C). It does not change the data-tree vectors. Its vectors are generated when it is implemented (§8). The gateway team has signed off the item envelope (§10 Q7).
2. Generate the shared data-tree vectors (spec.md §13) from one reference implementation and check them into both repos. Each implementation passes them before it merges. The `tx_root` v2 group is not in that set yet.
3. L1 integration (§7) is a separate work item from this crate. Implement L1 and the gateway in parallel. The gateway node simulator emits v2 proofs.
4. Run a joint devnet: the gateway test harness against a node with the partition-resize migration, and the multiversion harness across the activation as a cutover.
5. Release node, gateway services and SDKs together. Reset testnets. Activate on mainnet.

**Persisted data.** A database version bump covers the renamed chunk-key tables and the new `CachedDataRoot` rows, which lose their size-confirmation field. Testnets are reset, so nothing is re-keyed. The submodule databases are moving to RocksDB; the RocksDB branches take the new key names and the note-free proof format.

## 7. Implementation map

This crate is the data tree and spec.md. The rows below are the L1 and gateway integration. That integration is a separate work item (§6).

| L1 area | Change |
| --- | --- |
| `crates/types`: new data-tree module | geometry, tags, `leaf` / `node` / `size_witness`, the `empty` table, `chunk_root`, the streaming builder with its node cache, chunk and segment proofs, verifiers, `chunk_key`; a newtype for each value |
| `crates/types/src/merkle.rs` | after Estuary it no longer builds `tx_root` and it is no longer used for data. It still builds the address-bound half of an ingress proof. The ingress-proof change is the ingress-proofs row |
| `tx_root` | new v2 encoder (Appendix C). Block production and migration compute the new `tx_root`. Block validation checks the partition and rejects a mismatch (Appendix C). Not implemented in this crate |
| `crates/types/src/transaction.rs` | field `data_root` becomes `data_tree_root` at the same position; a `data_commitment()` accessor derives the commitment; `data_chunks()` builds chunks with the v2 builder |
| client builder (`irys.rs` merklize, api-client chunk upload) | v2 tree and chunk proofs; chunk messages carry `data_commitment` |
| `crates/types/src/chunk.rs`, gossip types | `ChunkKey`; the root field of `UnpackedChunk` holds `data_commitment` |
| consensus config | `segments_per_chunk`: a power of two, 1 ≤ SPC ≤ 256, `chunk_size % SPC == 0`, checked at load (spec.md §2). Where the segment-PoA rule runs, also reject SPC < 2 at load (segment-PoA spec §6.3 rule 3, §10 Q9). The tree stays general. The original reason (an empty segment proof) no longer applies since spec.md §15 Q10. Whether segment-PoA needs SPC ≥ 2 for another reason is open. Settle it against the segment-PoA spec during integration. Chunk proofs and segment proofs end with the size witness, so none is empty (spec.md §5, spec.md §15 Q10) |
| chunk ingress | `verify_chunk`, `InvalidChunkSize`, deletion of the v1 size machinery (§4); ingress-proof readiness becomes `!block_set.is_empty()` |
| p2p | `InvalidChunkSize` is a peer fault; chunk gossip is keyed by `ChunkKey` |
| database | chunk-key tables (spec.md §7); rows keyed by `data_commitment`; size-confirmation state deleted; the chunk backfill verifies with `verify_chunk`; database version bump. The MDBX path tables that parse the last 64 bytes as a v1 leaf, and the RocksDB derive, follow `tx_root` v2 (Appendix C) |
| `crates/database/src/tx_path_derive.rs` | builds v2 `tx_path` proofs (Appendix C). One format from Estuary (§10) |
| storage modules | keys by `chunk_key` and `data_commitment`; `c` from the placement. No empty-proof handling is needed: chunk proofs and segment proofs end with the size witness, and that witness is the same for every chunk of one upload (spec.md §5, spec.md §15 Q10). Flags and the format byte sit directly after the proof and before the CRC: `[proof][flags][format][crc]` (spec.md Appendix A.1). Stored proofs stay leaf to root, as received |
| mempool, block validation and discovery | reject every data tx in blocks up to and including E (§4). Block validation checks `1 ≤ data_size_k`, `ceil(data_size_k / chunk_size) ≤ max_data_tx_chunks` (spec.md §2), `chunk_start_0` equal to the parent's `total_chunks` for that ledger (`DataTransactionLedger.total_chunks` in `crates/types/src/block.rs`; 0 when the ledger is absent) and `chunk_start_{k+1} = chunk_start_k + ceil(data_size_k / chunk_size)` with checked arithmetic, and that the header's `tx_root` equals the root of exactly those leaves with `count_witness(N)`, where `N` is the ledger's tx count (Appendix C). A block that fails is invalid. PoA data form per the segment-PoA spec; whole-chunk offset mapping (§4); activation rules (§4). PoA validation verifies the owning tx with `verify_tx` (Appendix C). Which fields a PoA carries is open (Appendix C item 2). `InvalidPromotionDataSizeMismatch` and the `tx_selector` size filter deleted |
| release freeze | scan mainnet for any data tx or data-chunk PoA (§6). Expect none. One found means the §4 refusal does not hold, and nodes that sync from genesis keep today's data verifier for those blocks, gated by height |
| mining, chunk migration | `verify_chunk` against the header commitment |
| ingress proofs | Today `verify_ingress_proof` (`crates/types/src/ingress.rs:224-260`) rebuilds an address-bound tree and a v1 data tree. The address-bound tree hashes each chunk with the miner address (`crates/types/src/merkle.rs:492`, `merkle.rs:714-718`). The signature covers both roots, `chain_id` and the anchor (`ingress.rs:136-144`, `ingress.rs:249-256`). The address-bound tree stays on the v1 encoder until the custody redesign replaces ingress proofs. The data half changes from v1 `data_root` to v2 `data_commitment` from the v2 builder (§10 Q6). The v2 builder has no miner address, so it does not replace that tree. The planned signature preimage fields are `data_commitment`, the address-bound root, `chain_id` and the anchor. The exact layout is finalized in L1 integration (§6). Rows and gossip are keyed by `data_commitment`, so one proof covers one (data, size). A planned range-scoped proof also signs the chunk range `[first, last]` (chunk indices, never byte offsets). A contiguous range could reuse the v2 data tree (subtree roots plus the boundary siblings). That API needs its own section and vectors |
| API | `/internal/chunks` returns the v2 `data_path` with no version field (one format). JSON: the header key `dataRoot` becomes `dataTreeRoot`, chunk messages carry `dataCommitment`, and the chunk-by-root route is keyed by the commitment. Agree the names and the route with the gateway team |
| tests | the shared vectors (spec.md §13), a shape lock for `C` from 1 to 4,097, property tests over random sizes, the negative cases |

| Gateway area | Change |
| --- | --- |
| Item envelope | signs `data_tree_root` and `data_size`. `data_commitment` is derived, as on the L1 header (§2, §10 Q7) |
| Receipt verifier | follows `tx_root` v2. A proven leaf binds `tx_id`, `data_tree_root`, the exact `data_size`, `prefix_hash`, `prefix_size` and `chunk_start`. The count witness binds `N`. Placement follows the §4 rule (spec.md §6, Appendix C) |
| `bundle_items_root` | may adopt the same form as `tx_root` v2, with its own tags. The gateway team decides (Appendix C) |
| Custody framing | multi-segment answers use independent segment proofs that share one chunk proof (spec.md §5). The custody spec defines the framing |

**Gateway, SDKs, simulator.** One gateway library serves the data tree, `tx_root` and the bundle index trees. This crate changes the data tree. The receipt verifier follows `tx_root` v2 (Appendix C). `bundle_items_root` may adopt the same form with its own tags; the gateway team decides. Other changes: receipts, relay, bundler, upload service, SDK, wasm build and node simulator. Receipt header bindings shrink from up to 529,216 B (1 to 2 full chunks with paths) to at most 10,240 B (1 to 2 segments with proofs). The joint L1 and gateway proposal lists every file. Gateway state resets with the testnet reset.

## 8. Test vectors

These groups are not part of the data-tree vectors in spec.md §13. They are not generated yet.

| Group | Cases |
| --- | --- |
| Receipt header binding (gateway) | A 100 B span inside one segment, across two segments of one chunk, and across a chunk boundary. Not generated yet. |
| `tx_root` v2 (Appendix C) | `N` = 1, 2, 3, 100; every `k` for small `N`; the 40/1/64-byte ledger (Appendix C); negatives: wrong `N`, wrong `k`, wrong witness, a flipped bit per sibling, a wrong `tx_id`, a wrong `prefix_hash`, a wrong `prefix_size`, a wrong `chunk_start`, `data_size = 0`, one hash too many or too few. Not generated yet. Generated when `tx_root` v2 is implemented |
| Wire | The tx header with `data_tree_root`; the chunk message JSON; the `/internal/chunks` response; the gateway item envelope (signs `data_tree_root` and `data_size`) and receipt. Not generated yet. |

## 9. Open decisions

Q1 to Q10 are decided (spec.md §15 and §10). These items stay open. They are integration proposals. They are numbered as in Appendix C. Item 3 is closed (§10). They do not change the data-tree wire format or the data-tree vectors.

2. What a PoA carries: `(k, N, chunk_start)` explicitly, so a validator needs only the owning tx header; or derived from the block's tx headers (smaller PoA, more reads). `chunk_start` is the absolute ledger chunk index (§4, Appendix C). `b` is the PoA's absolute ledger chunk, so `c = b − chunk_start` needs no block ledger range. The owning-header option is sufficient. With the block-validation rule (Appendix C), that option is safe, because the `tx_root` it checks against has passed full validation.
4. Where the code lives: `irys-data-tree`, which shares `T`, `climb`, `depth` and the tag registry, or a separate crate.

**Segments per chunk.** The node still rejects SPC < 2 wherever the segment-PoA rule runs (§4). The original reason (an empty segment proof) no longer applies since spec.md §15 Q10. Whether segment-PoA needs SPC ≥ 2 for another reason is open. Settle it against the segment-PoA spec during integration. The tree's rule is unchanged (spec.md §2).

## 10. Planned decisions

This is the planned log. The binding log is spec.md §15. Both logs are append-only.

**Planned — integration (not binding).**

| Date | Decision |
| --- | --- |
| 2026-10-21 | Q4. A `tx_root` leaf value is `fold(data_tree_root, prefix_hash)` and the span note is the exact `data_size`. A proven leaf authenticates both, so a verifier re-derives `node(data_tree_root, size_witness(span))`. The `tx_root` code stays unchanged. Superseded: see Appendix C |
| 2026-10-21 | Q6. The ingress-proof change is §7. The data half is `data_commitment` from the v2 builder, so one proof covers exactly one (data, size). A planned proof of a chunk range binds `data_commitment` and that range for the same reason |
| 2026-10-21 | Q7. The gateway item envelope signs `data_tree_root` and `data_size`, and `data_commitment` is derived, as the L1 header does. This anchors the size and keeps one format for L1, the gateway and SDKs. Owner decision; the gateway team signed off on 2026-10-21 |
| 2026-10-21 | Q8. Every data tx in a block up to and including Estuary's activation block E is refused. Mainnet stores no data and test networks reset, so no pre-activation data tx can exist, and the fork release needs no v1 data code and no anchor rule |
| 2026-10-21 | Q9. The tree's rule is 1 ≤ SPC ≤ 256, a power of two, and it sets no tighter protocol floor. A bound is a property of how the tree is used, not of the tree. The irys node rejects SPC < 2 wherever the segment-PoA rule runs, because an empty segment proof signs like an absent optional RLP field. Reason void since Q10; see 2026-10-22 row |
| 2026-10-21 | The data root is external: clients, SDKs and the gateway commit to it, so it lands first and is hard to change later. `tx_root` is internal to the protocol and can change in any fork |
| 2026-10-21 | `tx_root` v2 ships at Estuary (Appendix C): it commits to its leaf count with a count witness, and its proofs end with that witness. Gateway receipts are meant to be near self-contained paths from `tx_root` to a chunk, so a verifier holding only `tx_root` must get the count bound |
| 2026-10-21 | Q3. `tx_root` v2 (Appendix C) ships at Estuary, in the same hardfork as the data tree. Tags `0x05` to `0x07` are in the spec.md §3 registry. Estuary already requires a data reset: test networks reset, and mainnet stores no data. Q8 refuses data txs up to and including E, so every pre-activation `tx_root` is `0^32` and no data PoA exists. One format. No dual format. A later fork would need both formats, gated by height. Appendix C items 2 to 4 stay open. L1 integration (§7) is a separate work item. Item 3 closed 2026-10-22; items 2 and 4 stay open |
| 2026-10-21 | The `tx_root` v2 leaf binds `data_size` directly (Appendix C). This replaces the Q4 mechanism, where the span note is the exact `data_size`, with the leaf field. The intent is the same: a proven leaf authenticates `data_tree_root` and the exact `data_size`, and a verifier re-derives `node(data_tree_root, size_witness(data_size))`. Formula superseded 2026-10-22, see the size-witness formula row |
| 2026-10-19 | The fork release deletes the old data-PoA verifier and keeps the capacity PoA path bit-exact |
| 2026-10-19 | `tx_path` note padding: if `tx_root` keeps its format, the PoA check rejects non-zero padding from activation |
| 2026-10-19 | One activation for all of it: Estuary's epoch-aligned activation |
| 2026-10-19 | The data tx header keeps its layout; no version tag. This relies on the testnet reset at the same activation |
| 2026-10-18 | No v1 and v2 coexistence: test networks reset, and mainnet stores no data |
| 2026-10-18 | The header stores `data_tree_root` and `data_size`; `data_commitment` is derived, never encoded |
| 2026-10-22 | The `tx_root` v2 leaf keeps `chunk_start` (Appendix C). A tx proof shows membership, the committed `data_size`, and which block chunks the tx occupies. Placement follows the §4 rule. Receipts are near self-contained paths from `tx_root` down to a chunk (block header → `tx_root` → tx → `data_commitment` → chunk), so the leaf lets a receipt show that ledger chunk `b` is tx `k`'s chunk `c` without the block's tx list, and lets PoA verify with the owning tx header. Considered and not chosen: membership and committed size only, as v1 `tx_root` means today, with placement recomputed from the block's tx headers with the padded-size helper. A receipt would then need the tx list, or a separate placement proof, to map a ledger chunk. Superseded by append. |
| 2026-10-22 | The `tx_root` v2 leaf is one preimage, 121 B (Appendix C): `H(0x05 ‖ tx_id ‖ data_tree_root ‖ be64(data_size) ‖ prefix_hash ‖ be64(prefix_size) ‖ be64(chunk_start))`. `prefix_hash` stays. This closes item 3. The gateway receipt verifier checks the receipt's `prefix_bytes` against `prefix_hash` (`crates/shared/gateway-core/src/verify.rs` in the gateway repo). That hash is authenticated only through this leaf. Dropping it would leave the bundle-header binding unanchored. The network does not check `prefix_hash` against the bytes (`crates/types/src/transaction.rs`). The value is signer-asserted, and the leaf fixes it to the tx. `tx_id` is added. It is keccak256 of the signature bytes (`DataTransactionHeader::sign` and `is_signature_valid` in `crates/types/src/transaction.rs`). The signature covers the signed header: `encode_for_signing` omits `id` and `signature`, and the preimage includes `data_root` (spec.md's `data_tree_root`, §3), `data_size`, `prefix_hash`, `prefix_size`, `signer`, `anchor`, `term_fee`, `perm_fee` when it is `Some`, `ledger_id`, `chain_id` and `metadata_format`. One 32-byte field binds that header. A receipt names the exact transaction, and anyone holding the header can link it. Signer, `term_fee`, `perm_fee` when it is `Some`, anchor and `chain_id` need no separate field. `ledger_id` is fixed by which per-ledger `tx_root` the leaf is under. `prefix_size` is added. The prefix bytes hash to `prefix_hash`, which fixes their length, but a consumer that does not hold those bytes needs `prefix_size` to locate the bundle header. It costs 8 bytes of hash input and no proof bytes. Items 2 and 4 stay open |
| 2026-10-22 | Q9's reason is void after Q10, and the node rule is kept. The node still rejects SPC < 2 wherever the segment-PoA rule runs. The original reason (an empty segment proof) no longer applies since Q10. With SPC = 1 and C = 1 the segment proof is `[size_witness(n)]`, 32 bytes, never empty. Whether segment-PoA needs SPC ≥ 2 for another reason is open. Settle it against the segment-PoA spec during integration (§9). The tree's rule is unchanged: 1 ≤ SPC ≤ 256, a power of two |
| 2026-10-22 | `chunk_start` is the absolute ledger chunk index where the tx begins (Appendix C, §4). `chunk_start_0` is the parent's `total_chunks` for that ledger. The field is `DataTransactionLedger.total_chunks` in `crates/types/src/block.rs`. It is signed in the parent header. That count is 0 when the ledger is absent. `chunk_start_{k+1} = chunk_start_k + ceil(data_size_k / chunk_size)`, with checked addition. In §4, `b` is the PoA's absolute ledger chunk. `c = b − chunk_start`, with `b ≥ chunk_start` and checked subtraction, and `c < ceil(data_size / chunk_size)`. Receipts are self-contained paths from `tx_root` to a chunk. An absolute start lets a receipt prove that ledger chunk `b` is tx `k`'s chunk `c` with nothing else. It lets PoA verify with only the owning tx header. The `tx_root` depends on the parent's chunk count. Block validation already checks that count. A byte offset `chunk_start × chunk_size` uses checked multiplication. The product can overflow `u64` at a large `chunk_start`. Considered and not chosen: block-local starts, with `chunk_start_0 = 0`, plus `b = ledger_offset − block_start`. That form needs the block's ledger range from the header. The receipt is then not self-contained. This supersedes the earlier `chunk_start` row that counts from the start of the block. |

## Appendix A.2. Gateway receipt chain (as of 2026-10-21)

A read of the gateway codebase (the `gateway` repository, paths relative to its root) for spec.md §15 Q10 and the `tx_root` design (Appendix C). Findings:

- **The receipt carries no block header.** A receipt is the JSON `DataItemReceipt` (`crates/shared/dto/src/lib.rs`): block height and hash, `txRoot`, `txLedgerIndex`, `txByteOffset`, and base64url proofs. The gateway's node stream parser keeps only a block's height and hash (`crates/shared/chain-stream/src/wire/node.rs`). The header's `tx_root`, `tx_ids` and signature are dropped.
- **The anchor is a `tx_root` rebuilt from every tx in the block.** `verify_data_item_receipt` (`crates/shared/gateway-core/src/verify.rs`) takes a `RelayAttestedAnchor` (`crates/shared/gateway-core/src/anchor.rs`) rebuilt from the relay's quorum population rows. Its comment: "This is **not** a node-header-signed `tx_root`". The leaf count is the row count, so a verifier must fetch the whole tx list to check one item. The tx index in a receipt is checked against that list, not proven.
- **Empty and missing collapse in three containers:** a header binding whose lists are both empty becomes absent; nested inner proofs use `skip_serializing_if = "Vec::is_empty"`; `ChunkWithProof.proof` uses `#[serde(default)]`. These fail closed today only because every v1 proof is at least 64 bytes. A sibling-only one-leaf proof would fall into each of them. This is why spec.md §15 Q10 makes every proof end with its witness.
- **One Merkle library serves four trees** (`crates/shared/bundle-format-v2/src/merkle.rs`): `data_root`, `tx_root`, `bundle_items_root` (leaf: item `data_root` and `data_size`) and `item_headers_root`, all in the v1 note format. `bundle_items_root` has the shape of `tx_root`.
- **What a format change breaks:** receipt proof bytes are not golden-pinned. The receipt JSON shape, the indexer OpenAPI snapshot and two Merkle root hex values are.

Consequences: `tx_root` v2 commits to its leaf count (count witness, Appendix C), so a receipt can be a near self-contained path from an anchored `tx_root` to a chunk; every proof ends with its witness (spec.md §15 Q10). Placement on that path follows the §4 rule. Today's `txByteOffset` is a sum of `data_size` and is not a v2 placement coordinate (Appendix C).

## Appendix C. `tx_root` v2 (designed, ships at Estuary, not implemented yet)

`tx_root` v2 is decided for Estuary, in the same hardfork as the data tree (§10). Nothing in this crate or the node implements it yet. It applies the data tree's rules to the ledger tree: positional leaves, one SHA-256 call per node with domain tags, no notes, a witness that binds the leaf count, and witness-last proofs (spec.md §15 Q10).

**Why it changes.** Today's `tx_root` (`crates/types/src/merkle.rs`, shared with v1 `data_path` and today's ingress proofs):

- Each node carries a 32-byte note, of which 24 bytes are unchecked padding. One `tx_path` has many byte encodings.
- The notes are prefix sums of raw `data_size`, but txs are placed in whole chunks. A PoA target `block_chunk × chunk_size` lands on the wrong byte after any tx that does not end on a chunk boundary.
- It has no domain tags. From Estuary, `tx_root` is tagged, so an untagged `tx_root` is not a cross-tree target. The address-bound half of an ingress proof stays on the v1 encoder (§7). A crafted 63 B or 95 B segment (about 2^8 tries) can hash to a leaf or branch of that tree. No exploit is known: those roots come from a different header field. This caveat is not part of the normative spec.md §10 domain-separation property.
- It does not commit to its leaf count. Gateway receipts are meant to be near self-contained Merkle paths from `tx_root` down to a chunk (block header → `tx_root` → tx → `data_commitment` → chunk), so a verifier that holds only `tx_root` must still get the count bound (Appendix A.2).

**Hashing.** Tags `0x05` TX_LEAF, `0x06` TX_NODE and `0x07` TX_COUNT are in the spec.md §3 registry. They are not part of the data tree. The code enum gains them when this is implemented.

```
N           = number of data txs in this ledger of this block, in tx_ids order k = 0 ..= N−1
chunk_start_0 = the parent's total_chunks for this ledger       // absolute ledger chunk where tx 0 begins (§4)
chunk_start_{k+1} = chunk_start_k + ceil(data_size_k / chunk_size)   // checked addition; every k with k + 1 < N
leaf_k      = H(0x05 ‖ tx_id_k ‖ data_tree_root_k ‖ be64(data_size_k) ‖ prefix_hash_k ‖ be64(prefix_size_k) ‖ be64(chunk_start_k))   // 121 B
tx_root     = H(0x06 ‖ T(leaf_0 ..= leaf_{N−1}) ‖ count_witness(N))   for N ≥ 1     // T() of spec.md §4 with TX_NODE
tx_root     = 0^32                                                     for N = 0     // unchanged sentinel
count_witness(N) = H(0x07 ‖ be32(N))
```

- The leaf is 121 B and has one parse. Every field is fixed width: the tag (1 B), `tx_id`, `data_tree_root` and `prefix_hash` (32 B each), and `data_size`, `prefix_size` and `chunk_start` (8 B each).
- The leaf binds `tx_id`, `data_tree_root`, the exact `data_size`, `prefix_hash`, `prefix_size` and `chunk_start`.
- A proven leaf gives `node(data_tree_root, size_witness(G, data_size))`. That commitment is anchored only when the `tx_root` is itself anchored (spec.md §6, assumption A4). The `data_size` field replaces the span note (§10).
- `chunk_start` is the absolute ledger chunk index where the tx begins. `chunk_start_0` is the parent's `total_chunks` for that ledger: `DataTransactionLedger.total_chunks` in `crates/types/src/block.rs`. The field is on the parent header's `data_ledgers`. The header signature covers it. `IrysBlockHeader::ledger_total_chunks` returns that field, or 0 when the ledger is absent. Then `chunk_start_{k+1} = chunk_start_k + ceil(data_size_k / chunk_size)`, with checked addition. Placement follows the §4 rule.
- Every leaf has `1 ≤ data_size_k` and `ceil(data_size_k / chunk_size) ≤ max_data_tx_chunks`, the spec.md §2 bounds. The builder does not emit a leaf outside those bounds.
- A separate node tag means a ledger-tree node can never be read as a data-tree node.

**Block validation.** The formulas above are how the builder sets `chunk_start`. Block validation must check the same partition, with checked arithmetic, and the spec.md §2 bounds on every leaf. `N` is the ledger's tx count. `verify_tx` is not this check.

```
when N = 0:
    require the header's tx_root = 0^32
when N ≥ 1:
    require 1 ≤ data_size_k                                          // every k with k < N; the spec.md §2 bounds
    require ceil(data_size_k / chunk_size) ≤ max_data_tx_chunks
    require chunk_start_0 = the parent's total_chunks for this ledger
    require chunk_start_{k+1} = chunk_start_k + ceil(data_size_k / chunk_size)
                                                                       // checked addition; every k with k + 1 < N
    require the header's tx_root = H(0x06 ‖ T(leaf_0 ..= leaf_{N−1}) ‖ count_witness(N))
                                                                       // leaf_k from the hashing section, with these starts
```

A block that fails is invalid. A sum that does not fit in 64 bits fails the check. The start equations reject an overlap and a gap. `ceil(data_size_k / chunk_size)` is at least 1, because `data_size_k ≥ 1`, so `chunk_start_{k+1} > chunk_start_k`. `chunk_start` is strictly increasing. Tx `k` owns `ceil(data_size_k / chunk_size)` chunks starting at `chunk_start_k`. The next tx, when there is one, starts where that range ends. The ranges are contiguous and disjoint, and they begin at the parent's `total_chunks`. Each chunk this block places has exactly one owner. A ledger chunk below `chunk_start_0` is not in this block. The owner of chunk `b` is the one tx whose range contains `b`. A lookup by `chunk_start` is unambiguous. The `tx_root` depends on the parent's chunk count. Block validation already checks that count: the header's `total_chunks` for the ledger is checked against the parent count (`crates/actors/src/block_validation.rs`).

These size bounds are already consensus. `data_tx_structural_defect` in `crates/actors/src/data_tx_validation.rs` returns `ZeroDataSize` when `data_size == 0`, and returns `DataSizeExceedsMax` when `ceil(data_size / chunk_size) > max_data_tx_chunks`. Mempool ingress and block prevalidation both call that function, before the block's `tx_root` is checked. Block prevalidation reports `ZeroSizeDataTx` and `DataSizeExceedsMax` (`crates/actors/src/block_validation.rs`). A zero-width leaf shares its start offset with the next tx. PoA owner recovery depends on unique start offsets (`is_poa_owning_leaf` in that file).

**Proofs.** Witness-last, like the data tree: the siblings of `leaf_k` in `T`, leaf to root, then `count_witness(N)`. Length `32 × (depth(k, N) + 1)`. Never empty.

```
verify_tx(tx_root, N, k, tx_id, data_tree_root, data_size, prefix_hash, prefix_size, chunk_start, proof):
    require 1 ≤ N and k < N
    require 1 ≤ data_size                                             // before any hashing; data_size = 0 is rejected
    require len(proof) == 32 × (depth(k, N) + 1)                      // before any hashing
    require last hash of proof == H(0x07 ‖ be32(N))                   // own witness; never climb an unchecked one
    r = climb(H(0x05 ‖ tx_id ‖ data_tree_root ‖ be64(data_size) ‖ prefix_hash ‖ be64(prefix_size) ‖ be64(chunk_start)), k, N, siblings)
                                                                       // climb() of spec.md §5 with TX_NODE; 121 B
    require H(0x06 ‖ r ‖ H(0x07 ‖ be32(N))) == tx_root
```

`verify_tx` rejects `data_size = 0` before it hashes. The spec.md §2 upper bound is the block-validation check above. `verify_tx` does not take `chunk_size`. A block that passed that check has already applied the upper bound. `verify_tx` does not anchor the supplied `tx_root`. For `N = 1` the proof is `[count_witness(1)]`. That proof passes with no signed header. The derived commitment is the unauthenticated case (assumption A4).

**Assumption A4.** A `data_commitment` taken from a `tx_root` v2 leaf is anchored only when that `tx_root` is itself anchored (spec.md §6). The `tx_root` is taken from a signature-checked block header, or from a trust source the consumer names explicitly. Today's gateway names the relay quorum attestation (Appendix A.2). The receipt carries no signed block header. A leaf proven against a `tx_root` the verifier computed itself is the unauthenticated case. A one-leaf tree is that case. A proven `chunk_start` is only as trustworthy as that anchor. `verify_tx` shows that the claimed start is in the supplied tree. It does not show that the start is the absolute index from the parent's `total_chunks`. PoA checks against blocks already validated on chain.

**What a tx proof proves.** Membership of this signed tx at index `k`: `tx_id`, `data_tree_root`, the exact `data_size`, `prefix_hash`, `prefix_size` and `chunk_start`. The count witness binds `N`. The direction bits come from `(k, N)`, so `k` is bound by the proof and is not a separate claim. A receipt names that `tx_id`. It does not need the block's tx list to check the index. Anyone who holds the header links it by `tx_id`. The proof does not show that `chunk_start` is the absolute index from the parent's `total_chunks`. Block validation does. `verify_tx` rejects `data_size = 0` before hashing. The spec.md §2 upper bound is a block-validation check. The proof does not show that the prefix bytes hash to `prefix_hash`. The signer asserts `prefix_hash`, and the leaf fixes that value to this tx.

**Why `chunk_start` is in the leaf.** A receipt is a path from an anchored `tx_root` down to a chunk: block header → `tx_root` → tx → `data_commitment` → chunk. `chunk_start` is the absolute ledger chunk index where the tx begins. Under assumption A4, the proven leaf shows that ledger chunk `b` is tx `k`'s chunk `c` with nothing else: no tx list and no block ledger range. `c = b − chunk_start`, with `b ≥ chunk_start` and checked subtraction, and `c < ceil(data_size / chunk_size)` (§4). PoA verifies with the owning tx header. That header carries the absolute start, and `b` is the PoA's absolute ledger chunk. Membership and committed size alone, which is what v1 `tx_root` proves, was considered and not chosen. A receipt would then need the tx list, or a separate placement proof, to map a ledger chunk. Block-local starts plus `b = ledger_offset − block_start` were also considered and not chosen. That form needs the block's ledger range from the header, and the receipt is not self-contained.

**Why `prefix_hash` stays.** The gateway receipt verifier checks the receipt's `prefix_bytes` against `prefix_hash` (`crates/shared/gateway-core/src/verify.rs` in the gateway repo). That `prefix_hash` is authenticated only through this leaf. Dropping it would leave the bundle-header binding unanchored. The network does not check `prefix_hash` against the bytes (`crates/types/src/transaction.rs`). The value is signer-asserted, and the leaf fixes it to the tx. This closes item 3.

**Why `tx_id` is in the leaf.** `tx_id` is keccak256 of the signature bytes (`DataTransactionHeader::sign` and `is_signature_valid` in `crates/types/src/transaction.rs`). The signature covers the signed header. `encode_for_signing` omits `id` and `signature`. The preimage includes `data_root` (spec.md's `data_tree_root`, §3), `data_size`, `prefix_hash`, `prefix_size`, `signer`, `anchor`, `term_fee`, `perm_fee` when it is `Some`, `ledger_id`, `chain_id` and `metadata_format`. One 32-byte field binds that header. A receipt names the exact transaction. Anyone holding the header can link it. Signer, `term_fee`, `perm_fee` when it is `Some`, anchor and `chain_id` need no separate field. `ledger_id` is fixed by which per-ledger `tx_root` the leaf is under. A second header that copies the other leaf fields and carries another signer or another tx id hashes to a different leaf, so the same proof does not verify.

**Why `prefix_size` is in the leaf.** The prefix bytes hash to `prefix_hash`, which fixes their length. A consumer that does not hold those bytes, such as an indexer, needs `prefix_size` to locate the bundle header. The field is 8 bytes of hash input and adds no proof bytes.

**Receipts.** The tx index `k` is bound by the proof. A receipt must not carry a byte offset as a placement coordinate. Today's `txByteOffset` is a sum of `data_size`. In v2 that field is dropped, or it is `chunk_start × chunk_size` with checked multiplication (§4). The product can overflow `u64`.

**Sizes.** The leaf preimage is 121 B. A proof is still the sibling hashes plus the count witness, so these sizes do not change.

| Data txs in the ledger | Today's `tx_path` | v2 proof |
| --- | --- | --- |
| 1 | 64 B | 32 B |
| 2 | 160 B | 64 B |
| 100 | 736 B | 256 B |

**Activation.** Estuary, with the data tree. One format from that activation. No dual format. Test networks reset, and mainnet stores no data. §10 Q8 refuses every data tx up to and including E, so every pre-activation `tx_root` is `0^32` and no data PoA exists. A v1 `tx_path` verifier is not needed. A later fork would need both formats, gated by height, because `tx_root` and the PoA `tx_path` are signed.

**What changes when it is implemented.** A new encoder. After Estuary, `crates/types/src/merkle.rs` no longer builds `tx_root` and it is no longer used for data. It still builds the address-bound half of an ingress proof. The ingress-proof change is §7. Block production and migration compute the new `tx_root`. Block validation checks the partition, including the spec.md §2 size bounds on every leaf, and requires the header's `tx_root` to equal the root of exactly those leaves. A block that fails is invalid. PoA validation verifies with `verify_tx` against a block that already passed that check. `tx_path` derivation (`crates/database/src/tx_path_derive.rs`) builds v2 proofs. The MDBX path tables that parse the last 64 bytes as a v1 leaf, and the RocksDB derive, follow. The gateway receipt verifier follows. Its `tx_root` is itself anchored: a signature-checked block header, or the relay quorum attestation (assumption A4, Appendix A.2). `bundle_items_root` has the same shape (a root plus a size per leaf) and may adopt the same form with its own tags. The gateway team decides.

**Vectors.** One group (§8). `N` = 1, 2, 3, 100. Every `k` for small `N`. Negatives: wrong `N`, wrong `k`, wrong witness, a flipped bit per sibling, a wrong `tx_id`, a wrong `prefix_hash`, a wrong `prefix_size`, a wrong `chunk_start`, `data_size = 0`, one hash too many or too few. Not generated yet.

One case is the 40/1/64-byte ledger, test geometry (`chunk_size` 32). The parent's `total_chunks` for that ledger is 5, so the starts are absolute. The `data_size`-sum target and the chunk target name different txs. Only the chunk target is right.

| tx `k` | `data_size` | `chunk_start` | ledger chunks | byte start (sum of `data_size`) |
| --- | --- | --- | --- | --- |
| 0 | 40 | 5 | 5 to 6 | 0 |
| 1 | 1 | 7 | 7 | 40 |
| 2 | 64 | 8 | 8 to 9 | 41 |

Ledger chunk 7 is tx 1, chunk 0 (`c = 7 − 7`). The byte target measured from this block's first data byte, `2 × chunk_size`, is 64. Tx 2 covers bytes 41 to 104, so byte 64 is in tx 2. Tx 2's tx proof and its chunk-0 proof both verify. That selection is the wrong tx. The leaf stores `chunk_start_0` = 5, not 0.

**Open items.** Same numbers as §9. Item 3 is closed (§10).

2. What a PoA carries: `(k, N, chunk_start)` explicitly, so a validator needs only the owning tx header; or derived from the block's tx headers (smaller PoA, more reads). `chunk_start` is the absolute ledger chunk index (§4). `b` is the PoA's absolute ledger chunk, so `c = b − chunk_start` needs no block ledger range. The owning-header option is sufficient. With this rule, that option is safe, because the `tx_root` it checks against has passed full validation.
4. Where the code lives: `irys-data-tree`, which shares `T`, `climb`, `depth` and the tag registry, or a separate crate.
