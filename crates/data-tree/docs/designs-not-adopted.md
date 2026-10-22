# Data Tree v2 — designs considered and not adopted

These designs were considered and not adopted. They are not binding and not implemented. They are kept so they can be resurrected. The binding specification is [spec.md](spec.md). The index is [README.md](README.md).

## Appendix B. Compact multi-segment proof (designed, not adopted)

spec.md §15 Q2 chose independent segment proofs that share one chunk proof (spec.md §5). This appendix keeps the design of the compact form so it can be adopted later, as a separate format, if custody answers need the bytes. Nothing in spec.md or this crate implements it, and no vectors exist for it.

**What it saves.** In option B every segment carries all `h` in-chunk siblings, even when another revealed segment's climb already computes that sibling. The compact form sends only the siblings that no revealed segment can compute. Counts below are for `h = 6` (mainnet), checked over every index set:

| Segments revealed in one chunk | Option B in-chunk hashes | Compact in-chunk hashes |
| --- | --- | --- |
| 1 | 6 | 6 |
| 2 | 12 | 5 (adjacent pair) to 10 |
| 4 | 24 | 4 to 16 |
| 64 (whole chunk) | 384 | 0 |

At 4 segments the compact form saves at least 8 hashes (256 B): about 1.4% of an answer that carries 4 × 4 KiB of segment bytes. The chunk proof is the same in both forms and is sent once. It ends with the size witness (spec.md §5).

**Example.** `SPC = 8` (`h = 3`), segments 1 and 6 of one chunk. Option B sends `[L0, A23, B4567]` and `[L7, A45, B0123]`: six hashes. The compact form sends `[L0, L7, A23, A45]`: four hashes. `B0123` and `B4567` are dropped, because the climbs from segments 1 and 6 compute them.

```
                 R_c
          ┌───────┴───────┐
        B0123           B4567          ◄ computed, not sent
       ┌──┴──┐         ┌──┴──┐
      A01   A23       A45   A67        ◄ A23, A45 sent (aux 3, 4)
     ┌┴┐   ┌┴┐       ┌┴┐   ┌┴┐
    L0 L1 L2 L3     L4 L5 L6 L7        ◄ L0, L7 sent (aux 1, 2); L1, L6 revealed
```

**Encoding.** One compact proof covers one chunk. Segments in two chunks are two compact proofs, each with its own chunk proof.

```
compact_proof = be16(m) ‖ j_1 ‖ … ‖ j_m ‖ aux[0] ‖ … ‖ aux[a−1] ‖ chunk_proof
    m          number of revealed segments, 1 ≤ m ≤ SPC
    j_i        one byte each (SPC ≤ 256), strictly increasing, j_i < SPC
    aux        a 32-byte hashes, in the canonical order below; a = aux_count(J)
    chunk_proof  32 × (depth(c, C) + 1) bytes, leaf to root, ending with size_witness(G, n), as in spec.md §5
```

The revealed segment bytes travel next to the proof in index order; each length is `segment_len(n, c, j_i)`, so no lengths are encoded.

**Canonical order and verification.** The verifier walks the chunk subtree level by level, bottom up, and within a level by ascending position. At each position it pairs two known siblings, or takes the next `aux` hash. That walk fixes the one order in which `aux` is written.

```
aux_count(J):                            // from the indices alone; no hashing
    known = J; a = 0
    for level in 0 .. h:
        next = []
        for each p in known, ascending:
            if p is even and p + 1 is in known: consume both
            else: a = a + 1
            append p >> 1 to next (once per pair)
        known = next
    return a                             // known is now [0]

verify_compact(G, D, n, c, proof, segment_bytes):
    parse m, J; require 1 ≤ m ≤ SPC; require J strictly increasing and every j < SPC
    require 1 ≤ n ≤ max; C = ceil(n / chunk_size); require c < C
    require len(segment_bytes[i]) == segment_len(n, c, j_i) for every i in 1 ..= m
    a = aux_count(J)
    require len(proof) == 2 + m + 32 × (a + depth(c, C) + 1)  // exact, before any hashing
    known = [(j_i, leaf(segment_bytes[i])) for i in 1 ..= m]; next_aux = 0
    for level in 0 .. h:
        next = []
        for each (p, x) in known, ascending p:
            if p is even and the next entry is (p + 1, y): parent = node(x, y); consume both
            else if p is even: parent = node(x, aux[next_aux]); next_aux += 1
            else:              parent = node(aux[next_aux], x); next_aux += 1
            append (p >> 1, parent) to next
        known = next
    require next_aux == a and known == [(0, r)]               // every hash used, one root
    require the last hash of chunk_proof == size_witness(G, n)   // before the climb
    r = climb(r, c, C, chunk_proof without its last hash)
    require node(r, size_witness(G, n)) == D
```

**Rules that make it safe.**

- *Canonical.* Strictly increasing indices give one encoding per index set, and the walk gives one `aux` order. For a fixed `(D, n, c, J)` the valid proof is unique up to SHA-256 collisions, as in spec.md §10.
- *Length before hashing.* `aux_count(J)` needs only the indices. The exact proof length is checked before any hash, and `m ≤ SPC` caps the parse.
- *Every hash used.* A proof with a spare or a missing `aux` hash fails the length check. The final `next_aux == a` check is an invariant, not a second length check.
- *Positional binding.* As in spec.md §10, the shape is fixed by `(n, chunk_size, SPC)`. A revealed segment hashes at its exact length, and a segment past the data is `leaf("")` with empty bytes.
- *Same anchor.* The commitment must be anchored (spec.md §6).

