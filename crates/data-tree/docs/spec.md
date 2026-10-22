# Data Tree v2 Spec

2026-10-19 · Jesse

This file is the source of truth for the Data Tree v2 spec. The code in this crate cites its sections as "spec §N". Change the spec here, in the same commit as the code that implements the change.

Data tree v2 replaces the v1 `data_root` tree for uploaded data. It drops offset notes, adds 4 KiB segment leaves inside each 256 KiB chunk, and binds `data_size` into the commitment. This work is committed before L1 integration. The data tree, and this crate, are confirmed. Planning around PoA, `tx_root` v2 and ingress proofs is not binding. Agents and implementers must not treat that planning as settled. §14 lists the open items, and §15 logs the decisions made. §13 names the source of the printed values and of the shared vector files, and it lists what was checked.

When a proposal and the normative text disagree, the normative text wins. Proposals are settled during L1 integration. Ingress-proof text is proposal in every section. It is not normative text.

| Section | Status |
| --- | --- |
| §2–§5, §9, §10, §13 | Normative. Binds the data tree and this crate |
| §6, verify only against an anchored commitment, and why | Normative. Binds the data tree and this crate |
| §7, chunk identity `chunk_key`, and keys by `data_commitment` | Normative. Binds the data tree and this crate |
| §6 consumer anchor table | Proposal — not binding until L1 integration |
| §7 header and chunk-message format | Proposal — not binding until L1 integration |
| §8, PoA mapping and activation rules | Proposal — not binding until L1 integration |
| §11, rollout | Proposal — not binding until L1 integration |
| §12, implementation map | Proposal — not binding until L1 integration |
| §14, open items | Proposal — not binding until L1 integration. Integration proposals |
| Appendix C, `tx_root` v2 | Proposal — not binding until L1 integration |
| Appendix A.2 consequence lines | Proposal — not binding until L1 integration |
| Ingress proofs, in any section | Proposal — not binding until L1 integration |
| Appendix B | Designed, not adopted |
| Appendix A, measurements, other than the A.2 consequence lines | Informative |

§1 is a summary. It adds no rule. A summary row about a header, a chunk message, ingress, or PoA has the status of the section that defines it.

## 1. Summary

Chunks stay 256 KiB for storage and transfer. Segments of 4 KiB exist only inside the tree, for proofs. One tree gives chunk proofs, segment proofs and the size.

```
Example: 917,504 bytes = 3.5 chunks, so C = 4.

Commitment           data_commitment = node(data_tree_root, size_witness)
(binds data + size)         /                                  \
                   data_tree_root                   size_witness = H(0x02 ‖ be64(size))
                   /            \
Chunk tree   node(R0, R1)    node(R2, R3)          same shape as v1;
             /      \         /      \             leaves are chunk roots
            R0      R1       R2      R3
            /\      /\       /\      /\
Segment   64 leaves each (4 KiB)    32 data + 32 empty leaves
subtrees  [########][########][########][####....]     # segment with data
                                                       . empty segment, H(0x00)
Data      chunk 0   chunk 1   chunk 2   chunk 3
          256 KiB   256 KiB   256 KiB   128 KiB

A segment proof climbs all three levels: 6 siblings in its segment subtree,
then its siblings in the chunk tree, then the size witness as the last hash.
```

Read it from the bottom up: each chunk's 64 segments hash into a chunk root; the chunk roots form the chunk tree, whose root is `data_tree_root`; `data_commitment` joins that root with the size witness. Every change, against v1 as it exists today:

| Area | v1 (today) | v2 |
| --- | --- | --- |
| Hash function `H` | SHA-256. Each leaf and node first hashes its parts, then hashes their concatenation (4 SHA-256 calls per node) | SHA-256, one call per leaf or node |
| Leaf | one chunk: `H(H(chunk) ‖ H(note))` | one 4 KiB segment: `H(0x00 ‖ bytes)` |
| Node | `H(H(left) ‖ H(right) ‖ H(note))`, with a 32-byte offset note | `H(0x01 ‖ left ‖ right)`, no note |
| Domain separation | none: a leaf and a node differ only in how many parts they hash | one-byte tags: `0x00` leaf, `0x01` node, `0x02` size witness, `0x03` chunk key (§3) |
| Tree shape | `build_layer`: pair nodes left to right, carry an unpaired node up a level | the same shape at the chunk level; inside each chunk, a perfect subtree of 64 segment leaves, with empty leaves past the data |
| Position in a proof | read from the byte-offset notes | computed from the chunk index `c` and the chunk count `C` |
| Proof contents | at each level, both children and a note (96 B), plus the leaf (64 B) | at each level, only the sibling (32 B), then the size witness (32 B) |
| Proof order | root to leaf | leaf to root: each step is one hash, so the check starts at the leaf and climbs. The size witness is the last hash |
| Chunk proof | 2,464 B at 5 TiB; 64 B for a one-chunk upload | 832 B at 5 TiB; 32 B for a one-chunk upload |
| Segment proof | none | 224 B (one chunk) to 1,024 B (5 TiB) |
| Size | implicit: the note of the rightmost leaf | `size_witness = H(0x02 ‖ be64(data_size))`, the last hash of every chunk proof and every segment proof. The verifier requires it to equal its own witness before the climb. The size check takes `(data_commitment, data_tree_root, data_size)` and no proof bytes |
| Root | `data_root` | `data_tree_root` (over the data) and `data_commitment = node(data_tree_root, size_witness)` |
| Sizes per root | many: txs can name one `data_root` with different sizes | one: a `data_commitment` fixes its size |
| Tx header | `data_root`, `data_size` | `data_tree_root` in the same field, `data_size`; the commitment is derived; no new version tag |
| Chunk message | `data_root`, `data_size` (a claim), offset, `data_path`, bytes | `data_commitment`, `data_size` (checked by the proof), chunk index, proof, bytes |
| Chunk identity key | `H(data_path)` | `chunk_key = H(0x03 ‖ data_commitment ‖ be32(c))` |
| Key for data in caches and storage | `data_root` | `data_commitment` |
| Malleable proof bytes | 24 per node (unchecked note padding) | none |
| Size handling at ingress | provisional sizes, chunk parking on larger claims, rightmost-chunk confirmation, size re-checks from storage | none: the size is final when the header arrives |
| Chunk of the wrong length | dropped silently | rejected as a peer fault |
| Builder memory at 5 TiB | every proof built up front: about 51 GB | chunk roots and internal nodes: about 1.3 GB |
| Hashing cost | baseline | +11% to hash a chunk, +3% to check a chunk proof (§9) |

**Why.**

| Goal | Change that gives it | Effect |
| --- | --- | --- |
| Smaller proof-of-access and custody answers | segment leaves and segment proofs | an answer shrinks from a whole chunk plus its path (about 258 KiB) to segments plus paths (about 5 to 18 KB): less block space and gossip bandwidth |
| One size per root | the size witness in every chunk proof and every segment proof; the commitment covers data and size | deletes provisional sizes, chunk parking, rightmost-chunk confirmation and size re-checks. Two txs can no longer name the same root with different sizes |
| Canonical proofs | proofs of sibling hashes and the size witness, no notes | removes 24 malleable bytes per node; node and gateway verifiers accept exactly the same proofs; proofs compare byte for byte |
| Correct chunk identity | `chunk_key` by position | identical chunks at different offsets no longer collide, as keys made from proof hashes would |
| Shorter chunk proofs | 32 B per level instead of 96 B | a chunk proof at 5 TiB drops from 2,464 B to 832 B |
| Domain separation | one-byte tags | a leaf can never be read as a node or a size witness |
| A standard to test against | the RFC 6962 and RFC 9162 tree, proof and check (§4) | the gateway, SDKs and third parties implement a published algorithm; the shape equals today's `build_layer` |
| Lower builder memory | chunk roots and a node cache instead of every proof | about 51 GB down to about 1.3 GB at 5 TiB |
| Simpler, stricter ingress | the size is final at the header; a wrong length is a peer fault | less cached state, no size races, no silent drops |

The price: about 11% more hashing per chunk and 3% per chunk-proof check (§9), and a hardfork with a testnet reset (§11).

**Related documents.** Two separate proposals build on this tree: the segment-PoA spec (mining that hashes and proves single 4 KiB segments) and the data custody proposal (replica claims and challenges). This spec defines only the tree, its proofs and the data tx format. It names the other documents where a rule depends on them.

## 2. Definitions

`H` is single SHA-256. `‖` is byte concatenation. `be64` and `be32` are big-endian integers. Sizes are in bytes.

| Parameter | Mainnet and testnet | `testing()` | Rule |
| --- | --- | --- | --- |
| `chunk_size` | 262,144 | 32 | unchanged consensus field |
| `segments_per_chunk` (SPC) | 64 | 4 | NEW consensus config field: a power of two, 1 ≤ SPC ≤ 256 in the tree; consumers may require more. `chunk_size ≥ 1`, `chunk_size % SPC == 0`, `max_data_tx_chunks ≥ 1`, and `chunk_size × max_data_tx_chunks < 2^64` (no `u64` overflow), checked at config load |
| `segment_size` | 4,096 | 8 | `chunk_size / SPC` |
| `h` | 6 | 2 | `log2(SPC)`, the height of a chunk subtree |
| `max_data_tx_chunks` | 20,971,520 (`2^24 + 2^22`, 5 TiB) | same | unchanged |

The tree is a general primitive. A bound tighter than the rule above belongs to the consumer, not to the tree. The irys node's consensus config rejects SPC < 2 wherever the segment-PoA rule runs (§8). The original reason (an empty segment proof) no longer applies since §15 Q10. Whether segment-PoA needs SPC ≥ 2 for another reason is open. Settle it against the segment-PoA spec during integration.

| Name | Value |
| --- | --- |
| `data_tree_root` | the root over the data only (`T` below) |
| `size_witness` | `H(0x02 ‖ be64(data_size))` |
| `data_commitment` | `node(data_tree_root, size_witness)`: the identity of a v2 upload |
| `data_root` | the v1 value only. A v2 value is never called `data_root` |

Each value has its own newtype in code, so a function signature rejects the wrong one; a deliberate conversion through raw bytes is a decode, not a proof.

A range `a .. b` is half-open `[a, b)`, a range `a ..= b` includes both ends, and a range `a ..` runs from `a` through the last element.

```
n = data_size, with 1 ≤ n ≤ max_data_tx_chunks × chunk_size
C                    = ceil(n / chunk_size)                  // chunk count, C ≥ 1
chunk_len(n, c)      = min(chunk_size, n − c × chunk_size)   // for c < C: 1 ..= chunk_size
seg_start(c, j)      = c × chunk_size + j × segment_size
segment_len(n, c, j) = 0                                 if seg_start(c, j) ≥ n
                     = min(segment_size, n − seg_start)  otherwise
```

- `c` is the chunk index in the upload (`TxChunkOffset`, `u32`), never a byte offset. `c < C ≤ 20,971,520` fits `u32`.
- `j` is the segment index in the chunk, `0 ≤ j < SPC`.
- Use checked arithmetic. `segment_len` must never subtract past zero: an unsigned `n − seg_start` wraps when the segment starts past the data.

## 3. Hashing

Every hash input starts with a one-byte domain tag. This table is the single registry of tags. `0x05` to `0x07` are assigned to `tx_root` v2 (Appendix C). Any later tag set must avoid `0x00` to `0x07`.

