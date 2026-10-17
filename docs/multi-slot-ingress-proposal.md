# Ingress proofs for transactions spanning multiple slots

**Status:** Proposed design; no consensus implementation. Companion to the [partition-size proposal](partition-size-change-proposal.md).

Each miner generates a **partial ingress proof** for all transaction chunks within a Submit-ledger slot assigned to it. Collect enough partial ingress proofs from distinct assigned miners for every intersected slot, then promote the transaction. Chunks and proofs may arrive independently across multiple epochs; promotion remains atomic for the transaction.

"Partial" refers to the portion of the transaction covered. A proof requires every chunk in that assigned portion; it cannot omit chunks within the range. See the [glossary](99-reference/02-glossary.md).

## Current behavior

`IngressProofV1` commits to a complete transaction data root and a miner-specific ingress root. Generation waits for validated compact ingress leaves for every transaction chunk. This can span local storage modules, but separate miners cannot combine proofs for different portions.

Mainnet config currently requires ten distinct staked signers and zero proofs from assigned miners. `get_assigned_ingress_proofs` classifies a whole proof using the first intersecting assigned slot; it does not establish coverage independently for all slots. Raising the assigned-proof requirement alone does not solve this gap.

## Derive the required ranges

Use the transaction's canonical Submit-ledger placement, not its eventual Publish-ledger placement. For start offset `L`, chunk count `C`, partition size `P`, and each intersected slot `s`, derive a transaction-relative range with an exclusive end:

```text
C = ceil(data_size / chunk_size)
start = max(L, s × P) - L
end   = min(L + C, (s + 1) × P) - L
```

Ranges must cover `[0, C)` exactly. Producers cannot subdivide them arbitrarily. Resolve placement and assignments from the candidate block's parent branch. For the initial V2 design, require Submit inclusion in an ancestor before promotion; this makes placement available before miners sign and excludes same-block Submit/Publish promotion under V2.

For a slot-aligned 5 TB transaction with 14,500,000 chunks per partition:

| Submit slot portion | Transaction chunk range | Actual bytes | Required proofs at N = 10 |
| --- | --- | --- | --- |
| First | `[0, 14,500,000)` | 3.801088 TB | 10 |
| Second | `[14,500,000, 19,073,487)` | 1.198912 TB | 10 |

The final chunk is partial. An unaligned 5 TB transaction can span three slots and require 30 proofs. Each signer only needs the transaction chunks within its assigned slot. Ten proofs per range is the working proposal; the threshold remains a consensus parameter.

## Proof format and generation

Add `IngressProofV2` with a signature over the version, chain ID, data root, confirmed data size, transaction-relative `[start, end)` chunk range, miner-specific ingress root for that range, and freshness anchor.

Keep proofs about content: slot indexes, partition IDs, and transaction IDs need not be signed. The consumer derives which range is required for each transaction and checks assignment separately. Proof reuse for another transaction with the same root, size, and required range is intentional; a changed placement must be evaluated again.

Generate a partial ingress proof only for a slot assigned to the signer, once every validated compact leaf in the required range is present, with exact offsets and byte boundaries. Reuse the existing miner-specific chunk hashes and preserve original transaction-relative byte boundaries when constructing the range tree. Make the Merkle helper explicitly range-aware; do not reset the first chunk's offset to zero or treat the first leaf as covering a missing prefix.

Each chunk must still validate against the original transaction data root. Full verification of a range proof requires its chunks and their original data paths, or an equivalent Merkle multiproof, plus recomputation of the miner-specific range root. A root built from only those chunks is not the whole transaction's data root. This extends the existing signed-ingress-proof model; mainnet currently disables full chunk-based ingress-proof validation, and V2 does not by itself introduce a new succinct proof of possession.

## Coverage and promotion

For every derived range, require:

- Exactly `N` selected partial ingress proofs from distinct eligible staked signers, where `N > 0` is the configured proof requirement resolved for the candidate block.
- Every selected signer assigned to that particular Submit slot in the parent epoch snapshot. Proofs from unassigned signers do not count.
- Matching root, size, exact range, chain ID, valid signature, and valid anchor.

The same signer may prove several ranges if assigned to their slots, but counts once within each. Extra proofs for one range cannot compensate for another range. Insufficient assignments must not lower the configured quorum: an underassigned slot waits for capacity or replacement miners. Requiring all selected signers to be assigned changes mainnet's current zero-assignee-minimum policy and must activate with V2 at the hardfork. Configure `N` alongside the replica count, accounting for how many assigned miners must be available for ingress to complete.

Include all required ranges and their proofs together at promotion. Order them by transaction order, then range start, then signer address. Share one deterministic range/coverage module between selection and validation, and give reward calculation the same validated grouping.

## Multi-epoch completion

Persist completed range roots and compact leaves while the transaction remains eligible. Refresh anchors by re-signing a completed range after checking its leaf coverage, signer eligibility, and slot assignment; other ranges can remain incomplete. Refreshing an anchor must not require rereading the entire transaction.

On reorg, derive the required ranges again. Reuse matching proofs with valid anchors and eligible assigned signers; rebuild changed ranges from available leaves. Evaluate stake and assignment against the promotion parent's state. If an assignment changes during ingress, a former assignee's proof stops counting; the replacement miner must ingest its portion and produce its own proof. Early proofs do not extend Submit retention or establish current disk durability: existing chunk retention, expiry, and storage-module durability rules still apply. Test a slow upload whose first slot expires before the last range completes.

## Encoding, rewards, and implementation

Version the block proof representation to group variable numbers of range proofs by transaction. Replace the assumption that transaction `i` occupies `[i × N, (i + 1) × N)` in a flat list. Keep V1 decoding and validation for historical blocks; activate V2 rules through a hardfork. Under the current 20,971,520-chunk transaction limit, a transaction intersects at most three new-size slots. Validate encoded lengths and block proof budgets with checked arithmetic.

Keep the transaction's existing total ingress reward `R`, calculated with `N`, rather than multiplying it by the number of ranges. For a range containing `B` actual bytes of a `D`-byte transaction, each of its `N` selected proofs receives a share proportional to `R × B / (D × N)`. Use overflow-safe integer arithmetic, floor each share, and distribute remaining units in canonical range/signer order. Padding earns no reward. Aggregate rewards for a signer within a transaction before applying the transaction's reward rolling hash; repeated XOR of the same transaction ID would otherwise cancel it.

Primary changes:

| Area | Change |
| --- | --- |
| `types/ingress.rs`, `types/merkle.rs` | V2 signed ranges and range-aware generation/verification |
| `database/database.rs`, `database/tables.rs` | Exact range leaf scans; proof key includes root, size, range, and signer |
| `actors/chunk_ingress_service/` | Generate, retry, persist, refresh, and gossip each completed range independently |
| `actors/tx_selector/`, `actors/block_validation.rs` | Shared coverage rules, per-range signer deduplication and assignment checks |
| `types/block.rs`, `types/serialization.rs`, P2P/API consumers | Versioned per-transaction grouping, bounded decoding, historical compatibility |
| `actors/shadow_tx_generator.rs`, `types/transaction/fee_distribution.rs` | Consume the same grouping, conserve total rewards, aggregate repeated signers correctly |

Acceptance tests use small partitions: two/three-slot transactions; exact boundaries and partial final chunks; missing middle chunks/ranges; duplicate signers; unassigned signers and signers assigned only to another intersected slot; insufficient assigned replicas without quorum reduction; wrong root/size/range; independent completion across epochs and restart; anchor refresh; changed assignments and reorgs; Submit expiry; V1 historical validation; V2 activation and same-block promotion rejection; malformed grouping; exact reward conservation with one signer proving multiple ranges. Also verify that generation from compact leaves matches verification from the corresponding chunks and original data paths.