**If it is adopted.** Add a `verify_compact` beside `verify_segment` with the same error kinds, an encoder on `DataTree`, a spec.md §5 row, a spec.md §13 vector group (the option B cases, plus `m = SPC`, an index set with `aux_count = 0`, and the negative cases: unsorted or repeated indices, `j ≥ SPC`, `m = 0`, `m > SPC`, one `aux` hash too many or too few, a flipped bit in each `aux` hash), and property tests that the compact proof and the option B proofs agree on every index set of small geometries. A joint proof across several chunks of one upload (a chunk-level multiproof over the RFC 6962 shape, with carried nodes) is a further step and is not designed here.

## Appendix D. Tree salt (designed, not adopted)

Tree salt (Appendix D): design for team discussion — whether to adopt it, the L1 opt-in carrier, the anchor rule, the type name.

**Design only — not adopted, not binding, not implemented.** Recorded for team discussion. Nothing in this crate, the node or the gateway uses it. If adopted, it changes every hash of the data tree, so it must be decided before the data-tree format is released.

**Goal.** An emergency escape from a hash collision when the stored bytes cannot change. Today an uploader whose data hits a collision (accidental, or precomputed by an attacker) has no degree of freedom: the same bytes, size and geometry always give the same tree. The L1 `anchor` makes tx ids unique and blocks replay, but it never enters the data hashes.

**Construction.** A 32-byte salt enters every hash of the tree, not only the top:

```
leaf(bytes)        = H(0x00 ‖ salt ‖ bytes)
node(l, r)         = H(0x01 ‖ salt ‖ l ‖ r)
size_witness(G, n) = H(0x02 ‖ salt ‖ be64(n) ‖ be64(chunk_size) ‖ be16(segments_per_chunk))
empty(k)           = computed with the same salt
salt = 0^32 unless the upload opts in
```

- Salting only the top (the size witness) would change the commitment but leave every leaf and chunk root unchanged, so a collision between two chunks or segments would survive. The salt must be in every hash.
- With a salt the attacker does not choose, the scheme relies on randomized hashing (Halevi–Krawczyk): a collision must hold under a salt fixed after the attacker's precomputation.
- `chunk_key(D, c)` needs no salt: it hashes the commitment, which already depends on the salt.
- Every preimage stays fixed-width fields plus at most one variable field (the segment bytes), so each still has one parse.

**Cost.** None in proof bytes: the salt is one value per upload, known to the verifier, never in a proof. None in SHA-256 work at 32 bytes:

| Hash | Today | Salted | SHA-256 blocks |
| --- | --- | --- | --- |
| node | 65 B | 97 B | 2 → 2 |
| size witness | 19 B | 51 B | 1 → 1 |
| full 4 KiB segment leaf | 4,097 B | 4,129 B | 65 → 65 |

A 1-byte salt would give only 256 re-rolls, which an attacker can enumerate; 32 bytes costs the same.

**Opt-in, so deduplication stays the default.** In nearly every case, identical data should give an identical `data_commitment`, so storage can deduplicate. The salt is therefore `0^32` by default and set only when an upload opts in. Always keeping the salt field in the preimage (zero when unused) gives one hash format instead of two.

**Where the salt comes from: the anchor.** A salted upload uses its signed `anchor` as the salt:

- The anchor is already a signed 32-byte field of every data tx header, so no new 32-byte field is needed.
- It must be recent, so it is not freely chosen, and nobody can precompute collisions against future anchors.
- A different recent anchor gives a fresh salt (a re-roll).

**The opt-in flag (L1).** The L1 data tx header has no flags byte today (fields: `id`, `anchor`, `signer`, `data_root`, `data_size`, `prefix_size`, `prefix_hash`, `term_fee`, `ledger_id`, `chain_id`, `signature`, `metadata_format`, `perm_fee`; `crates/types/src/transaction.rs`). The design adds a signed `flags: u8` to the header at Estuary, bit 0 = salted; the other bits are reserved and must be zero. This reverses the 2026-10-19 decision "the data tx header keeps its layout; no version tag"; that decision relied on the Estuary reset, which this change would share. Overloading `metadata_format` was considered and rejected (it describes the prefix format; mixing meanings in one signed byte invites errors).

**The gateway.** The v2 item envelope already has a `flags: u8` with reserved bits (gateway `crates/shared/bundle-format-v2/src/item/envelope.rs`). A reserved bit can carry "salted" there with no layout change; the item's anchor (present when envelope bit 0 is set) would be its salt.

**Consequences.**

- The crate stays anchor-agnostic: the builder, the verifiers and the hash functions take a 32-byte salt like they take the geometry. "Salt = anchor when flagged" is an L1 and gateway rule.
- A salted upload is tied to its anchor. The client picks the anchor before hashing the data; anchors expire, so if a salted tx is not included in time, resubmitting with a new anchor means rehashing all of the data. Acceptable for a rare emergency; not suitable as a default.
- The L1 anchor may be "a recent transaction id from the signer" as well as a recent block hash. A tx id is `keccak256(signature)`, so a signer can generate many candidate anchors; for a salted upload that partly lets the uploader choose its salt.
- Every vector changes if adopted (the zero salt adds bytes to every preimage), including the roots, so the spec.md §13 root tables would be re-derived from independent references.

**Open questions for the team.**

1. Adopt the salt at all, given that a SHA-256 collision on stored data is not expected?
2. L1 opt-in: a new signed `flags: u8` header field (this design), or another carrier?
3. Must a salted upload use a block-hash anchor (not a signer tx id), so that the uploader cannot choose its salt?
4. Name of the salt type in the crate (proposed: `TreeSalt`, with `TreeSalt::NONE` for the zero salt).
