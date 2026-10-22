# Data Tree v2 Specification — ratified, implemented by this crate

This is the only binding document for the data tree and this crate. Everything here is implemented by this crate. Proposals live in [integration-proposal.md](integration-proposal.md). Designs that were considered and not adopted live in [designs-not-adopted.md](designs-not-adopted.md). The index is [README.md](README.md).

Code in this crate cites a section of this file as "spec §N".

Data tree v2 replaces the v1 `data_root` tree for uploaded data. It drops offset notes, adds 4 KiB segment leaves inside each 256 KiB chunk, and binds size and geometry into the commitment through `size_witness(G, n)`. §13 names the source of the printed values and of the shared vector files. §15 logs the decisions made.

Change this file in the same commit as the code that implements the change.

## 1. Summary

Chunks stay 256 KiB for storage and transfer. Segments of 4 KiB exist only inside the tree, for proofs. One tree gives chunk proofs, segment proofs and the size.

```
Example: 917,504 bytes = 3.5 chunks, so C = 4.

Commitment           data_commitment = node(data_tree_root, size_witness)
(binds data, size,          /                                  \
 and geometry)     data_tree_root                   size_witness(G, size) (§3)
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
| Size | implicit: the note of the rightmost leaf | `size_witness(G, data_size) = H(0x02 ‖ be64(data_size) ‖ be64(chunk_size) ‖ be16(segments_per_chunk))` (19 B), the last hash of every chunk proof and every segment proof. The verifier requires it to equal its own witness before the climb. The size check takes `(G, data_commitment, data_tree_root, data_size)` and no proof bytes |
| Root | `data_root` | `data_tree_root` (over the data) and `data_commitment = node(data_tree_root, size_witness)` |
| Sizes per root | many: txs can name one `data_root` with different sizes | one: a `data_commitment` fixes its size and its geometry |
| Chunk identity key | `H(data_path)` | `chunk_key = H(0x03 ‖ data_commitment ‖ be32(c))` |
| Malleable proof bytes | 24 per node (unchecked note padding) | none |
| Builder memory at 5 TiB | every proof built up front: about 51 GB | chunk roots and internal nodes: about 1.3 GB |
| Hashing cost | baseline | +11% to hash a chunk, +3% to check a chunk proof (§9) |

**Why.**

| Goal | Change that gives it | Effect |
| --- | --- | --- |
| Smaller proof-of-access and custody answers | segment leaves and segment proofs | an answer shrinks from a whole chunk plus its path (about 258 KiB) to segments plus paths (about 5 to 18 KB): less block space and gossip bandwidth |
| One size per root | the size witness in every chunk proof and every segment proof; the commitment covers data, size and geometry | deletes provisional sizes, chunk parking, rightmost-chunk confirmation and size re-checks. Two txs can no longer name the same root with different sizes |
| Canonical proofs | proofs of sibling hashes and the size witness, no notes | removes 24 malleable bytes per node; node and gateway verifiers accept exactly the same proofs; proofs compare byte for byte |
| Correct chunk identity | `chunk_key` by position | identical chunks at different offsets no longer collide, as keys made from proof hashes would |
| Shorter chunk proofs | 32 B per level instead of 96 B | a chunk proof at 5 TiB drops from 2,464 B to 832 B |
| Domain separation | one-byte tags | a leaf can never be read as a node or a size witness |
| A standard to test against | the RFC 6962 and RFC 9162 tree, proof and check (§4) | the gateway, SDKs and third parties implement a published algorithm; the shape equals today's `build_layer` |
| Lower builder memory | chunk roots and a node cache instead of every proof | about 51 GB down to about 1.3 GB at 5 TiB |

The price: about 11% more hashing per chunk and 3% per chunk-proof check (§9).

**Related documents.** Two separate proposals build on this tree: the segment-PoA spec (mining that hashes and proves single 4 KiB segments) and the data custody proposal (replica claims and challenges). This spec defines only the tree and its proofs. The data tx format is in integration-proposal.md §3. It names the other documents where a rule depends on them.

Tx header, chunk message, ingress, node-storage keys and the hardfork are not ratified — see integration-proposal.md §1.

## 2. Definitions

`H` is single SHA-256. `‖` is byte concatenation. `be64`, `be32` and `be16` are big-endian integers. Sizes are in bytes.

| Parameter | Mainnet and testnet | `testing()` | Rule |
| --- | --- | --- | --- |
| `chunk_size` | 262,144 | 32 | unchanged consensus field |
| `segments_per_chunk` (SPC) | 64 | 4 | NEW consensus config field: a power of two, 1 ≤ SPC ≤ 256 in the tree; consumers may require more. `chunk_size ≥ 1`, `chunk_size % SPC == 0`, `max_data_tx_chunks ≥ 1`, and `chunk_size × max_data_tx_chunks < 2^64` (no `u64` overflow), checked at config load |
| `segment_size` | 4,096 | 8 | `chunk_size / SPC` |
| `h` | 6 | 2 | `log2(SPC)`, the height of a chunk subtree |
| `max_data_tx_chunks` | 20,971,520 (`2^24 + 2^22`, 5 TiB) | same | unchanged |

The tree is a general primitive. A bound tighter than the rule above belongs to the consumer, not to the tree. The node's rejection of SPC < 2 is a proposal (integration-proposal.md §4, integration-proposal.md §10). It is not a rule of this tree.

| Name | Value |
| --- | --- |
| `data_tree_root` | the root over the data only (`T` below) |
| `size_witness` | `H(0x02 ‖ be64(data_size) ‖ be64(chunk_size) ‖ be16(segments_per_chunk))` (19 B). It binds size and geometry |
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

Every hash input starts with a one-byte domain tag. This table is the single registry of tag bytes. Tags `0x05` to `0x07` are reserved for the integration-proposal.md Appendix C proposal. They are not used by this tree. Their preimages are not binding. Any later tag set must avoid `0x00` to `0x07`.

| Tag | Name | Preimage | Use |
| --- | --- | --- | --- |
| `0x00` | LEAF | `0x00 ‖ segment bytes` (0 to `segment_size` bytes, exact length) | segment leaf |
| `0x01` | NODE | `0x01 ‖ left ‖ right` (65 B) | every internal node, chunk roots and the root |
| `0x02` | SIZE | `0x02 ‖ be64(data_size) ‖ be64(chunk_size) ‖ be16(segments_per_chunk)` (19 B) | size and geometry witness |
| `0x03` | CHUNK_KEY | `0x03 ‖ data_commitment ‖ be32(c)` (37 B) | chunk identity key; not part of the tree |
| `0x04` | TICKET | defined by the segment-PoA spec | mining ticket; not part of the tree |
| `0x05` | TX_LEAF | reserved for integration-proposal.md Appendix C proposal; not binding | not part of the data tree |
| `0x06` | TX_NODE | reserved for integration-proposal.md Appendix C proposal; not binding | not part of the data tree |
| `0x07` | TX_COUNT | reserved for integration-proposal.md Appendix C proposal; not binding | not part of the data tree |

TX_LEAF, TX_NODE and TX_COUNT are not used by this tree. The preimages are the integration-proposal.md Appendix C proposal. They are not binding.

```
leaf(bytes)     = H(0x00 ‖ bytes)
node(l, r)      = H(0x01 ‖ l ‖ r)
size_witness(G, n) = H(0x02 ‖ be64(n) ‖ be64(chunk_size) ‖ be16(segments_per_chunk))
empty(0)        = leaf("") = H(0x00)
empty(k)        = node(empty(k−1), empty(k−1))      // empty perfect subtree of height k
chunk_key(D, c) = H(0x03 ‖ D ‖ be32(c))              // D = data_commitment, c = chunk index
```

- Every preimage is fixed-width fields followed by at most one variable-length field, so no preimage can be read two ways.
- The size witness is 19 B. Every field is fixed width and there is no variable field, so distinct `(data_size, chunk_size, segments_per_chunk)` give distinct preimages. Tag `0x02` at 19 B matches no other row in this registry. A leaf of 18 data bytes is also 19 B and starts with `0x00`. `Geometry::new` requires `chunk_size` to fit in `u64` and `segments_per_chunk` ≤ 256, so `be64` and `be16` store both values with no truncation.
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
data_commitment = node(data_tree_root, size_witness(G, data_size))
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
              data_tree_root          size_witness(G, 917504)
              /            \
       node(R0, R1)     node(R2, R3)
        /      \         /      \
      R0       R1      R2       R3 = node(A, empty(5))
                                      /          \
                     A: tree over segments 0..=31  empty(5): segments 32..=63,
                        (data)                     all leaf("") = H(0x00)
```