| Tag | Name | Preimage | Use |
| --- | --- | --- | --- |
| `0x00` | LEAF | `0x00 ‖ segment bytes` (0 to `segment_size` bytes, exact length) | segment leaf |
| `0x01` | NODE | `0x01 ‖ left ‖ right` (65 B) | every internal node, chunk roots and the root |
| `0x02` | SIZE | `0x02 ‖ be64(data_size)` (9 B) | size witness |
| `0x03` | CHUNK_KEY | `0x03 ‖ data_commitment ‖ be32(c)` (37 B) | chunk identity key; not part of the tree |
| `0x04` | TICKET | defined by the segment-PoA spec | mining ticket; not part of the tree |
| `0x05` | TX_LEAF | `0x05 ‖ tx_id ‖ data_tree_root ‖ be64(data_size) ‖ prefix_hash ‖ be64(prefix_size) ‖ be64(chunk_start)` (121 B) | `tx_root` v2 (Appendix C); not part of the data tree |
| `0x06` | TX_NODE | `0x06 ‖ left ‖ right` (65 B) | `tx_root` v2 (Appendix C); not part of the data tree |
| `0x07` | TX_COUNT | `0x07 ‖ be32(N)` (5 B) | `tx_root` v2 (Appendix C); not part of the data tree |

TX_LEAF, TX_NODE and TX_COUNT are not used by this tree. The TX_LEAF fields are fixed (Appendix C).

```
leaf(bytes)     = H(0x00 ‖ bytes)
node(l, r)      = H(0x01 ‖ l ‖ r)
size_witness(n) = H(0x02 ‖ be64(n))
empty(0)        = leaf("") = H(0x00)
empty(k)        = node(empty(k−1), empty(k−1))      // empty perfect subtree of height k
chunk_key(D, c) = H(0x03 ‖ D ‖ be32(c))              // D = data_commitment, c = chunk index
```

- Every preimage is fixed-width fields followed by at most one variable-length field, so no preimage can be read two ways.
- A segment is hashed at its exact length. No padding is ever hashed: `n` bytes and `n` bytes plus zeros give different leaves.
- A leaf does not include its index. The fixed shape gives the position (§10).

## 4. Tree

**Chunk subtree.** Every chunk, the last one included, is a perfect binary tree of SPC leaves and height `h`. Leaf `j` is `leaf(segment j)` at its exact length. A segment past the data is `leaf("") = empty(0)`. The chunk root `R_c` depends only on the chunk bytes.

**Chunk level.** The `C` chunk roots combine into one root with the same shape as today's v1 builder (`build_layer`): pair nodes left to right, and carry an unpaired last node up one level unchanged. Only the node hash changes, to `node()`. The chunk roots are already hashes, so this level does not hash them again. Written recursively, the shape is:

```
T(R[0..C]):
    if C == 1: return R[0]
    k = largest power of two < C
    return node(T(R[0..k]), T(R[k..C]))

data_tree_root  = T(R_0 ..= R_{C−1})
data_commitment = node(data_tree_root, size_witness(data_size))
```

**Why this spec cites RFC 6962 and RFC 9162.** These two IETF documents specify the Merkle trees of Certificate Transparency, the public logs of TLS certificates. RFC 6962 (2013) is the original; RFC 9162 (2021) replaces it. Their tree has exactly the shape of v1's `build_layer` and of `T` above (checked for every `C` from 1 to 5,000). They also define the two algorithms this spec reuses at the chunk level:

- the **audit path**: how to build the proof for one leaf (RFC 6962 §2.1.1; `path()` below);
- the **inclusion check**: how to verify that proof from only the leaf's index and the leaf count (RFC 9162 §2.1.3.2; `climb()` in §5).

Reusing them gives the gateway, SDKs and third parties a public, widely implemented reference to test against. Their leaf and node prefixes, `0x00` and `0x01`, are also the same as the tags in §3.

One difference matters for anyone who takes code from an RFC library: an RFC tree hashes every leaf as `H(0x00 ‖ leaf)`. Here the chunk-level leaves are chunk roots, which are already hashes, and they are not hashed again. Skip the library's leaf hash and use `R_c` itself as the leaf value.

```
data_commitment for 3.5 chunks (917,504 bytes): data tree left, size witness right

                        data_commitment
                       /               \
              data_tree_root          size_witness(917504)
              /            \
       node(R0, R1)     node(R2, R3)
        /      \         /      \
      R0       R1      R2       R3 = node(A, empty(5))
                                      /          \
                     A: tree over segments 0..=31  empty(5): segments 32..=63,
                        (data)                     all leaf("") = H(0x00)
```

**Example: 3.5 chunks.** `data_size = 917,504`, so `C = 4`. Chunks 0 to 2 are full. Chunk 3 holds segments 0 to 31; segments 32 to 63 are empty, so `R_3 = node(A, empty(5))`, where `A` is the tree over segments 0 to 31. Then `T = node(node(R_0, R_1), node(R_2, R_3))` and `data_commitment = node(T, size_witness(917504))`.

**Non-power-of-two.** With `C = 5`, `T = node(node(node(R_0, R_1), node(R_2, R_3)), R_4)`. The chunk proof for `c = 4` has one sibling, then the size witness. The chunk proof for `c = 0` has three siblings, then the size witness.

**Building.** A streaming builder keeps a stack of at most `log2(C) + 1` subtree roots, plus the chunk roots. To serve proofs it caches every layer. Layer `l` stores `ceil(C / 2^l)` hashes, from the chunk roots (`l = 0`) through the single root. The stored count is `Σ_l ceil(C / 2^l)`, at most about `2C + log2(C)` hashes: 64 B per chunk in all (256 KiB per GiB, about 1.3 GB at 5 TiB). A carried node is copied into each layer it passes through, so a proof lookup is a plain index in that layer. `path()` below is the sibling list. The chunk proof is that list and then `size_witness(n)`. An implementation reads the siblings from the node cache and does not recompute subtrees:

```
path(c, R[0..C]):                         // siblings of chunk c, leaf to root (RFC 6962 audit path)
    if C == 1: return []
    k = largest power of two < C
    if c < k: return path(c, R[0..k]) ++ [T(R[k..C])]
    else:     return path(c − k, R[k..C]) ++ [T(R[0..k])]
```

- The chunk proof is `path(c) ++ [size_witness(n)]`.
- A segment proof is the `h` in-chunk siblings, built from the chunk bytes, then the chunk proof.
- `path` is empty when `C = 1`. The chunk proof is not: it is the size witness. The sibling count is `depth(c, C)` (§5). Do not read the proof length from the streaming builder's stack: after the final merges, a stack height is not the height of the subtree it holds.

## 5. Proofs and verification

A chunk proof or a segment proof is concatenated 32-byte hashes: the sibling hashes, leaf to root, then `size_witness(n)`. No header, no leaf hash, no notes. The order is leaf to root, so the witness is the last hash. Leaf to root is the default order for every proof in the protocol, including data-tree proofs and `tx_root` v2 proofs (Appendix C). Storage keeps a proof in the order it arrived. Why the order changes from v1: a v1 proof holds both children of every node plus an offset note, so a verifier can check it from the root down. A v2 proof holds only the sibling at each level, so the check must start at the leaf and climb, because a parent's hash cannot be computed before the child's. Leaf to root stores the hashes in the order the check uses them; root to leaf would store the same hashes reversed, and every verifier would read the list backwards. The verifier computes the exact length from `(data_size, c[, j])` and rejects any other length. It then computes `size_witness(n)` from that `data_size` and requires the last hash to equal it. Only then does it climb the siblings. A supplied witness that was not checked must not be climbed: a false `data_size` would pass with the true witness attached.

```
depth(c, C):                              // chunk-level siblings in the proof of chunk c
    d = 0
    while C > 1:
        k = largest power of two < C
        if c < k: return d + 1 + log2(k)
        c = c − k; C = C − k; d = d + 1
    return d
```

`depth` is defined for `c < C`, and an implementation rejects any other input.

| `C` | `depth(c, C)` |
| --- | --- |
| 1 | 0 (the chunk proof is the size witness alone) |
| 3 | `c` = 0, 1: 2; `c` = 2: 1 |
| 5 | `c` = 0 to 3: 3; `c` = 4: 1 |
| 4,096 (1 GiB) | 12 |
| 20,971,520 (5 TiB) | `c < 2^24`: 25; `c ≥ 2^24`: 23 |

| Proof | Content | Hashes |
| --- | --- | --- |
| Chunk (`data_path`) | chunk-level siblings, then `size_witness(n)` | `depth(c, C) + 1`; the witness alone when `C = 1` |
| Segment | `h` in-chunk siblings (level 0 first), then the chunk proof | `h + depth(c, C) + 1` |
| Multi-segment (`m` segments of one chunk) | `m` independent segment proofs that share one chunk proof. Each segment carries its `h` in-chunk siblings. The shared chunk proof ends with `size_witness(n)`, and for `C = 1` it is that witness alone. A segment's full proof is its in-chunk siblings concatenated with the chunk proof. That concatenation is the full proof in every case, including one chunk | `m × h + depth(c, C) + 1` |
| Size check | `(data_commitment, data_tree_root, data_size)`; no proof bytes | none |

```
max = max_data_tx_chunks × chunk_size
A proof is a byte string of 32 × k bytes; proof[i] is its i-th 32-byte hash.

verify_chunk(D, n, c, bytes, proof):      // D: an anchored data_commitment (§6)
    require 1 ≤ n ≤ max; C = ceil(n / chunk_size); require c < C
    require len(bytes) == chunk_len(n, c)
    require len(proof) == 32 × (depth(c, C) + 1)
    w = size_witness(n)                    // computed here, from this n
    require the last hash of proof == w   // else stop; do not climb
    r = climb(chunk_root(bytes), c, C, proof without its last hash)
    require node(r, w) == D

verify_segment(D, n, c, j, bytes, proof):
    require 1 ≤ n ≤ max; C = ceil(n / chunk_size); require c < C and j < SPC
    require len(bytes) == segment_len(n, c, j)
    require len(proof) == 32 × (h + depth(c, C) + 1)
    w = size_witness(n)                    // computed here, from this n
    require the last hash of proof == w   // else stop; do not climb
    siblings = proof without its last hash
    s = ceil(chunk_len(n, c) / segment_size)   // a segment at or past s is empty
    r = leaf(bytes); i = j
    for level in 0 .. h:                  // bit `level` of j picks the side
        start = ((j >> level) ^ 1) << level    // first segment of the sibling subtree
        if start ≥ s: require siblings[level] == empty(level)
        r = node(r, siblings[level]) if i is even else node(siblings[level], r)
        i = i >> 1
    r = climb(r, c, C, siblings[h ..])
    require node(r, w) == D

climb(r, c, C, hashes):                   // RFC 9162 §2.1.3.2 inclusion walk, node() as the hash
    fn = c; sn = C − 1
    for p in hashes:
        require sn != 0                   // proof too long
        if fn is odd or fn == sn:
            r = node(p, r)
            while fn is even and fn != 0: fn = fn >> 1; sn = sn >> 1
        else:
            r = node(r, p)
        fn = fn >> 1; sn = sn >> 1
    require sn == 0                       // proof too short
    return r
```

`climb` is the RFC 9162 inclusion check (§4). From the index `c` and the count `C` alone it knows, at each level, whether the sibling is on the left or the right, and where an unpaired node was carried up a level. That is why a v2 proof needs no offset notes. v1 stores byte offsets in every node to find the way; v2 computes the way from `(c, C)`.