**Example: 3.5 chunks.** `data_size = 917,504`, so `C = 4`. Chunks 0 to 2 are full. Chunk 3 holds segments 0 to 31; segments 32 to 63 are empty, so `R_3 = node(A, empty(5))`, where `A` is the tree over segments 0 to 31. Then `T = node(node(R_0, R_1), node(R_2, R_3))` and `data_commitment = node(T, size_witness(G, 917504))`.

**Non-power-of-two.** With `C = 5`, `T = node(node(node(R_0, R_1), node(R_2, R_3)), R_4)`. The chunk proof for `c = 4` has one sibling, then the size witness. The chunk proof for `c = 0` has three siblings, then the size witness.

**Building.** A streaming builder keeps a stack of at most `log2(C) + 1` subtree roots, plus the chunk roots. To serve proofs it caches every layer. Layer `l` stores `ceil(C / 2^l)` hashes, from the chunk roots (`l = 0`) through the single root. The stored count is `Σ_l ceil(C / 2^l)`, at most about `2C + log2(C)` hashes: 64 B per chunk in all (256 KiB per GiB). At 5 TiB that sum is 41,943,041 hashes, 1,342,177,312 bytes, about 1.3 GB. A carried node is copied into each layer it passes through, so a proof lookup is a plain index in that layer. `path()` below is the sibling list. The chunk proof is that list and then `size_witness(G, n)`. An implementation reads the siblings from the node cache and does not recompute subtrees:

```
path(c, R[0..C]):                         // siblings of chunk c, leaf to root (RFC 6962 audit path)
    if C == 1: return []
    k = largest power of two < C
    if c < k: return path(c, R[0..k]) ++ [T(R[k..C])]
    else:     return path(c − k, R[k..C]) ++ [T(R[0..k])]
```

- The chunk proof is `path(c) ++ [size_witness(G, n)]`.
- A segment proof is the `h` in-chunk siblings, built from the chunk bytes, then the chunk proof.
- `path` is empty when `C = 1`. The chunk proof is not: it is the size witness. The sibling count is `depth(c, C)` (§5). Do not read the proof length from the streaming builder's stack: after the final merges, a stack height is not the height of the subtree it holds.

## 5. Proofs and verification

A chunk proof or a segment proof is concatenated 32-byte hashes: the sibling hashes, leaf to root, then `size_witness(G, n)`. No header, no leaf hash, no notes. The order is leaf to root, so the witness is the last hash. Leaf to root is the default order for every proof in the protocol, including data-tree proofs and `tx_root` v2 proofs (integration-proposal.md Appendix C). Storage keeps a proof in the order it arrived. Why the order changes from v1: a v1 proof holds both children of every node plus an offset note, so a verifier can check it from the root down. A v2 proof holds only the sibling at each level, so the check must start at the leaf and climb, because a parent's hash cannot be computed before the child's. Leaf to root stores the hashes in the order the check uses them; root to leaf would store the same hashes reversed, and every verifier would read the list backwards. The verifier computes the exact length from `(data_size, c[, j])` and rejects any other length. It then computes `size_witness(G, n)` from the geometry and that `data_size` and requires the last hash to equal it. Only then does it climb the siblings. A supplied witness that was not checked must not be climbed: a false `data_size` would pass with the true witness attached.

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
| Chunk (`data_path`) | chunk-level siblings, then `size_witness(G, n)` | `depth(c, C) + 1`; the witness alone when `C = 1` |
| Segment | `h` in-chunk siblings (level 0 first), then the chunk proof | `h + depth(c, C) + 1` |
| Multi-segment (`m` segments of one chunk) | `m` independent segment proofs that share one chunk proof. Each segment carries its `h` in-chunk siblings. The shared chunk proof ends with `size_witness(G, n)`, and for `C = 1` it is that witness alone. A segment's full proof is its in-chunk siblings concatenated with the chunk proof. That concatenation is the full proof in every case, including one chunk | `m × h + depth(c, C) + 1` |
| Size check | `(G, data_commitment, data_tree_root, data_size)`; no proof bytes | none |