- **Size check.** The inputs are `(data_commitment, data_tree_root, data_size)`. There are no proof bytes. The verifier computes `size_witness(data_size)`. `verify_size` takes any `u64` `data_size` as input and does not range-check it. The check is `node(data_tree_root, size_witness(data_size)) == data_commitment`. It fails the comparison unless `data_size` is the committed size.
- **Packed bytes:** unpack, then trim to `chunk_len` or `segment_len` before hashing. The segment-PoA spec also requires the trimmed tail to unpack to zero.
- **Empty in-chunk siblings.** At level `ℓ` the sibling subtree starts at `start = ((j >> ℓ) ^ 1) << ℓ`. Let `s = ceil(chunk_len(n, c) / segment_size)`. A segment at or past `s` is empty (`segment_len` is 0). If `start ≥ s`, that subtree is entirely past the data, and the sibling must be `empty(ℓ)`. `(n, c, j)` fixes `start` and `s` before the climb. An honest proof already carries `empty(ℓ)`. Any other hash fails, the same way a bad climb fails. A last chunk that puts some other hash where an empty subtree must be then fails every data segment of that chunk. No bytes of that chunk pass `verify_chunk` either.
- **No chunk or segment proof is empty.** Each ends with its size witness. For `C = 1` the chunk proof is `[size_witness(n)]`. A missing proof and a present proof differ in length, so a container does not have to tell an empty value from an absent one (§15 Q10). A last hash that is not `size_witness(n)` fails (`SizeWitnessMismatch`) before the climb.
- **Compact multiproof:** a possible future format, not the multi-segment encoding above. Its design is in Appendix B.

| Upload | `C` | Max depth | v1 chunk path | Chunk proof | Segment proof | 4 segments of one chunk (option B / compact form) |
| --- | --- | --- | --- | --- | --- | --- |
| ≤ 1 chunk | 1 | 0 | 64 | **32** | 224 | 800 / 544 |
| 1 GiB | 4,096 | 12 | 1,216 | **416** | 608 | 1,184 / 928 |
| 5 TiB (max) | 20,971,520 | 25 | 2,464 | **832** (768 for `c ≥ 2^24`) | 1,024 | 1,600 / 1,344 |

All sizes are in bytes and exclude the segment and chunk bytes. The last column is option B (`4 × h` in-chunk siblings plus one chunk proof), then a compact multiproof at most. The compact form is a possible future format, designed in Appendix B.

## 6. Trust anchor rule

**Normative.**

Verify chunks and segments only against an **anchored** `data_commitment`: one derived from a header whose signature was checked, or from a `tx_root` v2 leaf that was proven. A commitment that a verifier derives from an unauthenticated `(data_tree_root, data_size)` pair binds nothing. The witness is not a secret. The attacker chooses the claimed size and the claimed chunk index, and writes `size_witness` of that size as the proof's last hash. The claimed size may be smaller or larger than the size the tree was built for. The claimed index may differ from the honest index.

An honest proof for `(c, C)` passes as `(c', C')` against the re-derived commitment when, and only when, two checks hold. The left/right step sequence of `(c', C')` equals that of `(c, C)` at every level. `chunk_len(n', c')` equals `chunk_len(n, c)`. A step is the side `path` and `climb` give the sibling at that level, left or right (§4, §5). The bytes that pass are always some honest chunk of that tree. They are never bytes from outside it. `node(data_tree_root, size_witness(claimed n))` equals the re-derived commitment, so the proof passes against it and fails against the anchored commitment. A proof that still carries the true witness fails earlier, before the climb: the last hash is not `size_witness` of the claimed size.

| Honest proof | Passes against the re-derived commitment as |
| --- | --- |
| `n = 96` (`C = 3`), chunk 2 | chunk 1 of `n' = 64` (`C' = 2`) |
| `n = 128` (`C = 4`), chunk 0, last hash `size_witness(96)` | chunk 0 of `C' = 3` |

Test geometry, full chunks. Both proofs fail against the anchored commitment.

**Proposal — not binding.** Planned for L1 integration; may change.

| Consumer | Anchor |
| --- | --- |
| L1 chunk ingress, PoA, mining, migration | the signed tx header |
| Gateway receipt verifier | a proven `tx_root` v2 leaf. The leaf binds `tx_id`, `data_tree_root`, the exact `data_size`, `prefix_hash`, `prefix_size` and `chunk_start`, so the anchor is `node(data_tree_root, size_witness(data_size))`. The count witness binds `N` (Appendix C) |
| Gateway staging ingest | the item envelope, which signs `data_tree_root` and `data_size`. `data_commitment` is derived (§15 Q7) |
| Gateway relay | its payload-quorum attestation of the tx |
| Gateway data service | tx records from the Indexer are not authenticated today. They need the signed header or a `tx_root` proof before the service deletes its size checks |

## 7. Transaction header, chunk message and chunk identity

**Proposal — not binding.** Planned for L1 integration; may change.

The data tx header keeps its layout and gets no new version tag (decided). From activation, the 32-byte field that holds `data_root` today holds `data_tree_root`, and code names it `data_tree_root`.

| Header field | Size | Signed | Note |
| --- | --- | --- | --- |
| `data_tree_root` | 32 B | yes | same position as today's `data_root` |
| `data_size` | 8 B | yes | unchanged |
| `data_commitment` | — | — | derived on decode (one hash); never encoded |

- Both inputs are signed and the commitment is derived from them, so the size is bound to the commitment. Only chunk proofs show that the data matches it.
- Some chunks of a tx whose `data_tree_root` was built for another size can verify under the signed size. A chunk can verify at its own index or at another index, including the last. Its chunks never all verify, so the tx never completes and expires like any unsupplied upload. Never treat a last-chunk accept, or any single chunk accept, as a size or completion signal. Completion means every chunk verified.
- From Estuary, `tx_root` is v2 (Appendix C). The leaf is `H(0x05 ‖ tx_id ‖ data_tree_root ‖ be64(data_size) ‖ prefix_hash ‖ be64(prefix_size) ‖ be64(chunk_start))` (121 B). A proven leaf binds those fields. A verifier re-derives `node(data_tree_root, size_witness(data_size))`.

| Chunk message field (gossip, `POST /v1/chunk`, gateway batches) | Size | Note |
| --- | --- | --- |
| `data_commitment` | 32 B | the anchor; the receiver looks it up (L1: from tx headers) |
| `data_size` | 8 B | a separate field; the proof's last hash is `size_witness(data_size)`, checked before the climb |
| `tx_offset` | 4 B | the chunk index `c` |
| `data_path` | `32 × (depth(c, C) + 1)` | chunk-level siblings, then the size witness; 32 B when `C = 1` |
| `bytes` | `chunk_len(data_size, c)` | any other length is invalid before hashing |

**Normative.**

**Chunk identity.** A validated chunk is identified by `chunk_key(data_commitment, c)`, not by `H(data_path)`. Identical chunks at different indices of one upload (all-zero data is common) can have identical proofs, so a proof cannot be an identity. Table and type names (decided): `ChunkPathHash` → `ChunkKey`, `ChunkDataPathByPathHash` → `ChunkDataPathByChunkKey`, `ChunkPathHashesByOffset` → `ChunkKeysByOffset`, field `data_path_hash` → `chunk_key`, and `GossipCacheKey::Chunk` keyed by `ChunkKey`.

**Keys.** Everything keyed by data uses `data_commitment`, never `data_tree_root`: `CachedDataRoot` rows, data-root records in storage modules, parked and pending chunks, ingress-proof rows, the root field of `UnpackedChunk`, and the chunk-by-root API route. Two txs with the same `data_tree_root` and different sizes then never share a row, so the multi-size state that v2 deletes cannot come back.

## 8. Consensus rules on L1

**Proposal — not binding.** Planned for L1 integration; may change.

**Chunk ingress.** Verify every chunk with `verify_chunk` against the commitment derived from the tx header.

- A chunk whose length is not `chunk_len(data_size, c)` is `InvalidChunkSize`: a peer fault. Today such a chunk is dropped silently.
- A chunk message whose `data_size` differs from the header is invalid.
- Deleted for v2: provisional size raises; parking on a larger size claim and its rescue; the `layout_data_size` fallback; the rightmost-chunk position check; confirmation on the rightmost chunk; size re-checks from storage modules; the consensus rule `InvalidPromotionDataSizeMismatch` and the `tx_selector` size filter (both read node-local size state); the `data_size_confirmed` field and `confirm_data_size_for_data_root`. Every v2 size is final when the header arrives.
- Kept: parking chunks that arrive before their header. Parking handles arrival order, not size. Once the header arrives, a parked chunk is verified in full.

**PoA offset mapping.** A data PoA names a partition and an offset, which give a block-relative chunk index `b`. Txs fill a ledger in whole chunks, in order. Chunk placement comes only from padded chunk counts (`chunk_start`), never from sums of `data_size`. A byte offset, where one is needed, is `chunk_start × chunk_size`. The `tx_root` v2 leaf binds `chunk_start`, the padded chunk count before the tx. Block validation checks the §2 bounds on every `data_size` and checks that those counts are the prefix sums (Appendix C). That count is the block-relative chunk index where the tx starts:

```
require b ≥ chunk_start                                          // before the subtraction
c = b − chunk_start                                              // checked arithmetic
require c < ceil(data_size / chunk_size)
```

A check that fails makes the PoA invalid. Checked subtraction rejects a `chunk_start` above `b`. An unsigned wrap of `b − chunk_start` is not a chunk index.

The owner of chunk `b` is the one tx whose range contains `b`: `chunk_start ≤ b < chunk_start + ceil(data_size / chunk_size)`. Every ledger tx has `1 ≤ data_size` and `ceil(data_size / chunk_size) ≤ max_data_tx_chunks` (Appendix C, the §2 bounds). Each tx covers at least one chunk, so `chunk_start` is strictly increasing. Each block chunk has exactly one owner. A lookup by `chunk_start` is unambiguous. A zero-size data tx is rejected. `data_tx_structural_defect` in `crates/actors/src/data_tx_validation.rs` returns `ZeroDataSize` when `data_size == 0`. Block prevalidation maps that defect to `ZeroSizeDataTx` (`crates/actors/src/block_validation.rs`). A zero-width leaf shares its start offset with the next tx, so those start offsets are no longer unique (`is_poa_owning_leaf`).

- Each tx starts on a chunk boundary, so segment `j` of the packed chunk at `b` is segment `j` of the tx's chunk `c`.
- The PoA verifies the owning tx with `verify_tx` (Appendix C). Which fields the PoA carries is open (Appendix C item 2).
- The whole-chunk offset mapping lands with v2. Today's mapping uses raw-size byte ranges and drifts after any tx that does not end on a chunk boundary.
- The PoA rule itself (which segment, the ticket, the forms) is in the segment-PoA spec.

**Segments per chunk.** The tree allows 1 ≤ SPC ≤ 256 (§2). Wherever the segment-PoA rule runs, the irys node's consensus config rejects SPC < 2 at load time (segment-PoA spec §6.3 rule 3, §15 Q9). That bound is on this use of the tree. The tree stays general. The original reason (an empty segment proof) no longer applies since §15 Q10. Whether segment-PoA needs SPC ≥ 2 for another reason is open. Settle it against the segment-PoA spec during integration. A segment proof is never empty: when `h = 0` and `C = 1` it is the size witness (§5, §15 Q10).

**Other verifiers.** Mining (winning chunks), chunk migration and storage-module serving use `verify_chunk` or the v2 builder against the header commitment. Ingress-proof checks follow §12. A storage module derives `c` from the placement (partition offset minus the tx's start), never from proof bytes.

**Activation.** Let E be Estuary's activation block: the first epoch block whose timestamp is at or after the activation time. The v2 rules apply from the first child of E; E itself uses the old rules.

- **Data txs up to activation.** No block up to and including E carries a data tx. Mainnet refuses data txs in those blocks, and test networks restart from a reset. The header's root field changes meaning at E and has no version tag. Refusing these txs means none can carry the v1 meaning into v2, so the fork release needs no v1 data code and no anchor rule. The release-freeze mainnet scan (§11) is the precondition.

## 9. Cost

v2 adds about 11% to hashing a chunk (tx creation) and about 3% to verifying a chunk proof. This answers the review question "how much time and compute does it add to tx creation and chunk verification?". Both formats spend almost all their time hashing the chunk bytes once; v2 hashes them as 64 segments of 4 KiB instead of one 256 KiB input.

Rates measured with openssl speed -evp sha256 -bytes N on one core of an Intel i9-13900HK with SHA-NI: 1.82 GB/s on 256 KiB inputs, 1.75 GB/s on 4,097-byte inputs, about 0.15 µs per 65-byte node. Every row below is computed from these rates.

| Operation | v1 | v2 | Change |
| --- | --- | --- | --- |
| Hash one chunk (build or verify) | 0.144 ms: one 256 KiB hash | 0.160 ms: 64 leaves + 63 nodes | +11% |
| Verify one chunk proof at 5 TiB (depth 25) | 0.159 ms | 0.164 ms | +3% |
| Build the tree of a 1 GiB upload, one core | 0.59 s | 0.65 s | +0.06 s |
| Verify one segment proof at 5 TiB | — | about 7 µs | new |
| Builder memory, 1 GiB | every proof up front: 5 MB | chunk roots and nodes: 256 KiB | lower |
| Builder memory, 5 TiB | every proof up front: about 51 GB | chunk roots and nodes: about 1.3 GB, proofs on demand | lower |

- Chunk roots are independent, so tree building parallelizes per chunk, as v1's leaf hashes do.
- The empty-segment hashes of a last chunk come from a fixed table of `h + 1` values.
- Other tx-creation steps (signing, upload) do not change.

## 10. Security properties

| Property | How it holds |
| --- | --- |
| Positional binding | The shape is a function of `(data_size, chunk_size, SPC)`. The verifier derives every direction bit from `(c, j)` and requires the exact proof length. Known attacks on RFC 6962-style proofs (an inner node shown as a leaf, or a proof checked against another leaf count) need a verifier that does not know the leaf count; here `C` is fixed before any hashing. A proof for position `i` shown as position `k` fails unless the bytes at both positions are identical, and then the claim is true |
| Size binding | The verifier computes `size_witness(data_size)` itself and requires the proof's last hash to equal it, before the climb (§5). It then checks `node(root, size_witness(data_size))` against an anchored commitment (§6). A wrong size fails. A proof that carries the true witness fails the witness check even when a re-derived commitment would match that witness |
| Domain separation | Distinct tags and fixed widths (§3). A 64-byte segment equal to `l ‖ r` hashes as a leaf, never as `node(l, r)`. A v2 `data_commitment` and a v1 root can collide only by a SHA-256 collision. A bare `data_tree_root` can equal a v1 leaf or branch id only for a one-leaf tree (SPC = 1, C = 1) of 63 or 95 bytes, found by about 2^8 tries. Key by the commitment, never the bare root (§7). From Estuary, `tx_root` is tagged (Appendix C), so an untagged `tx_root` is not a cross-tree target. The address-bound half of an ingress proof stays on the v1 encoder (§12). A crafted 63 B or 95 B segment (about 2^8 tries) can hash to a leaf or branch of that tree. No exploit is known: those roots come from a different header field |
| One hash per node | v1 hashes each part of a node first, then the concatenation (4 SHA-256 calls per node). v2 hashes once: H(0x01 ‖ l ‖ r), the RFC 6962 construction. The inner hashes add no security: l and r are already 32-byte hashes, and forging any proof still needs a SHA-256 collision at the first node where the honest and forged inputs differ (about 2^128 work). Fixed widths make 0x01 ‖ l ‖ r readable only one way, and the tags replace v1's leaf and node separation by part count. Length extension does not apply: the tree has no secrets, and every node input is fixed-width |
| No malleability | A proof is the sibling hashes and then the size witness. The witness must equal the verifier's own `size_witness(data_size)`. For a fixed `(data_commitment, data_size, c[, j])` the valid proof is unique up to SHA-256 collisions |
| Padding | Segments hash at their true length. `n` bytes and `n` bytes plus zeros differ in a leaf and in the size witness. Packed storage pads with entropy; verifiers trim before hashing |
| Identical data | Identity is `chunk_key`, never proof bytes (§7) |
| Denial of service | Length checks run before any hashing. Work per proof is bounded. A wrong-length chunk is a peer fault, not a silent drop |
| Determinism | Integers and bytes only, checked arithmetic, no map iteration order |
| Availability | Not proven by any root. A dishonest builder can commit to a tree with no preimage for some chunks. Storage proofs (PoA, ingress proofs, custody) detect that, and the upload never completes. A segment proof pins every in-chunk sibling that lies entirely past the data to `empty(ℓ)` (§5), so this holds for segment proofs too: a malformed last chunk fails them |

## 11. Rollout

**Proposal — not binding.** Planned for L1 integration; may change.

v2 ships at Estuary's activation, together with the partition resize, the segment-PoA rule and `tx_root` v2 (Appendix C). There is one switch and no period where v1 and v2 data coexist. `tx_root` has one format from that activation. No dual format (§15).

| Network | What happens |
| --- | --- |
| Testnets, devnets | Reset by the partition-resize migration. They restart in v2. No v1 data code path is kept |
| Mainnet | Hardfork at the Estuary activation. Nothing to migrate: mainnet stores no data |

**Preconditions.**

- At release freeze, scan mainnet for any block with a data-chunk PoA and any tx whose data was stored. Expect none. If one exists, nodes that sync from genesis need today's verifier for those blocks, gated by height.
- Keep it that way until activation (§8). The fork release keeps the old capacity PoA path bit-exact, because mainnet sync from genesis needs it.
- The no-version-tag header decision depends on the reset happening at this same activation. If v2 misses Estuary, it needs its own fork and another testnet reset.

**Order of work.**

1. The wire-format decisions are closed (§15). `tx_root` v2 is part of the Estuary release (Appendix C). It does not change the data-tree vectors. Its vectors are generated when it is implemented (§13). The gateway team has signed off the item envelope (§15 Q7).
2. Generate the shared data-tree vectors (§13) from one reference implementation and check them into both repos. Each implementation passes them before it merges. The `tx_root` v2 group is not in that set yet.
3. L1 integration (§12) is a separate work item from this crate. Implement L1 and the gateway in parallel. The gateway node simulator emits v2 proofs.
4. Run a joint devnet: the gateway test harness against a node with the partition-resize migration, and the multiversion harness across the activation as a cutover.
5. Release node, gateway services and SDKs together. Reset testnets. Activate on mainnet.

**Persisted data.** A database version bump covers the renamed chunk-key tables and the new `CachedDataRoot` rows, which lose their size-confirmation field. Testnets are reset, so nothing is re-keyed. The submodule databases are moving to RocksDB; the RocksDB branches take the new key names and the note-free proof format.

## 12. Implementation map

**Proposal — not binding.** Planned for L1 integration; may change.

This crate is the data tree and this spec. The rows below are the L1 and gateway integration. That integration is a separate work item (§11).

| L1 area | Change |
| --- | --- |
| `crates/types`: new data-tree module | geometry, tags, `leaf` / `node` / `size_witness`, the `empty` table, `chunk_root`, the streaming builder with its node cache, chunk and segment proofs, verifiers, `chunk_key`; a newtype for each value |
| `crates/types/src/merkle.rs` | after Estuary it no longer builds `tx_root` and it is no longer used for data. It still builds the address-bound half of an ingress proof. The ingress-proof change is the ingress-proofs row |
| `tx_root` | new v2 encoder (Appendix C). Block production and migration compute the new `tx_root`. Block validation checks the partition and rejects a mismatch (Appendix C). Not implemented in this crate |
| `crates/types/src/transaction.rs` | field `data_root` becomes `data_tree_root` at the same position; a `data_commitment()` accessor derives the commitment; `data_chunks()` builds chunks with the v2 builder |
| client builder (`irys.rs` merklize, api-client chunk upload) | v2 tree and chunk proofs; chunk messages carry `data_commitment` |
| `crates/types/src/chunk.rs`, gossip types | `ChunkKey`; the root field of `UnpackedChunk` holds `data_commitment` |
| consensus config | `segments_per_chunk`: a power of two, 1 ≤ SPC ≤ 256, `chunk_size % SPC == 0`, checked at load (§2). Where the segment-PoA rule runs, also reject SPC < 2 at load (segment-PoA spec §6.3 rule 3, §15 Q9). The tree stays general. The original reason (an empty segment proof) no longer applies since §15 Q10. Whether segment-PoA needs SPC ≥ 2 for another reason is open. Settle it against the segment-PoA spec during integration. Chunk proofs and segment proofs end with the size witness, so none is empty (§5, §15 Q10) |
| chunk ingress | `verify_chunk`, `InvalidChunkSize`, deletion of the v1 size machinery (§8); ingress-proof readiness becomes `!block_set.is_empty()` |
| p2p | `InvalidChunkSize` is a peer fault; chunk gossip is keyed by `ChunkKey` |
| database | chunk-key tables (§7); rows keyed by `data_commitment`; size-confirmation state deleted; the chunk backfill verifies with `verify_chunk`; database version bump. The MDBX path tables that parse the last 64 bytes as a v1 leaf, and the RocksDB derive, follow `tx_root` v2 (Appendix C) |
| `crates/database/src/tx_path_derive.rs` | builds v2 `tx_path` proofs (Appendix C). One format from Estuary (§15) |
| storage modules | keys by `chunk_key` and `data_commitment`; `c` from the placement. No empty-proof handling is needed: chunk proofs and segment proofs end with the size witness, and that witness is the same for every chunk of one upload (§5, §15 Q10). Flags and the format byte sit directly after the proof and before the CRC: `[proof][flags][format][crc]` (Appendix A.1). Stored proofs stay leaf to root, as received |
| mempool, block validation and discovery | reject every data tx in blocks up to and including E (§8). Block validation checks `1 ≤ data_size_k`, `ceil(data_size_k / chunk_size) ≤ max_data_tx_chunks` (§2), `chunk_start_0 = 0` and `chunk_start_{k+1} = chunk_start_k + ceil(data_size_k / chunk_size)` with checked arithmetic, and that the header's `tx_root` equals the root of exactly those leaves with `count_witness(N)`, where `N` is the ledger's tx count (Appendix C). A block that fails is invalid. PoA data form per the segment-PoA spec; whole-chunk offset mapping (§8); activation rules (§8). PoA validation verifies the owning tx with `verify_tx` (Appendix C). Which fields a PoA carries is open (Appendix C item 2). `InvalidPromotionDataSizeMismatch` and the `tx_selector` size filter deleted |
| release freeze | scan mainnet for any data tx or data-chunk PoA (§11). Expect none. One found means the §8 refusal does not hold, and nodes that sync from genesis keep today's data verifier for those blocks, gated by height |
| mining, chunk migration | `verify_chunk` against the header commitment |
| ingress proofs | Today `verify_ingress_proof` (`crates/types/src/ingress.rs:224-256`) rebuilds an address-bound tree and a v1 data tree. The address-bound tree hashes each chunk with the miner address (`crates/types/src/merkle.rs:492`, `merkle.rs:714-718`). The signature covers both roots, `chain_id` and the anchor (`ingress.rs:136-144`, `ingress.rs:249-256`). The address-bound tree stays on the v1 encoder until the custody redesign replaces ingress proofs. The data half changes from v1 `data_root` to v2 `data_commitment` from the v2 builder (§15 Q6). The v2 builder has no miner address, so it does not replace that tree. The planned signature preimage fields are `data_commitment`, the address-bound root, `chain_id` and the anchor. The exact layout is finalized in L1 integration (§11). Rows and gossip are keyed by `data_commitment`, so one proof covers one (data, size). A planned range-scoped proof also signs the chunk range `[first, last]` (chunk indices, never byte offsets). A contiguous range could reuse the v2 data tree (subtree roots plus the boundary siblings). That API needs its own section and vectors |
| API | `/internal/chunks` returns the v2 `data_path` with no version field (one format). JSON: the header key `dataRoot` becomes `dataTreeRoot`, chunk messages carry `dataCommitment`, and the chunk-by-root route is keyed by the commitment. Agree the names and the route with the gateway team |
| tests | the shared vectors (§13), a shape lock for `C` from 1 to 4,097, property tests over random sizes, the negative cases |

| Gateway area | Change |
| --- | --- |
| Item envelope | signs `data_tree_root` and `data_size`. `data_commitment` is derived, as on the L1 header (§6, §15 Q7) |
| Receipt verifier | follows `tx_root` v2. A proven leaf binds `tx_id`, `data_tree_root`, the exact `data_size`, `prefix_hash`, `prefix_size` and `chunk_start`. The count witness binds `N`. Placement follows the §8 rule (§6, Appendix C) |
| `bundle_items_root` | may adopt the same form as `tx_root` v2, with its own tags. The gateway team decides (Appendix C) |
| Custody framing | multi-segment answers use independent segment proofs that share one chunk proof (§5). The custody spec defines the framing |

**Gateway, SDKs, simulator.** One gateway library serves the data tree, `tx_root` and the bundle index trees. This crate changes the data tree. The receipt verifier follows `tx_root` v2 (Appendix C). `bundle_items_root` may adopt the same form with its own tags; the gateway team decides. Other changes: receipts, relay, bundler, upload service, SDK, wasm build and node simulator. Receipt header bindings shrink from up to 529,216 B (1 to 2 full chunks with paths) to at most 10,240 B (1 to 2 segments with proofs). The joint L1 and gateway proposal lists every file. Gateway state resets with the testnet reset.

## 13. Test vectors

The printed values in this section came from an independent Python reference. That reference uses the Python standard library only. It is not in this repo. Proof order is leaf to root (§5). The size witness covers `data_size` only: `H(0x02 ‖ be64(data_size))` (§3). The reference also checked:

- the streaming builder against the recursive definition for every `C` from 1 to 299;
- that every chunk proof and segment proof verifies, for every `data_size` from 1 to 199 in test geometry;
- these negative cases: wrong size with the same and with another `C`, `c ≥ C`, a proof one hash too long and too short, a flipped sibling bit, a zero-padded chunk, a 64 B segment equal to `l ‖ r`, and an unanchored commitment;
- the depths 25 and 23 at the maximum size.

Data is `b[i] = i mod 251`. Proofs are leaf to root and end with `size_witness(data_size)`.

```
leaf("")              6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d
empty(6)  (SPC 64)    2be3cf0551cc6936d461e3dc43f3c4bf50cbee1bc091925254e879f4e7665e94
size_witness(1)       f83f60940c1ec44c0f1e90f694c6cae7c99b4b6d1507d60ad5a3282a7750d0ee
size_witness(4096)    719e72af15abf7c6f95ea297c02cd24b41b6b3467a78583a7103dbb19d45571f
size_witness(262144)  313eba86534a86e9270cfa6cb7cee900b753358a0c5ef61330202481653d22eb
size_witness(917504)  42bb7029ceeb50156a1b044c1af62a0d78207b52708ff5c31a78ed81c2bd595b
```

Test geometry (`chunk_size` 32, SPC 4, `segment_size` 8):

```
n    C  data_tree_root                                                    data_commitment
1    1  8521b87ff724236722cfb6d6306ad59bceb4aee3e35ae73c41326cd31915e13e  a107920f86fd22b8f786cace824308964a955e019625b0246a2caf3cdd122eb6
8    1  2239889b93ac82ead2db9605f2584a25255c90206c1fccaddf54caa75b7fc1a1  87bbfb2c56ebf02b55bd51d32633445fe986f4031be1a3d04d09fac9d447106e
9    1  3ba2878285288e004dcd35a73e9a3df4a19f5fe5acccaf8d84b50fc7254e6ddb  55100cd7d0c2bdbc9d03808eebcde5d5d144f27e021e3477c8a84834d5a0de94
32   1  d3e81f198d6bd3782d1941172c89e49721d8f5e33cc709d4f6ba3f54b9e35f82  00f0535265357690ad8c75c88235490e7340e4cef3a81157c2c473861866d8d7
33   2  bf98e27c471bfda3de1d44b4eb420f528238aa75e6e002fe1636c04a431713a9  ca888a5334e649c2c8a6ddece5b132464e67b1f59f0b5b5d02433070cce02fcf
112  4  7fd698c547deb0bbb8f8ab7db08dd34616a496f141167252487509cb74824564  cf1fcae00134fbb7e7317da641c092740b22136c469ae46b6d1db03c4f01dbe5
157  5  81e7a996cd2ba141815aa48a1afe4f9c3d5c0ca450f12c9a605c20a0c3037c95  f8ba5215b628deec40d735d6cdf825016fca443f0f8cbff3eb5361de59e8ab03

The reference printed the sibling hashes. The last line of each proof is size_witness(112).
n = 112, chunk proof c = 2:
  dbc3c2433981a503f8ffa38629a9ec5d41501618b2afd860af52d6a93e06e8b6
  50cb073b3d437e037c34dc0d757489f56fd63b9122dd4edfbe8bc42812a5bd84
  f127eb7779eafb7f0f10fd2f7fa512d89e862c71b77e4424f40f02ccc6220945
n = 112, segment proof (c = 3, j = 1):
  82bdff7cd7950d248431b84dd588f1777777ee141ebc86ac8f72080aaf11e0e5
  fe43d66afa4a9a5c4f9c9da89f4ffb52635c8f342e7ffb731d68e36c5982072a
  f8e87ba8c906d8391fb1182e6c1e7d123392e8770d510b547edb3bbffe7cfb65
  50cb073b3d437e037c34dc0d757489f56fd63b9122dd4edfbe8bc42812a5bd84
  f127eb7779eafb7f0f10fd2f7fa512d89e862c71b77e4424f40f02ccc6220945
n = 112, chunk_key(c = 3):
  b541f01b947453085184d77f79e23e48acd6d3f0ff773843392ab4dafad1b425
```

Mainnet geometry (`chunk_size` 262,144, SPC 64):

```
n        C  data_tree_root                                                    data_commitment
1        1  2438dc9c444ab818a41197dfd3aa6502bc3e2fb03895b7ed79ab275539b7972b  719863e12c1e714b1df22d0600ed93ad96486cbc7920f565a5e434574463d013
4095     1  851d7b0214e05f85e581de8dd408f5a4b0c31d8baad3f42986f056949ee1d575  e773c7e0780a33fca2cbb91aae23fc0ba476ea86eb156ee79e93df35d1dda5be
4096     1  be58572dcfba33df3201cdbeda2799dd0c5b508a0df41a47912cb008896f9195  be251ef52dcd4fd580211d8fb5eb451be18a3e02c16d4512b56953d9a3c1e748
4097     1  f516d84903ce88d9d1d80d4071ce945dc53707bd4ed60346a3ba6392412c9134  ce4a935a089dd36beb84501e6813bf4e328bb0ed15307ac3c4a6ab246d04c3c6
262143   1  b6dd65dde53181e3443d128d05b7cbf57c5f4352caa8216ed365d716896335bd  7db8ca90c9fab87dc8577252f4ac084bfbc12f491ff049318352623ae715a83c
262144   1  8e2d2d85eb5fd8e66d4f4b874b5ad6b5aa46cad790fbe64f61c374d8f8243d5f  97c9ecd15caca57f3f74198a5a456d583fedd22ed267f951cce1cd14998b6e4f
262145   2  dcc4ac982c84bdf0d6df5c16d2728b65bb4bae355b078b5a22e47a6a3e518825  75f80cf9008e5f4c65a847f036fccd0c10176cc9840f82a4f0d6b846404d6326
917504   4  3d86c811acf3356576cbe00b28814ad89ee42bd5aa36b755cea977620366de67  472cfc752f2aacd4662235abe756da1e2079db705736ad22a6db9db6f6d6cc9c
1310720  5  1d5501730df0426a6f2b223d3fc31703d5eb6e64bae166736c920ccd23594ac0  027fba531187d84813dd203d3e03dda3a3898cde1bbfd2675b54e46c230d6e89
```

**Full shared set** (rollout step 2). The shared vector files under `crates/data-tree/vectors/` are generated by `examples/gen_vectors.rs` (`cargo run -p irys-data-tree --example gen_vectors -- <out_dir>`). `tests/vectors.rs` checks them. The directory holds `chunk_proofs.json`, `multi_segment.json`, `negative.json`, `primitives.json`, `roots_mainnet.json`, `roots_testing.json`, and `segment_proofs.json`.

| Group | Cases |
| --- | --- |
| Primitives | `leaf` of 0, 1, 4,095 and 4,096 B; `node`; `size_witness` of 1, 4,096, 262,144, 917,504, `2^40` and 5 TiB; `empty(0..=8)`; `chunk_key` at `c` = 0, 1, `2^24` |
| Roots, mainnet geometry | `data_size` 1; 4,095; 4,096; 4,097; 262,143; 262,144; 262,145; 524,288; 786,432; 917,504; 1,310,720; `C` = 8, 9, 16, 17; 1 GiB; 1 GiB + 1. Each with `i mod 251` bytes and with all-zero bytes. Output: chunk roots, `data_tree_root`, `size_witness`, `data_commitment` |
| Roots, test geometry | `chunk_size` 32, SPC 4: `data_size` 1, 8, 9, 32, 33, 112, 157 |
| Chunk proofs | every chunk for `C ≤ 17`; chunks 0, 2,047 and 4,095 of 1 GiB; `C = 5`, `c = 4` (one sibling, then the size witness) |
| Segment proofs | every segment for `C ≤ 2`; for 917,504: `(3, 5)`, `(3, 31)` (last full), `(3, 32)` (first empty), `(3, 63)`; the partial segment `(0, 1)` of 4,097 B |
| Multi-segment | option B. In one chunk, `m` = 1, 2, 4 and 64: each case is `m` segment proofs (the `h` in-chunk siblings each) and one shared chunk proof. The full segment proof is those siblings concatenated with the chunk proof, including when `C = 1`. An adjacent pair in one chunk. A pair in two chunks, which is two chunk proofs |
| Receipt header binding (gateway) | a 100 B span inside one segment, across two segments of one chunk, and across a chunk boundary. Not generated yet. |
| `tx_root` v2 (Appendix C) | `N` = 1, 2, 3, 100; every `k` for small `N`; the 40/1/64-byte ledger (Appendix C); negatives: wrong `N`, wrong `k`, wrong witness, a flipped bit per sibling, a wrong `tx_id`, a wrong `prefix_hash`, a wrong `prefix_size`, a wrong `chunk_start`, `data_size = 0`, one hash too many or too few. Not generated yet. Generated when `tx_root` v2 is implemented |
| Negative | wrong `data_size` with the same `C` and with another `C`; `c ≥ C`; proof one hash too long and too short; empty proof for `C > 1`; chunk length ±1; one flipped bit per sibling; a zero-padded segment; `data_size = 0`; a valid proof shown at another index with different bytes; a 64 B segment equal to `l ‖ r`; a commitment re-derived from an unauthenticated pair (§6) |
| Wire | the tx header with `data_tree_root`; the chunk message JSON; the `/internal/chunks` response; the gateway item envelope (signs `data_tree_root` and `data_size`) and receipt. Not generated yet. |

## 14. Open decisions

**Proposal — not binding.** Planned for L1 integration; may change.

Q1 to Q10 are decided (§15). These items stay open. They are integration proposals. They are numbered as in Appendix C. Item 3 is closed (§15). They do not change the data-tree wire format or the data-tree vectors.

2. What a PoA carries: `(k, N, chunk_start)` explicitly, so a validator needs only the owning tx header; or derived from the block's tx headers (smaller PoA, more reads). With the block-validation rule (Appendix C), the option that verifies with only the owning tx header is safe, because the `tx_root` it checks against has passed full validation.
4. Where the code lives: `irys-data-tree`, which shares `T`, `climb`, `depth` and the tag registry, or a separate crate.

**Segments per chunk.** The node still rejects SPC < 2 wherever the segment-PoA rule runs (§8). The original reason (an empty segment proof) no longer applies since §15 Q10. Whether segment-PoA needs SPC ≥ 2 for another reason is open. Settle it against the segment-PoA spec during integration. The tree's rule is unchanged (§2).

## 15. Decision log

The log is append-only. A row is not reworded. The first table binds the data tree. The second table is integration planning.

**Decided — the data tree (binding).**

**Normative.**

| Date | Decision |
| --- | --- |
| 2026-10-21 | Q1. Proof order is leaf to root, and leaf to root is the default order for every proof in the protocol. Compression is not a reason to pick an order: the measured gap comes from where the constant row bytes sit, not from the order (Appendix A.1) |
| 2026-10-21 | Q2. A multi-segment answer is independent segment proofs that share one chunk proof. A compact multiproof saves about 1.4% and needs a second consensus verifier, so it is not this format. It can be a later format if custody needs it (design: Appendix B) |
| 2026-10-21 | Q5. The size witness covers `data_size` only: `H(0x02 ‖ be64(data_size))`. Geometry is already on the consensus surface a client needs before it can verify, so the commitment does not repeat it |
| 2026-10-21 | Q10. Every proof ends with `size_witness(n)`, leaf to root, so the witness is the last hash. A chunk proof is the chunk-level siblings then the witness: `32 × (depth(c, C) + 1)` bytes, and for `C = 1` it is `[size_witness(n)]`. A segment proof is the `h` in-chunk siblings then that chunk proof. Plain concatenation is the full segment proof in every case, including one chunk. The verifier checks the length, requires the last hash to equal `size_witness` computed from the `data_size` it is checking, and does not climb until that matches. It then climbs the siblings and requires `node(root, size_witness(n))` to equal the commitment. A sibling-only proof of a one-leaf tree is empty, and containers that store or sign proofs confuse empty with missing: a header binding with empty lists becomes absent, nested inner proofs are skipped when empty, and `ChunkWithProof.proof` defaults to empty. Witness-last costs 32 B per proof on the wire. In a storage-module row the witness is the same for every chunk of an upload, so LZ4 shares it. Rejected: leaf-first for every proof, which adds 32 B per proof and per storage row, and the leaf differs per row so LZ4 cannot share it; and a leaf only when the proof would otherwise be empty, which is a special-case branch |
| 2026-10-19 | Tag registry `0x00` to `0x04` (`0x04` is the mining ticket) and `segments_per_chunk ≤ 256` (segment-PoA spec, ticket encoding) |
| 2026-10-18 | Chunk identity is `chunk_key(data_commitment, c)`, with the table and type names in §7 |
| 2026-10-18 | `segments_per_chunk` is a validated consensus config field (64 on mainnet and testnet, 4 in `testing()`) |
| 2026-10-18 | Names: `data_tree_root`, `size_witness`, `data_commitment`; `data_root` means v1 only; a newtype per value |
| 2026-10-18 | Witness layout: `data_commitment = node(data_tree_root, size_witness)`, the size witness as the root's right child |
| 2026-10-22 | A segment proof pins each in-chunk sibling whose subtree lies entirely past the data to `empty(ℓ)` (§5). The subtree starts at `start = ((j >> ℓ) ^ 1) << ℓ`. It is pinned when `start ≥ ceil(chunk_len(n, c) / segment_size)`. Those indexes follow from `(n, c, j)` before the climb. Honest proofs already carry `empty(ℓ)` there. Without the pin, a last chunk can put another hash in that position: every data segment of the chunk verifies, and no bytes pass `verify_chunk`. Segment proofs would then miss a chunk with no preimage |

Status: binding. The leaf rules in the next sentence are part of the confirmed data tree. They are not a separate log row.

Proposed with the approved design, not decided on their own: the last chunk is a perfect subtree with empty leaves, and the chunk level keeps today's RFC 6962 shape.

**Planned — integration (not binding).**

**Proposal — not binding.** Planned for L1 integration; may change.

| Date | Decision |
| --- | --- |
| 2026-10-21 | Q4. A `tx_root` leaf value is `fold(data_tree_root, prefix_hash)` and the span note is the exact `data_size`. A proven leaf authenticates both, so a verifier re-derives `node(data_tree_root, size_witness(span))`. The `tx_root` code stays unchanged. Superseded: see Appendix C |
| 2026-10-21 | Q6. The ingress-proof change is §12. The data half is `data_commitment` from the v2 builder, so one proof covers exactly one (data, size). A planned proof of a chunk range binds `data_commitment` and that range for the same reason |
| 2026-10-21 | Q7. The gateway item envelope signs `data_tree_root` and `data_size`, and `data_commitment` is derived, as the L1 header does. This anchors the size and keeps one format for L1, the gateway and SDKs. Owner decision; the gateway team signed off on 2026-10-21 |
| 2026-10-21 | Q8. Every data tx in a block up to and including Estuary's activation block E is refused. Mainnet stores no data and test networks reset, so no pre-activation data tx can exist, and the fork release needs no v1 data code and no anchor rule |
| 2026-10-21 | Q9. The tree's rule is 1 ≤ SPC ≤ 256, a power of two, and it sets no tighter protocol floor. A bound is a property of how the tree is used, not of the tree. The irys node rejects SPC < 2 wherever the segment-PoA rule runs, because an empty segment proof signs like an absent optional RLP field. Reason void since Q10; see 2026-10-22 row |
| 2026-10-21 | The data root is external: clients, SDKs and the gateway commit to it, so it lands first and is hard to change later. `tx_root` is internal to the protocol and can change in any fork |
| 2026-10-21 | `tx_root` v2 ships at Estuary (Appendix C): it commits to its leaf count with a count witness, and its proofs end with that witness. Gateway receipts are meant to be near self-contained paths from `tx_root` to a chunk, so a verifier holding only `tx_root` must get the count bound |
| 2026-10-21 | Q3. `tx_root` v2 (Appendix C) ships at Estuary, in the same hardfork as the data tree. Tags `0x05` to `0x07` are in the §3 registry. Estuary already requires a data reset: test networks reset, and mainnet stores no data. Q8 refuses data txs up to and including E, so every pre-activation `tx_root` is `0^32` and no data PoA exists. One format. No dual format. A later fork would need both formats, gated by height. Appendix C items 2 to 4 stay open. L1 integration (§12) is a separate work item |
| 2026-10-21 | The `tx_root` v2 leaf binds `data_size` directly (Appendix C). This replaces the Q4 mechanism, where the span note is the exact `data_size`, with the leaf field. The intent is the same: a proven leaf authenticates `data_tree_root` and the exact `data_size`, and a verifier re-derives `node(data_tree_root, size_witness(data_size))` |
| 2026-10-22 | The `tx_root` v2 leaf keeps `chunk_start` (Appendix C). A tx proof shows membership, the committed `data_size`, and which block chunks the tx occupies. Placement follows the §8 rule. Receipts are near self-contained paths from `tx_root` down to a chunk (block header → `tx_root` → tx → `data_commitment` → chunk), so the leaf lets a receipt show that ledger chunk `b` is tx `k`'s chunk `c` without the block's tx list, and lets PoA verify with the owning tx header. Considered and not chosen: membership and committed size only, as v1 `tx_root` means today, with placement recomputed from the block's tx headers with the padded-size helper. A receipt would then need the tx list, or a separate placement proof, to map a ledger chunk |
| 2026-10-22 | The `tx_root` v2 leaf is one preimage, 121 B (Appendix C): `H(0x05 ‖ tx_id ‖ data_tree_root ‖ be64(data_size) ‖ prefix_hash ‖ be64(prefix_size) ‖ be64(chunk_start))`. `prefix_hash` stays. This closes item 3. The gateway receipt verifier checks the receipt's `prefix_bytes` against `prefix_hash` (`crates/shared/gateway-core/src/verify.rs` in the gateway repo). That hash is authenticated only through this leaf. Dropping it would leave the bundle-header binding unanchored. The network does not check `prefix_hash` against the bytes (`crates/types/src/transaction.rs`). The value is signer-asserted, and the leaf fixes it to the tx. `tx_id` is added. It is keccak256 of the signature bytes (`DataTransactionHeader::sign` and `is_signature_valid` in `crates/types/src/transaction.rs`). The signature covers the signed header: `encode_for_signing` omits `id` and `signature`, and the preimage includes `data_root` (this spec's `data_tree_root`, §7), `data_size`, `prefix_hash`, `prefix_size`, `signer`, `anchor`, the fees, `ledger_id`, `chain_id` and `metadata_format`. One 32-byte field binds that header. A receipt names the exact transaction, and anyone holding the header can link it. Signer, fees, anchor and `chain_id` need no separate field. `ledger_id` is fixed by which per-ledger `tx_root` the leaf is under. `prefix_size` is added. The prefix bytes hash to `prefix_hash`, which fixes their length, but a consumer that does not hold those bytes needs `prefix_size` to locate the bundle header. It costs 8 bytes of hash input and no proof bytes. Items 2 and 4 stay open |
| 2026-10-19 | The fork release deletes the old data-PoA verifier and keeps the capacity PoA path bit-exact |
| 2026-10-19 | `tx_path` note padding: if `tx_root` keeps its format, the PoA check rejects non-zero padding from activation |
| 2026-10-19 | One activation for all of it: Estuary's epoch-aligned activation |
| 2026-10-19 | The data tx header keeps its layout; no version tag. This relies on the testnet reset at the same activation |
| 2026-10-18 | No v1 and v2 coexistence: test networks reset, and mainnet stores no data |
| 2026-10-18 | The header stores `data_tree_root` and `data_size`; `data_commitment` is derived, never encoded |
| 2026-10-22 | Q9's reason is void after Q10, and the node rule is kept. The node still rejects SPC < 2 wherever the segment-PoA rule runs. The original reason (an empty segment proof) no longer applies since Q10. With SPC = 1 and C = 1 the segment proof is `[size_witness(n)]`, 32 bytes, never empty. Whether segment-PoA needs SPC ≥ 2 for another reason is open. Settle it against the segment-PoA spec during integration (§14). The tree's rule is unchanged: 1 ≤ SPC ≤ 256, a power of two |