```
max = max_data_tx_chunks × chunk_size
A proof is a byte string of 32 × k bytes; proof[i] is its i-th 32-byte hash.

verify_chunk(G, D, n, c, bytes, proof):   // D: an anchored data_commitment (§6)
    require 1 ≤ n ≤ max; C = ceil(n / chunk_size); require c < C
    require len(bytes) == chunk_len(n, c)
    require len(proof) == 32 × (depth(c, C) + 1)
    w = size_witness(G, n)                 // computed here, from G and this n
    require the last hash of proof == w   // else stop; do not climb
    r = climb(chunk_root(bytes), c, C, proof without its last hash)
    require node(r, w) == D

verify_segment(G, D, n, c, j, bytes, proof):
    require 1 ≤ n ≤ max; C = ceil(n / chunk_size); require c < C and j < SPC
    require len(bytes) == segment_len(n, c, j)
    require len(proof) == 32 × (h + depth(c, C) + 1)
    w = size_witness(G, n)                 // computed here, from G and this n
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

- **Size check.** The inputs are `(G, data_commitment, data_tree_root, data_size)`. There are no proof bytes. The verifier computes `size_witness(G, data_size)`. `verify_size` takes any `u64` `data_size` as input and does not range-check it. The check is `node(data_tree_root, size_witness(G, data_size)) == data_commitment`. It fails the comparison unless `data_size` and the geometry are the committed ones.
- **Packed bytes:** unpack, then trim to `chunk_len` or `segment_len` before hashing. The segment-PoA spec also requires the trimmed tail to unpack to zero.
- **Empty in-chunk siblings.** At level `ℓ` the sibling subtree starts at `start = ((j >> ℓ) ^ 1) << ℓ`. Let `s = ceil(chunk_len(n, c) / segment_size)`. A segment at or past `s` is empty (`segment_len` is 0). If `start ≥ s`, that subtree is entirely past the data, and the sibling must be `empty(ℓ)`. `(n, c, j)` fixes `start` and `s` before the climb. An honest proof already carries `empty(ℓ)`. Any other hash at that sibling fails, the same way a bad climb fails. The pin does not open a sibling that is only partly past the data. A bad hash in an empty leaf of that mixed sibling leaves the other data segments verifying. Test geometry (`chunk_size` 32, SPC 4, `segment_size` 8), `n = 20`, `s = 3`: a hash other than `empty(0)` at leaf 3, segments 0 and 1 verify, and segments 2 and 3 fail. The rightmost data segment's path meets every empty leaf through a sibling that lies entirely past the data. That holds for every power-of-two `segments_per_chunk` in `1..=256` and every data-segment count. A hash other than `empty(0)` at an empty leaf therefore fails that proof. `verify_chunk` fails for every byte string of length `chunk_len`. `chunk_root` places `empty(k)` on every subtree that starts at or past the bytes. Rejecting the mixed sibling would reject an honest proof of a data segment whose sibling is only partly past the data.
- **No chunk or segment proof is empty.** Each ends with its size witness. For `C = 1` the chunk proof is `[size_witness(G, n)]`. A missing proof and a present proof differ in length, so a container does not have to tell an empty value from an absent one (§15 Q10). A last hash that is not `size_witness(G, n)` fails (`SizeWitnessMismatch`) before the climb.
- **Compact multiproof:** a possible future format, not the multi-segment encoding above. Its design is in designs-not-adopted.md Appendix B.

| Upload | `C` | Max depth | v1 chunk path | Chunk proof | Segment proof | 4 segments of one chunk (option B / compact form) |
| --- | --- | --- | --- | --- | --- | --- |
| ≤ 1 chunk | 1 | 0 | 64 | **32** | 224 | 800 / 544 |
| 1 GiB | 4,096 | 12 | 1,216 | **416** | 608 | 1,184 / 928 |
| 5 TiB (max) | 20,971,520 | 25 | 2,464 | **832** (768 for `c ≥ 2^24`) | 1,024 | 1,600 / 1,344 |

All sizes are in bytes and exclude the segment and chunk bytes. The last column is option B (`4 × h` in-chunk siblings plus one chunk proof), then a compact multiproof at most. The compact form is a possible future format, designed in designs-not-adopted.md Appendix B.

## 6. Trust anchor rule

Verify chunks and segments only against an **anchored** `data_commitment`. A `data_commitment` is anchored when it is derived from a signature-checked header. It is also anchored when it is derived from a `tx_root` v2 leaf proven against a `tx_root` that is itself anchored. That `tx_root` is taken from a signature-checked block header, or from a trust source the consumer names explicitly. Today's gateway names the relay quorum attestation (integration-proposal.md Appendix A.2). A leaf proven against a `tx_root` the verifier computed itself is the unauthenticated case. A commitment that a verifier derives from an unauthenticated `(data_tree_root, data_size)` pair under a chosen geometry binds nothing. The witness is not a secret. The attacker chooses the claimed size and the claimed chunk index, and writes `size_witness(G, claimed n)` as the proof's last hash. The claimed size may be smaller or larger than the size the tree was built for. The claimed index may differ from the honest index.

The bytes that pass are always honest bytes of that tree. They are never bytes from outside it.

Under the same geometry, a chunk proof for `(c, C)` passes as `(c', C')` against the re-derived commitment when, and only when, two checks hold. The left/right step sequence of `(c', C')` equals that of `(c, C)` at every level. `chunk_len(n', c')` equals `chunk_len(n, c)`. A step is the side `path` and `climb` give the sibling at that level, left or right (§4, §5).

Under the same geometry, a segment proof passes as `(c', j)` against the re-derived commitment when three checks hold. The chunk-level left/right steps of `(c', C')` equal those of `(c, C)`. `segment_len(n', c', j)` equals `segment_len(n, c, j)`. Every in-chunk sibling at or past the claimed data-segment count is already `empty(ℓ)`. `chunk_len(n', c')` need not equal `chunk_len(n, c)`. The claimed data-segment count is `s' = ceil(chunk_len(n', c') / segment_size)`. At level `ℓ` the sibling subtree starts at `start = ((j >> ℓ) ^ 1) << ℓ` (§5). That sibling is at or past the claimed data when `start ≥ s'`.

`node(data_tree_root, size_witness(G, claimed n))` equals the re-derived commitment, so the proof passes against it and fails against the anchored commitment. A proof that still carries the true witness fails earlier, before the climb: the last hash is not `size_witness(G, claimed n)`.

Across geometries the commitment differs. `size_witness` binds `chunk_size` and `segments_per_chunk` (§3), so `node(data_tree_root, size_witness(G', n'))` is not the commitment under `G`. Equal steps and an equal `chunk_len` are not the accept rule under `G'`.

| Honest chunk proof | Passes against the re-derived commitment as |
| --- | --- |
| `n = 96` (`C = 3`), chunk 2 | chunk 1 of `n' = 64` (`C' = 2`) |
| `n = 128` (`C = 4`), chunk 0, last hash `size_witness(G, 96)` | chunk 0 of `C' = 3` |

Test geometry, full chunks, same geometry. Both proofs fail against the anchored commitment.

| Honest segment proof | Passes against the re-derived commitment as |
| --- | --- |
| `n = 56` (`C = 2`), chunk 1, segment 0, `chunk_len = 24` | segment 0 of chunk 1 for every `n'` in `49..=64` (`C' = 2`). `chunk_len(n', 1)` is 17 to 32. `n'` in `33..=48` does not pass |

Test geometry, same geometry. The last chunk holds three segments (`s = 3`). The anchored commitment rejects each of those claims whose size is not 56.

The consumer anchor table is not ratified — see integration-proposal.md §2.

## 7. Chunk identity and keys

**Chunk identity.** A validated chunk is identified by `chunk_key(data_commitment, c)`, not by `H(data_path)`. Identical chunks at different indices of one upload (all-zero data is common) can have identical proofs, so a proof cannot be an identity. Table and type names (decided): `ChunkPathHash` → `ChunkKey`, `ChunkDataPathByPathHash` → `ChunkDataPathByChunkKey`, `ChunkPathHashesByOffset` → `ChunkKeysByOffset`, field `data_path_hash` → `chunk_key`, and `GossipCacheKey::Chunk` keyed by `ChunkKey`.

**Keys.** Everything keyed by data uses `data_commitment`, never `data_tree_root`: `CachedDataRoot` rows, data-root records in storage modules, parked and pending chunks, the root field of `UnpackedChunk`, and the chunk-by-root API route. Two txs with the same `data_tree_root` and different sizes then never share a row, so the multi-size state that v2 deletes cannot come back. `data_commitment` and `chunk_key` bind the geometry. Two geometries that split the same bytes do not share a key. A change of `chunk_size` or `segments_per_chunk` does not alias those rows, so that change needs no reset and no re-key.

The header and chunk-message format are not ratified — see integration-proposal.md §3.

## 8. Consensus rules on L1

Not ratified — see integration-proposal.md §4.

## 9. Cost

v2 adds about 11% to hashing a chunk (tx creation) and about 3% to verifying a chunk proof. About 11% is this table's +10.6%. About 3% is this table's +2.61%. This answers the review question "how much time and compute does it add to tx creation and chunk verification?". Both formats spend almost all their time hashing the chunk bytes once. v2 hashes them as 64 segments of 4 KiB instead of one 256 KiB input.

Rates measured with openssl speed -evp sha256 -bytes N on one core of an Intel i9-13900HK with SHA-NI: 1.82 GB/s on 256 KiB inputs, 1.75 GB/s on 4,097-byte inputs, about 0.15 µs per 65-byte node. 1 GB is 10^9 bytes. The sums use 0.15 µs for every small hash. That rate was measured on a 65-byte node. v1 applies it to each 32-byte hash and each 96-byte hash: a v1 node is four hashes, three of 32 bytes and one of 96 bytes (§10). The 19-byte size witness uses the same rate. That is an assumption of this model.

Each time and each change has three significant figures. A change is the ratio or the difference of the unrounded sums. A v2 proof ends with the size witness, and the verifier hashes that witness (§5). The verify rows include it.

| Operation | v1 | v2 | Change |
| --- | --- | --- | --- |
| Hash one chunk (build or verify) | 0.144 ms: one 262,144-byte hash | 0.159 ms: 64 hashes of 4,097 bytes + 63 nodes | +10.6% |
| Verify one chunk proof at 5 TiB (depth 25) | 0.159 ms: chunk hash + 25 × 4 small hashes | 0.163 ms: chunk hashes + 25 nodes + the size witness | +2.61% |
| Build the tree of a 1 GiB upload, one core | 0.590 s: 4,096 × the unrounded chunk time | 0.652 s: 4,096 × the unrounded chunk time | +0.0625 s |
| Verify one segment proof at 5 TiB | — | 7.29 µs: one 4,097-byte leaf + 33 small hashes | new |
| Builder memory, 1 GiB | every proof up front: 4.98 MB | chunk roots and nodes: 256 KiB | lower |
| Builder memory, 5 TiB | every proof up front: 50.9 GB | chunk roots and nodes: 1.34 GB, proofs on demand | lower |

The 33 small hashes are 6 in-chunk nodes, 25 climb nodes, the size witness, and `node(r, w)`. A chunk-verify sum that stops after the 25 climb nodes also rounds to 0.163 ms. Adding `node(r, w)` still rounds that time to 0.163 ms. §5 hashes the size witness once. `data_commitment` hashes that witness again before `node(r, w)`, so a measurement of `verify_segment` is 7.44 µs under the same rate.

Memory uses the same three significant figures. 1 MB is 10^6 bytes and 1 GB is 10^9 bytes. v1 stores every proof at 64 + 96 × depth bytes. 1 GiB is 4,096 × 1,216 = 4,980,736 bytes, 4.98 MB. 5 TiB is 2^24 × 2,464 + 2^22 × 2,272 = 50,868,518,912 bytes, 50.9 GB. v2 stores the chunk roots and the nodes at 32 bytes each, and builds proofs on demand. 1 GiB uses the §4 bound of 64 bytes per chunk: 4,096 × 64 = 262,144 bytes, 256 KiB. 5 TiB is the §4 sum, 41,943,041 × 32 = 1,342,177,312 bytes, 1.34 GB.

- Chunk roots are independent, so tree building parallelizes per chunk, as v1's leaf hashes do.
- The empty-segment hashes of a last chunk come from a fixed table of `h + 1` values.
- Other tx-creation steps (signing, upload) do not change.

## 10. Security properties

| Property | How it holds |
| --- | --- |
| Positional binding | The shape is a function of `(data_size, chunk_size, SPC)`. The verifier derives every direction bit from `(c, j)` and requires the exact proof length. Known attacks on RFC 6962-style proofs (an inner node shown as a leaf, or a proof checked against another leaf count) need a verifier that does not know the leaf count; here `C` is fixed before any hashing. A proof for position `i` shown as position `k` fails when the bytes differ. When the bytes match it can still fail, because the verifier also requires the depth and the left/right steps of `(k, C)`. Identical chunks can share a proof (§7), so identity is `chunk_key` |
| Size and geometry binding | The verifier computes `size_witness(G, data_size)` itself and requires the proof's last hash to equal it, before the climb (§5). It then checks `node(root, size_witness(G, data_size))` against an anchored commitment (§6). A wrong size fails. A wrong geometry fails the same way. A proof that carries the true witness fails the witness check even when a re-derived commitment would match that witness. The preimage is 19 B and every field is fixed width: `0x02 ‖ be64(data_size) ‖ be64(chunk_size) ‖ be16(segments_per_chunk)`. There is no variable field, so distinct `(data_size, chunk_size, segments_per_chunk)` give distinct preimages. Tag `0x02` at 19 B matches no other registered preimage. `Geometry::new` requires `chunk_size` to fit in `u64` and `segments_per_chunk` ≤ 256, which fits `be16`, so neither field is truncated |
| Domain separation | Distinct tags and fixed widths (§3). A 64-byte segment equal to `l ‖ r` hashes as a leaf, never as `node(l, r)`. A v2 `data_commitment` and a v1 root can collide only by a SHA-256 collision. A bare `data_tree_root` can equal a v1 leaf or branch id only for a one-leaf tree (SPC = 1, C = 1) of 63 or 95 bytes, found by about 2^8 tries. Key by the commitment, never the bare root (§7) |
| One hash per node | v1 hashes each part of a node first, then the concatenation (4 SHA-256 calls per node). v2 hashes once: H(0x01 ‖ l ‖ r), the RFC 6962 construction. The inner hashes add no security: l and r are already 32-byte hashes, and forging any proof still needs a SHA-256 collision at the first node where the honest and forged inputs differ (about 2^128 work). Fixed widths make 0x01 ‖ l ‖ r readable only one way, and the tags replace v1's leaf and node separation by part count. Length extension does not apply: the tree has no secrets, and every node input is fixed-width |
| No malleability | A proof is the sibling hashes and then the size witness. The witness must equal the verifier's own `size_witness(G, data_size)`. For a fixed `(data_commitment, G, data_size, c[, j])` the valid proof is unique up to SHA-256 collisions |
| Padding | Segments hash at their true length. `n` bytes and `n` bytes plus zeros differ in a leaf and in the size witness. Packed storage pads with entropy; verifiers trim before hashing |
| Identical data | Identity is `chunk_key`, never proof bytes (§7) |
| Denial of service | Length checks run before any hashing. Work per proof is bounded. |
| Determinism | Integers and bytes only, checked arithmetic, no map iteration order |
| Availability | Not proven by any root. A dishonest builder can commit to a tree with no preimage for some chunks. A segment proof pins an in-chunk sibling to `empty(ℓ)` only when that subtree lies entirely past the data (§5). A bad hash in an empty leaf of a mixed sibling does not fail every data segment. Test geometry, `n = 20`: segments 0 and 1 verify, and segments 2 and 3 fail. The rightmost data segment's proof fails. `verify_chunk` fails for every byte string of length `chunk_len` |

The peer-fault rule for a wrong-length chunk is not ratified — see integration-proposal.md §5.

## 11. Rollout

Not ratified — see integration-proposal.md §6.

## 12. Implementation map

Not ratified — see integration-proposal.md §7.

## 13. Test vectors

The printed `data_tree_root` values and the sibling hashes came from an independent Python reference of the tree. That reference uses the Python standard library only. It is not in this repo. The printed `size_witness`, `data_commitment` and `chunk_key` lines came from a second independent Python reference of `size_witness(G, n)`. That reference uses the Python standard library only. It is not in this repo. The generator matches both references byte for byte. Proof order is leaf to root (§5). The size witness binds size and geometry: `H(0x02 ‖ be64(data_size) ‖ be64(chunk_size) ‖ be16(segments_per_chunk))` (19 B, §3). The four size-witness lines below use mainnet geometry. The tree reference also checked:

- the streaming builder against the recursive definition for every `C` from 1 to 299;
- that every chunk proof and segment proof verifies, for every `data_size` from 1 to 199 in test geometry;
- these negative cases: wrong size with the same and with another `C`, `c ≥ C`, a proof one hash too long and too short, a flipped sibling bit, a zero-padded segment, a 64 B segment equal to `l ‖ r`, and an unanchored commitment;
- the depths 25 and 23 at the maximum size.

Data is `b[i] = i mod 251`. Proofs are leaf to root and end with `size_witness(G, data_size)`.

```
leaf("")              6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d
empty(6)  (SPC 64)    2be3cf0551cc6936d461e3dc43f3c4bf50cbee1bc091925254e879f4e7665e94
size_witness(G, 1)       e677df7f858cd910622cfd5c6ac7bbf6200e7829c3e4a7325590a6b6309c0597
size_witness(G, 4096)    c0c06f04b271e626a424dad90c9ad30189e210d4b9690f231d9147cdeca448f0
size_witness(G, 262144)  86b2be42530761806437d0632cfb93c56f9294c8cc1c8776ad01d08b3078fa49
size_witness(G, 917504)  438a131aaf3445c16ab8b8f66374f8ec253f8bce7acc9efd91f722e2409ef89e
```

Test geometry (`chunk_size` 32, SPC 4, `segment_size` 8):

```
n    C  data_tree_root                                                    data_commitment
1    1  8521b87ff724236722cfb6d6306ad59bceb4aee3e35ae73c41326cd31915e13e  594c8b3b9cff69e0b5dd50c7465a7b2f7dc722d445f0ac2952d135c04b79f2b6
8    1  2239889b93ac82ead2db9605f2584a25255c90206c1fccaddf54caa75b7fc1a1  6e9c94693cb2fea535b79bb9938982db61ff23ade48119a60340e9a680423469
9    1  3ba2878285288e004dcd35a73e9a3df4a19f5fe5acccaf8d84b50fc7254e6ddb  245a5af1aa8e606cb4e8878d93449d7872f68de9bb8b1db696c13ea81c393728
32   1  d3e81f198d6bd3782d1941172c89e49721d8f5e33cc709d4f6ba3f54b9e35f82  aa774e6e5029925d0c43c6f019c56222cb4f220bad6626bb37bdf17180f5d6ea
33   2  bf98e27c471bfda3de1d44b4eb420f528238aa75e6e002fe1636c04a431713a9  074db04f4bc2e5cf84f47d2772cf100c0c09d372e15c28264f9edf9187645d00
112  4  7fd698c547deb0bbb8f8ab7db08dd34616a496f141167252487509cb74824564  d3175b025ae584b19f464af849bff465ecf9b9a2c83376b947b5681d82cca1c4
157  5  81e7a996cd2ba141815aa48a1afe4f9c3d5c0ca450f12c9a605c20a0c3037c95  8eb9e426d13ab02def39117e53b838110f8cebfdfb7d0b9c1726e05001c00e0d

The tree reference printed the sibling hashes. The last line of each proof is size_witness(G, 112) under test geometry:
  80a98067aa92e3bf32b45c5afa5ff2bf38640d51a0e4e338b938bf735f99b0e9
n = 112, chunk proof c = 2:
  dbc3c2433981a503f8ffa38629a9ec5d41501618b2afd860af52d6a93e06e8b6
  50cb073b3d437e037c34dc0d757489f56fd63b9122dd4edfbe8bc42812a5bd84
  80a98067aa92e3bf32b45c5afa5ff2bf38640d51a0e4e338b938bf735f99b0e9
n = 112, segment proof (c = 3, j = 1):
  82bdff7cd7950d248431b84dd588f1777777ee141ebc86ac8f72080aaf11e0e5
  fe43d66afa4a9a5c4f9c9da89f4ffb52635c8f342e7ffb731d68e36c5982072a
  f8e87ba8c906d8391fb1182e6c1e7d123392e8770d510b547edb3bbffe7cfb65
  50cb073b3d437e037c34dc0d757489f56fd63b9122dd4edfbe8bc42812a5bd84
  80a98067aa92e3bf32b45c5afa5ff2bf38640d51a0e4e338b938bf735f99b0e9
n = 112, chunk_key(c = 3):
  91ff543f909c38411b53d65ef72e411ed81721af90f8f88f6b49450688b32674
```

Mainnet geometry (`chunk_size` 262,144, SPC 64):

```
n        C  data_tree_root                                                    data_commitment
1        1  2438dc9c444ab818a41197dfd3aa6502bc3e2fb03895b7ed79ab275539b7972b  9029711cdb35496538bd4e91ac809a8abe2e3bb94376d44595e097478a4a8698
4095     1  851d7b0214e05f85e581de8dd408f5a4b0c31d8baad3f42986f056949ee1d575  267ff99c8c7c3580124a730ee7e7c28781d3969a5e52ea164a8fc5fcfa78416b
4096     1  be58572dcfba33df3201cdbeda2799dd0c5b508a0df41a47912cb008896f9195  7f3301e487d66e71bbb39ab1ae67d6c3ce37fee25eab9daf8259a3713cd4d47c
4097     1  f516d84903ce88d9d1d80d4071ce945dc53707bd4ed60346a3ba6392412c9134  3262c546bc4db11e05171d083d33fef98a36ddbb18f56ee64fcc2b5f3bb0aade
262143   1  b6dd65dde53181e3443d128d05b7cbf57c5f4352caa8216ed365d716896335bd  5ab3d70ba6f6ef25d19d456d9d4bfa57ed26796d10339b4ba95f34bfd41d0f83
262144   1  8e2d2d85eb5fd8e66d4f4b874b5ad6b5aa46cad790fbe64f61c374d8f8243d5f  774bddbe62aae051508341d9291b1533eda68d8f95f034c6905de097aa185730
262145   2  dcc4ac982c84bdf0d6df5c16d2728b65bb4bae355b078b5a22e47a6a3e518825  ff03453747ac366f182de34614fef6816feda7428fc7d5b00f931561eaa8fc04
917504   4  3d86c811acf3356576cbe00b28814ad89ee42bd5aa36b755cea977620366de67  683b214b4775ab4fbe5baa64cf193f50a1ecb06ea3a90f3b3e85a60639c3c91f
1310720  5  1d5501730df0426a6f2b223d3fc31703d5eb6e64bae166736c920ccd23594ac0  36c380c4cb361fe958b443d27656130e5f57e58b9fd8c80af9a0130a390b83ab
```

**Full shared set** (integration-proposal.md §6, step 2). The shared vector files under `crates/data-tree/vectors/` are generated by `examples/gen_vectors.rs` (`cargo run -p irys-data-tree --example gen_vectors -- <out_dir>`). `tests/vectors.rs` checks them. The directory holds `chunk_proofs.json`, `multi_segment.json`, `negative.json`, `primitives.json`, `roots_mainnet.json`, `roots_testing.json`, and `segment_proofs.json`.

| Group | Cases |
| --- | --- |
| Primitives | `leaf` of 0, 1, 4,095 and 4,096 B; `node`; `size_witness` of 1, 4,096, 262,144, 917,504, `2^40` and 5 TiB; `empty(0..=8)`; `chunk_key` at `c` = 0, 1, `2^24`, and the printed `c` = 3 |
| Roots, mainnet geometry | `data_size` 1; 4,095; 4,096; 4,097; 262,143; 262,144; 262,145; 524,288; 786,432; 917,504; 1,310,720; `C` = 8, 9, 16, 17; 1 GiB; 1 GiB + 1. Each with `i mod 251` bytes and with all-zero bytes. Output: chunk roots, `data_tree_root`, `size_witness`, `data_commitment` |
| Roots, test geometry | `chunk_size` 32, SPC 4: `data_size` 1, 8, 9, 32, 33, 112, 157 |
| Chunk proofs | every chunk for `C ≤ 17`; chunks 0, 2,047 and 4,095 of 1 GiB; `C = 5`, `c = 4` (one sibling, then the size witness); every chunk of the printed testing upload `n` = 112 |
| Segment proofs | every segment for `C ≤ 2`; for 917,504: `(3, 5)`, `(3, 31)` (last full), `(3, 32)` (first empty), `(3, 63)`; the partial segment `(0, 1)` of 4,097 B; the printed testing proof `(n = 112, c = 3, j = 1)` |
| Multi-segment | option B. In one chunk, `m` = 1, 2, 4 and 64: each case is `m` segment proofs (the `h` in-chunk siblings each) and one shared chunk proof. The full segment proof is those siblings concatenated with the chunk proof, including when `C = 1`. An adjacent pair in one chunk. A pair in two chunks, which is two chunk proofs |
| Negative | wrong `data_size` with the same `C` and with another `C`; `c ≥ C`; proof one hash too long and too short; empty proof for `C > 1`; chunk length ±1; one flipped bit per sibling; a zero-padded segment; `data_size = 0`; a valid proof shown at another index with different bytes; a 64 B segment equal to `l ‖ r`; a commitment re-derived from an unauthenticated pair (§6) |

The receipt, `tx_root` v2 and wire groups are not ratified — see integration-proposal.md §8.

## 14. Open decisions

Not ratified — see integration-proposal.md §9. The tree-salt item is in designs-not-adopted.md Appendix D.

## 15. Decision log

The log is append-only. Each table keeps the order in which its rows were added. A new row goes at the end of that table. Dates are not re-sorted. A row is not reworded. The first table binds the data tree. The second table is integration planning.

**Decided — the data tree (binding).**

| Date | Decision |
| --- | --- |
| 2026-10-21 | Q1. Proof order is leaf to root, and leaf to root is the default order for every proof in the protocol. Compression is not a reason to pick an order: the measured gap comes from where the constant row bytes sit, not from the order (Appendix A.1) |
| 2026-10-21 | Q2. A multi-segment answer is independent segment proofs that share one chunk proof. A compact multiproof saves about 1.4% and needs a second consensus verifier, so it is not this format. It can be a later format if custody needs it (design: designs-not-adopted.md Appendix B) |
| 2026-10-21 | Q5. The size witness covers `data_size` only: `H(0x02 ‖ be64(data_size))`. Geometry is already on the consensus surface a client needs before it can verify, so the commitment does not repeat it. Superseded 2026-10-22 |
| 2026-10-21 | Q10. Every proof ends with `size_witness(n)`, leaf to root, so the witness is the last hash. A chunk proof is the chunk-level siblings then the witness: `32 × (depth(c, C) + 1)` bytes, and for `C = 1` it is `[size_witness(n)]`. A segment proof is the `h` in-chunk siblings then that chunk proof. Plain concatenation is the full segment proof in every case, including one chunk. The verifier checks the length, requires the last hash to equal `size_witness` computed from the `data_size` it is checking, and does not climb until that matches. It then climbs the siblings and requires `node(root, size_witness(n))` to equal the commitment. A sibling-only proof of a one-leaf tree is empty, and containers that store or sign proofs confuse empty with missing: a header binding with empty lists becomes absent, nested inner proofs are skipped when empty, and `ChunkWithProof.proof` defaults to empty. Witness-last costs 32 B per proof on the wire. In a storage-module row the witness is the same for every chunk of an upload, so LZ4 shares it. Rejected: leaf-first for every proof, which adds 32 B per proof and per storage row, and the leaf differs per row so LZ4 cannot share it; and a leaf only when the proof would otherwise be empty, which is a special-case branch. Formula superseded 2026-10-22, see the size-witness formula row |
| 2026-10-19 | Tag registry `0x00` to `0x04` (`0x04` is the mining ticket) and `segments_per_chunk ≤ 256` (segment-PoA spec, ticket encoding) |
| 2026-10-18 | Chunk identity is `chunk_key(data_commitment, c)`, with the table and type names in §7 |
| 2026-10-18 | `segments_per_chunk` is a validated consensus config field (64 on mainnet and testnet, 4 in `testing()`) |
| 2026-10-18 | Names: `data_tree_root`, `size_witness`, `data_commitment`; `data_root` means v1 only; a newtype per value |
| 2026-10-18 | Witness layout: `data_commitment = node(data_tree_root, size_witness)`, the size witness as the root's right child |
| 2026-10-22 | A segment proof pins each in-chunk sibling whose subtree lies entirely past the data to `empty(ℓ)` (§5). The subtree starts at `start = ((j >> ℓ) ^ 1) << ℓ`. It is pinned when `start ≥ ceil(chunk_len(n, c) / segment_size)`. Those indexes follow from `(n, c, j)` before the climb. Honest proofs already carry `empty(ℓ)` there. Without the pin, a last chunk can put another hash in that position: every data segment of the chunk verifies, and no bytes pass `verify_chunk`. Segment proofs would then miss a chunk with no preimage |
| 2026-10-22 | The size witness binds size and geometry. The name stays `size_witness`. `size_witness(G, n) = H(0x02 ‖ be64(data_size) ‖ be64(chunk_size) ‖ be16(segments_per_chunk))` (19 B). Every field is fixed width and the total length is fixed, so distinct `(data_size, chunk_size, segments_per_chunk)` give distinct preimages. `data_commitment = node(data_tree_root, size_witness(G, n))`. A commitment and a `chunk_key` are specific to one geometry. Two geometries that split the same bytes do not share a key. A change of `chunk_size` or `segments_per_chunk` does not alias those keys, so that change needs no reset and no re-key. Geometry stays on the consensus surface a client already has. The witness repeats it, so a commitment describes its own geometry. Supersedes Q5. |
| 2026-10-22 | The empty-sibling pin does not fail every data segment of a malformed last chunk. It applies only to a sibling whose subtree lies entirely past the data (§5). A bad hash in an empty leaf of a mixed sibling is not opened. Other data segments of that chunk still verify. Test geometry, `n = 20`, SPC 4: segments 0 and 1 verify, and segments 2 and 3 fail. The rightmost data segment's path meets every empty leaf, for every power-of-two `segments_per_chunk` in `1..=256` and every data-segment count, so that proof fails. `verify_chunk` fails for every byte string of length `chunk_len`. Rejecting the mixed sibling would reject an honest proof. The 2026-10-22 empty-sibling row's "every data segment verifies" is the tree with the pin removed. It is not the result of this check |
| 2026-10-22 | Size-witness formula. The witness in Q10 and in the 2026-10-21 tx_root row is `size_witness(G, n)` (19 B). See the row that supersedes Q5. The verifier computes it from `G` and `n`. It checks `node(root, size_witness(G, n)) == data_commitment`. This supersedes those two formulas. Q10's witness-last rule stays |

Status: binding. The leaf rules in the next sentence are part of the confirmed data tree. They are not a separate log row.

Proposed with the approved design, not decided on their own: the last chunk is a perfect subtree with empty leaves, and the chunk level keeps today's RFC 6962 shape.

The planned decision log is not ratified — see integration-proposal.md §10.

## Appendix A. Measurements

### A.1 Proof order and RocksDB compression

This measures the stored size of leaf-to-root and root-to-leaf chunk proofs under LZ4, and the effect of the row trailer.

**Rows.** 65,536 proofs, mainnet geometry (`chunk_size` 262,144, SPC 64). The one-upload case is one 16 GiB upload. Every proof has depth 16. The measured sibling list, without the size witness, is 512 bytes. A raw row is that proof plus a 6-byte trailer: 518 bytes. Value-only LZ4 stores that row at about 80 bytes.

These measurements used proofs without the size witness (§15 Q10). The witness is identical for every row of an upload, so LZ4 shares it. The tables were not re-run.

**Production settings, as of this writing.** `ChunkMetaByOffset` uses `RocksTuning::baseline` (`crates/database/src/submodule/rocks.rs:172-192`) through `column_options` (`rocks.rs:3617-3696`). Compression is LZ4 (`rocks.rs:3646-3658`). The block size is `BLOCK_BYTES`, 64 KiB (`rocks.rs:60`). `encode_offset_meta` (`crates/database/src/submodule/records.rs:30-54`) writes a non-empty proof, then the CRC little-endian when present (`records.rs:48-50`), then the flags byte (`records.rs:52`). `HAS_PROOF | CRC_PRESENT` is `0x03` (`records.rs:16-18`). The format byte is `1` (`records.rs:14`, `records.rs:53`). An empty proof returns an error (`records.rs:37-39`).

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

**Conclusion.** There is no storage reason to prefer an order. Place the constant row bytes next to the shared run. Leaf to root is the protocol default (§5), so that run is a suffix: the flags and the format byte go directly after the proof, and the CRC goes last (integration-proposal.md §7).

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

### A.2 Gateway receipt chain

Not ratified — see integration-proposal.md Appendix A.2.