## Appendix A. Measurements

### A.1 Proof order and RocksDB compression

This measures the stored size of leaf-to-root and root-to-leaf chunk proofs under LZ4, and the effect of the row trailer.

**Rows.** 65,536 proofs, mainnet geometry (`chunk_size` 262,144, SPC 64). The one-upload case is one 16 GiB upload. Every proof has depth 16. The measured sibling list, without the size witness, is 512 bytes. A raw row is that proof plus a 6-byte trailer: 518 bytes. Value-only LZ4 stores that row at about 80 bytes.

These measurements used proofs without the size witness (§15 Q10). The witness is identical for every row of an upload, so LZ4 shares it. The tables were not re-run.

**Production settings, as of this writing.** `ChunkMetaByOffset` uses `RocksTuning::baseline` (`crates/database/src/submodule/rocks.rs:172-192`) through `column_options` (`rocks.rs:3617-3696`). Compression is LZ4 (`rocks.rs:3646-3658`). The block size is `BLOCK_BYTES`, 64 KiB (`rocks.rs:60`). `encode_offset_meta` (`crates/database/src/submodule/records.rs:30-54`) writes a non-empty proof, then the CRC little-endian when present (`records.rs:48-50`), then the flags byte (`records.rs:44-47`). `HAS_PROOF | CRC_PRESENT` is `0x03` (`records.rs:16-18`). The format byte is `1` (`records.rs:14`, `records.rs:53`). An empty proof returns an error (`records.rs:37-39`).

**Value-only LZ4.** Consecutive row values, no keys, packed on row boundaries into groups of at most 64 KiB. Each group is one LZ4 default block with no size prefix. 65,536 rows of the one upload:

| Row trailer layout | leaf to root | root to leaf |
| --- | --- | --- |
| today: `[proof][CRC 4 B][flags 0x03][format 0x01]` | 80.04 B/row | 77.90 B/row |
| `[proof][flags][format][CRC]` | 77.65 B/row | 79.81 B/row |
| 6 random trailer bytes | 80.02 B/row | 79.82 B/row |

**RocksDB SST.** The same proof bytes, today's trailer only, and the production `ChunkMetaByOffset` options above (LZ4, 64 KiB blocks). The size is the live SST after one batch, a flush, and a full compaction with bottommost compaction forced.

| Upload mix | leaf to root | root to leaf |
| --- | --- | --- |
| one 16 GiB upload | 5,616,187 B | 5,422,387 B (-3.45%) |
| mixed uploads | 5,459,663 B | 5,264,187 B (-3.58%) |
| one-chunk uploads | 651,429 B | 651,429 B |

**Why.** LZ4 copies the run of upper siblings that neighbouring proofs share, in either order. Leaf to root puts that run at the end of the proof. Root to leaf puts it at the start. A match extends only over bytes that are equal in both copies, and a match needs 4 bytes to start. The two constant trailer bytes (`0x03`, `0x01`) therefore help only the order in which they touch the shared run. A per-row CRC between the run and those bytes blocks the extension. With six trailer bytes that vary per row, the orders tie: 80.02 B/row and 79.82 B/row, a 0.25% gap. Proof order itself does not change compressibility.

**Conclusion.** There is no storage reason to prefer an order. Place the constant row bytes next to the shared run. Leaf to root is the protocol default (§5), so that run is a suffix: the flags and the format byte go directly after the proof, and the CRC goes last (§12).

**Rebuilding the measurement.** The bench code is not in this repo. Two programs produced the tables. Both use `Geometry::MAINNET` and the streaming builder: `push_chunk_root(leaf(preimage), chunk_size)`, then `finish`, then `chunk_proof`. The preimage is 12 bytes: a `u64` upload id in little-endian, then the chunk index as a `u32` in little-endian. Leaf to root is the proof the builder returns. Root to leaf lists those 32-byte hashes from the last hash back to the first.

The per-row CRC is `splitmix32` of the offset, as a wrapping `u32`. Start from `offset + 0x9E3779B9`. Then `x = (x xor (x >> 16)) * 0x85EBCA6B`, `x = (x xor (x >> 13)) * 0xC2B2AE35`, and the result is `x xor (x >> 16)`. This is not a checksum of a chunk. It makes four trailer bytes differ per row. The same CRC is used for both orders.

Value-only table. One upload, id 1, 65,536 chunks. Three trailers, each 6 bytes:

- today: CRC little-endian, then `0x03`, then `0x01`
- constants first: `0x03`, `0x01`, then the CRC little-endian
- 6 random bytes: CRC little-endian, then the low 2 bytes of `splitmix32(offset xor 0xFFFF)`. Every one of those six bytes can differ per row. None is constant

Pack the rows in order into groups of at most 64 KiB. Split only between rows. Compress each group with `LZ4_compress_default`. The `lz4` 1.28.1 crate calls this `lz4::block::compress` with `CompressionMode::DEFAULT` and the size prefix off. Sum the compressed lengths and divide by 65,536.

SST table. Today's trailer only. A non-empty value is the byte string `encode_offset_meta` writes for a proof and a CRC. One-chunk rows in this measurement are the 6-byte trailer and no proof bytes: the measured proof is the sibling list, and that list is empty for one chunk. `encode_offset_meta` returns an error for an empty proof (`records.rs:37-39`), so the measurement writes those six bytes directly.

- One 16 GiB upload: upload id 1, every proof 512 bytes.
- Mixed uploads: each upload is its own tree. Lengths are log-uniform on 1..=4,096 chunks, seed `0xA11CE`, and the last upload is clipped to the chunks left in the 65,536 span. Upload ids start at 1. Draw with xorshift64 (`x ^= x << 13`, `x ^= x >> 7`, `x ^= x << 17`). Bucket `draw % 13` selects the size class: bucket 0 is 1 chunk, and bucket `k` (1..=12) is `2^(k-1)+1 ..= 2^k`. The size inside the class is uniform in the draw. This draw yields 96 uploads.
- One-chunk uploads: 65,536 trees of one chunk. The measured one-chunk proof is empty, so the order does not change a byte.
- Key: the offset as 4 big-endian bytes. Offsets are `0..=65535`.
- Open `ChunkMetaByOffset` with the baseline options cited above. Write one batch, flush the family, then compact the whole key range with bottommost compaction forced. The SST size is the sum of live file sizes for that family.

The one-chunk SST matches to the byte in either order. The blobs preset sends a value to a blob file at or above 2,048 bytes (`BLOB_MIN_BYTES`, `rocks.rs` line 67). Every row here is 6 to 518 bytes, so that preset writes the same SST and no blob file. Block-cache size does not change these SST bytes.

### A.2 Gateway receipt chain (as of 2026-10-21)

A read of the gateway codebase (the `gateway` repository, paths relative to its root) for §15 Q10 and the `tx_root` design (Appendix C). Findings:

- **The receipt carries no block header.** A receipt is the JSON `DataItemReceipt` (`crates/shared/dto/src/lib.rs`): block height and hash, `txRoot`, `txLedgerIndex`, `txByteOffset`, and base64url proofs. The gateway's node stream parser keeps only a block's height and hash (`crates/shared/chain-stream/src/wire/node.rs`). The header's `tx_root`, `tx_ids` and signature are dropped.
- **The anchor is a `tx_root` rebuilt from every tx in the block.** `verify_data_item_receipt` (`crates/shared/gateway-core/src/verify.rs`) takes a `RelayAttestedAnchor` (`crates/shared/gateway-core/src/anchor.rs`) rebuilt from the relay's quorum population rows. Its comment: "This is **not** a node-header-signed `tx_root`". The leaf count is the row count, so a verifier must fetch the whole tx list to check one item. The tx index in a receipt is checked against that list, not proven.
- **Empty and missing collapse in three containers:** a header binding whose lists are both empty becomes absent; nested inner proofs use `skip_serializing_if = "Vec::is_empty"`; `ChunkWithProof.proof` uses `#[serde(default)]`. These fail closed today only because every v1 proof is at least 64 bytes. A sibling-only one-leaf proof would fall into each of them. This is why §15 Q10 makes every proof end with its witness.
- **One Merkle library serves four trees** (`crates/shared/bundle-format-v2/src/merkle.rs`): `data_root`, `tx_root`, `bundle_items_root` (leaf: item `data_root` and `data_size`) and `item_headers_root`, all in the v1 note format. `bundle_items_root` has the shape of `tx_root`.
- **What a format change breaks:** receipt proof bytes are not golden-pinned. The receipt JSON shape, the indexer OpenAPI snapshot and two Merkle root hex values are.

**Proposal — not binding.** Planned for L1 integration; may change.

Consequences: `tx_root` v2 commits to its leaf count (count witness, Appendix C), so a receipt can be a near self-contained path from an anchored `tx_root` to a chunk; every proof ends with its witness (§15 Q10). Placement on that path follows the §8 rule. Today's `txByteOffset` is a sum of `data_size` and is not a v2 placement coordinate (Appendix C).

## Appendix B. Compact multi-segment proof (designed, not adopted)

§15 Q2 chose independent segment proofs that share one chunk proof (§5). This appendix keeps the design of the compact form so it can be adopted later, as a separate format, if custody answers need the bytes. Nothing in this spec or crate implements it, and no vectors exist for it.

**What it saves.** In option B every segment carries all `h` in-chunk siblings, even when another revealed segment's climb already computes that sibling. The compact form sends only the siblings that no revealed segment can compute. Counts below are for `h = 6` (mainnet), checked over every index set:

| Segments revealed in one chunk | Option B in-chunk hashes | Compact in-chunk hashes |
| --- | --- | --- |
| 1 | 6 | 6 |
| 2 | 12 | 5 (adjacent pair) to 10 |
| 4 | 24 | 4 to 16 |
| 64 (whole chunk) | 384 | 0 |

At 4 segments the compact form saves at least 8 hashes (256 B): about 1.4% of an answer that carries 4 × 4 KiB of segment bytes. The chunk proof is the same in both forms and is sent once. It ends with the size witness (§5).

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
    chunk_proof  32 × (depth(c, C) + 1) bytes, leaf to root, ending with size_witness(n), as in §5
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

verify_compact(D, n, c, proof, segment_bytes):
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
    require the last hash of chunk_proof == size_witness(n)   // before the climb
    r = climb(r, c, C, chunk_proof without its last hash)
    require node(r, size_witness(n)) == D
```

**Rules that make it safe.**

- *Canonical.* Strictly increasing indices give one encoding per index set, and the walk gives one `aux` order. For a fixed `(D, n, c, J)` the valid proof is unique up to SHA-256 collisions, as in §10.
- *Length before hashing.* `aux_count(J)` needs only the indices. The exact proof length is checked before any hash, and `m ≤ SPC` caps the parse.
- *Every hash used.* A proof with a spare or a missing `aux` hash fails the length check. The final `next_aux == a` check is an invariant, not a second length check.
- *Positional binding.* As in §10, the shape is fixed by `(n, chunk_size, SPC)`. A revealed segment hashes at its exact length, and a segment past the data is `leaf("")` with empty bytes.
- *Same anchor.* The commitment must be anchored (§6).

**If it is adopted.** Add a `verify_compact` beside `verify_segment` with the same error kinds, an encoder on `DataTree`, a §5 row, a §13 vector group (the option B cases, plus `m = SPC`, an index set with `aux_count = 0`, and the negative cases: unsorted or repeated indices, `j ≥ SPC`, `m = 0`, `m > SPC`, one `aux` hash too many or too few, a flipped bit in each `aux` hash), and property tests that the compact proof and the option B proofs agree on every index set of small geometries. A joint proof across several chunks of one upload (a chunk-level multiproof over the RFC 6962 shape, with carried nodes) is a further step and is not designed here.

## Appendix C. `tx_root` v2 (designed, ships at Estuary, not implemented yet)

**Proposal — not binding.** Planned for L1 integration; may change.

`tx_root` v2 is decided for Estuary, in the same hardfork as the data tree (§15). Nothing in this crate or the node implements it yet. It applies the data tree's rules to the ledger tree: positional leaves, one SHA-256 call per node with domain tags, no notes, a witness that binds the leaf count, and witness-last proofs (§15 Q10).

**Why it changes.** Today's `tx_root` (`crates/types/src/merkle.rs`, shared with v1 `data_path` and today's ingress proofs):

- Each node carries a 32-byte note, of which 24 bytes are unchecked padding. One `tx_path` has many byte encodings.
- The notes are prefix sums of raw `data_size`, but txs are placed in whole chunks. A PoA target `block_chunk × chunk_size` lands on the wrong byte after any tx that does not end on a chunk boundary.
- It has no domain tags. Tags close the `tx_root` half of the §10 cross-tree caveat. The address-bound half of an ingress proof stays on the v1 encoder (§12).
- It does not commit to its leaf count. Gateway receipts are meant to be near self-contained Merkle paths from `tx_root` down to a chunk (block header → `tx_root` → tx → `data_commitment` → chunk), so a verifier that holds only `tx_root` must still get the count bound (Appendix A.2).

**Hashing.** Tags `0x05` TX_LEAF, `0x06` TX_NODE and `0x07` TX_COUNT are in the §3 registry. They are not part of the data tree. The code enum gains them when this is implemented.

```
N           = number of data txs in this ledger of this block, in tx_ids order k = 0 ..= N−1
chunk_start_k = Σ_{i<k} ceil(data_size_i / chunk_size)          // padded chunk count before tx k (§8)
leaf_k      = H(0x05 ‖ tx_id_k ‖ data_tree_root_k ‖ be64(data_size_k) ‖ prefix_hash_k ‖ be64(prefix_size_k) ‖ be64(chunk_start_k))   // 121 B
tx_root     = H(0x06 ‖ T(leaf_0 ..= leaf_{N−1}) ‖ count_witness(N))   for N ≥ 1     // T() of §4 with TX_NODE
tx_root     = 0^32                                                     for N = 0     // unchanged sentinel
count_witness(N) = H(0x07 ‖ be32(N))
```

- The leaf is 121 B and has one parse. Every field is fixed width: the tag (1 B), `tx_id`, `data_tree_root` and `prefix_hash` (32 B each), and `data_size`, `prefix_size` and `chunk_start` (8 B each).
- The leaf binds `tx_id`, `data_tree_root`, the exact `data_size`, `prefix_hash`, `prefix_size` and `chunk_start`.
- A proven leaf gives the anchored commitment `node(data_tree_root, size_witness(data_size))` (§6). The `data_size` field replaces the span note (§15).
- `chunk_start` is the padded chunk count before the tx. Placement follows the §8 rule.
- Every leaf has `1 ≤ data_size_k` and `ceil(data_size_k / chunk_size) ≤ max_data_tx_chunks`, the §2 bounds. The builder does not emit a leaf outside those bounds.
- A separate node tag means a ledger-tree node can never be read as a data-tree node.

**Block validation.** The formulas above are how the builder sets `chunk_start`. Block validation must check the same partition, with checked arithmetic, and the §2 bounds on every leaf. `N` is the ledger's tx count. `verify_tx` is not this check.

```
when N = 0:
    require the header's tx_root = 0^32
when N ≥ 1:
    require 1 ≤ data_size_k                                          // every k with k < N; the §2 bounds
    require ceil(data_size_k / chunk_size) ≤ max_data_tx_chunks
    require chunk_start_0 = 0
    require chunk_start_{k+1} = chunk_start_k + ceil(data_size_k / chunk_size)
                                                                       // checked addition; every k with k + 1 < N
    require the header's tx_root = H(0x06 ‖ T(leaf_0 ..= leaf_{N−1}) ‖ count_witness(N))
                                                                       // leaf_k from the hashing section, with these starts
```

A block that fails is invalid. A sum that does not fit in 64 bits fails the check. The prefix-sum equations reject an overlap and a gap. `ceil(data_size_k / chunk_size)` is at least 1, because `data_size_k ≥ 1`, so `chunk_start_{k+1} > chunk_start_k`. `chunk_start` is strictly increasing. Tx `k` owns `ceil(data_size_k / chunk_size)` chunks starting at `chunk_start_k`. The next tx, when there is one, starts where that range ends. The ranges are contiguous and disjoint. Each chunk the ledger places has exactly one owner. The owner of chunk `b` is the one tx whose range contains `b`. A lookup by `chunk_start` is unambiguous.

These size bounds are already consensus. `data_tx_structural_defect` in `crates/actors/src/data_tx_validation.rs` returns `ZeroDataSize` when `data_size == 0`, and returns `DataSizeExceedsMax` when `ceil(data_size / chunk_size) > max_data_tx_chunks`. Mempool ingress and block prevalidation both call that function, before the block's `tx_root` is checked. Block prevalidation reports `ZeroSizeDataTx` and `DataSizeExceedsMax` (`crates/actors/src/block_validation.rs`). A zero-width leaf shares its start offset with the next tx. PoA owner recovery depends on unique start offsets (`is_poa_owning_leaf` in that file).

**Proofs.** Witness-last, like the data tree: the siblings of `leaf_k` in `T`, leaf to root, then `count_witness(N)`. Length `32 × (depth(k, N) + 1)`. Never empty.

```
verify_tx(tx_root, N, k, tx_id, data_tree_root, data_size, prefix_hash, prefix_size, chunk_start, proof):
    require 1 ≤ N and k < N
    require 1 ≤ data_size                                             // before any hashing; data_size = 0 is rejected
    require len(proof) == 32 × (depth(k, N) + 1)                      // before any hashing
    require last hash of proof == H(0x07 ‖ be32(N))                   // own witness; never climb an unchecked one
    r = climb(H(0x05 ‖ tx_id ‖ data_tree_root ‖ be64(data_size) ‖ prefix_hash ‖ be64(prefix_size) ‖ be64(chunk_start)), k, N, siblings)
                                                                       // climb() of §5 with TX_NODE; 121 B
    require H(0x06 ‖ r ‖ H(0x07 ‖ be32(N))) == tx_root
```

`verify_tx` rejects `data_size = 0` before it hashes. The §2 upper bound is the block-validation check above. `verify_tx` does not take `chunk_size`. A block that passed that check has already applied the upper bound.

**Assumption A4.** A proven `chunk_start` is only as trustworthy as the validation of the block whose `tx_root` anchors it. `verify_tx` shows that the claimed start is in that tree. It does not show that the start is the prefix sum. PoA checks against blocks already validated on chain. A receipt's `tx_root` anchor must come from a validated block.

**What a tx proof proves.** Membership of this signed tx at index `k`: `tx_id`, `data_tree_root`, the exact `data_size`, `prefix_hash`, `prefix_size` and `chunk_start`. The count witness binds `N`. The direction bits come from `(k, N)`, so `k` is bound by the proof and is not a separate claim. A receipt names that `tx_id`. It does not need the block's tx list to check the index. Anyone who holds the header links it by `tx_id`. The proof does not show that `chunk_start` is the prefix sum. Block validation does. `verify_tx` rejects `data_size = 0` before hashing. The §2 upper bound is a block-validation check. The proof does not show that the prefix bytes hash to `prefix_hash`. The signer asserts `prefix_hash`, and the leaf fixes that value to this tx.

**Why `chunk_start` is in the leaf.** A receipt is a path from an anchored `tx_root` down to a chunk: block header → `tx_root` → tx → `data_commitment` → chunk. Under assumption A4, the proven placement shows that ledger chunk `b` is tx `k`'s chunk `c` without that tx list, and PoA verifies with the owning tx header. `c = b − chunk_start`, with `b ≥ chunk_start` and checked subtraction (§8). Membership and committed size alone, which is what v1 `tx_root` proves, was considered and not chosen. Placement would then be recomputed from the block's tx headers with the padded-size helper. A receipt would need the tx list, or a separate placement proof, to map a ledger chunk.

**Why `prefix_hash` stays.** The gateway receipt verifier checks the receipt's `prefix_bytes` against `prefix_hash` (`crates/shared/gateway-core/src/verify.rs` in the gateway repo). That `prefix_hash` is authenticated only through this leaf. Dropping it would leave the bundle-header binding unanchored. The network does not check `prefix_hash` against the bytes (`crates/types/src/transaction.rs`). The value is signer-asserted, and the leaf fixes it to the tx. This closes item 3.

**Why `tx_id` is in the leaf.** `tx_id` is keccak256 of the signature bytes (`DataTransactionHeader::sign` and `is_signature_valid` in `crates/types/src/transaction.rs`). The signature covers the signed header. `encode_for_signing` omits `id` and `signature`. The preimage includes `data_root` (this spec's `data_tree_root`, §7), `data_size`, `prefix_hash`, `prefix_size`, `signer`, `anchor`, the fees, `ledger_id`, `chain_id` and `metadata_format`. One 32-byte field binds that header. A receipt names the exact transaction. Anyone holding the header can link it. Signer, fees, anchor and `chain_id` need no separate field. `ledger_id` is fixed by which per-ledger `tx_root` the leaf is under. A second header that copies the other leaf fields and carries another signer or another tx id hashes to a different leaf, so the same proof does not verify.

**Why `prefix_size` is in the leaf.** The prefix bytes hash to `prefix_hash`, which fixes their length. A consumer that does not hold those bytes, such as an indexer, needs `prefix_size` to locate the bundle header. The field is 8 bytes of hash input and adds no proof bytes.

**Receipts.** The tx index `k` is bound by the proof. A receipt must not carry a byte offset as a placement coordinate. Today's `txByteOffset` is a sum of `data_size`. In v2 that field is dropped, or it is `chunk_start × chunk_size` (§8).

**Sizes.** The leaf preimage is 121 B. A proof is still the sibling hashes plus the count witness, so these sizes do not change.

| Data txs in the ledger | Today's `tx_path` | v2 proof |
| --- | --- | --- |
| 1 | 64 B | 32 B |
| 2 | 160 B | 64 B |
| 100 | 736 B | 256 B |

**Activation.** Estuary, with the data tree. One format from that activation. No dual format. Test networks reset, and mainnet stores no data. §15 Q8 refuses every data tx up to and including E, so every pre-activation `tx_root` is `0^32` and no data PoA exists. A v1 `tx_path` verifier is not needed. A later fork would need both formats, gated by height, because `tx_root` and the PoA `tx_path` are signed.

**What changes when it is implemented.** A new encoder. After Estuary, `crates/types/src/merkle.rs` no longer builds `tx_root` and it is no longer used for data. It still builds the address-bound half of an ingress proof. The ingress-proof change is §12. Block production and migration compute the new `tx_root`. Block validation checks the partition, including the §2 size bounds on every leaf, and requires the header's `tx_root` to equal the root of exactly those leaves. A block that fails is invalid. PoA validation verifies with `verify_tx` against a block that already passed that check. `tx_path` derivation (`crates/database/src/tx_path_derive.rs`) builds v2 proofs. The MDBX path tables that parse the last 64 bytes as a v1 leaf, and the RocksDB derive, follow. The gateway receipt verifier follows, and its `tx_root` anchor must come from a validated block (assumption A4). `bundle_items_root` has the same shape (a root plus a size per leaf) and may adopt the same form with its own tags. The gateway team decides.

**Vectors.** One group (§13). `N` = 1, 2, 3, 100. Every `k` for small `N`. Negatives: wrong `N`, wrong `k`, wrong witness, a flipped bit per sibling, a wrong `tx_id`, a wrong `prefix_hash`, a wrong `prefix_size`, a wrong `chunk_start`, `data_size = 0`, one hash too many or too few. Not generated yet.

One case is the 40/1/64-byte ledger, test geometry (`chunk_size` 32). The `data_size`-sum target and the chunk target name different txs. Only the chunk target is right.

| tx `k` | `data_size` | `chunk_start` | block chunks | byte start (sum of `data_size`) |
| --- | --- | --- | --- | --- |
| 0 | 40 | 0 | 0 to 1 | 0 |
| 1 | 1 | 2 | 2 | 40 |
| 2 | 64 | 3 | 3 to 4 | 41 |

Block chunk 2 is tx 1, chunk 0. The byte target `2 × chunk_size` is 64. Tx 2 covers bytes 41 to 104, so byte 64 is in tx 2. Tx 2's tx proof and its chunk-0 proof both verify. That selection is the wrong tx.

**Open items.** Same numbers as §14. Item 3 is closed (§15).

2. What a PoA carries: `(k, N, chunk_start)` explicitly, so a validator needs only the owning tx header; or derived from the block's tx headers (smaller PoA, more reads). With this rule, the option that verifies with only the owning tx header is safe, because the `tx_root` it checks against has passed full validation.
4. Where the code lives: `irys-data-tree`, which shares `T`, `climb`, `depth` and the tag registry, or a separate crate.
