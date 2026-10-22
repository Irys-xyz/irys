//! One newtype per hash (spec §2). The newtypes stop accidental mix-ups in
//! function signatures. They do not stop a deliberate conversion through raw
//! bytes, which is a decode, not a proof.

use core::fmt;

/// SHA-256 digest width in bytes (spec §3, §5). Proofs are a sequence of these.
pub const HASH_SIZE: usize = 32;

/// Characters in the lowercase hex form of one digest.
const HEX_LENGTH: usize = HASH_SIZE * 2;

fn write_lower_hex(f: &mut fmt::Formatter<'_>, bytes: &[u8; HASH_SIZE]) -> fmt::Result {
    const HEX_ALPHABET: &[u8] = b"0123456789abcdef";
    let mut out = [0_u8; HEX_LENGTH];
    for (byte_index, byte) in bytes.iter().enumerate() {
        // High nibble, then low nibble.
        out[byte_index * 2] = HEX_ALPHABET[usize::from(byte >> 4)];
        out[byte_index * 2 + 1] = HEX_ALPHABET[usize::from(byte & 0x0f)];
    }
    match core::str::from_utf8(&out) {
        Ok(text) => f.write_str(text),
        Err(_) => Err(fmt::Error),
    }
}

macro_rules! hash_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name([u8; HASH_SIZE]);

        impl $name {
            /// Raw digest bytes.
            pub const fn as_bytes(&self) -> &[u8; HASH_SIZE] {
                &self.0
            }
        }

        impl From<[u8; HASH_SIZE]> for $name {
            fn from(bytes: [u8; HASH_SIZE]) -> Self {
                Self(bytes)
            }
        }

        impl AsRef<[u8]> for $name {
            fn as_ref(&self) -> &[u8] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write_lower_hex(f, &self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, f)
            }
        }
    };
}

hash_id!(
    /// Hash of a leaf or an internal node (spec §2, §3).
    NodeHash
);

hash_id!(
    /// Root over the data only. The builder is what produces one (spec §2, §4).
    ///
    /// There is no `From<NodeHash>`. The newtypes stop accidental mix-ups in
    /// function signatures. They do not stop a deliberate conversion through
    /// raw bytes, which is a decode, not a proof.
    DataTreeRoot
);

impl DataTreeRoot {
    pub(crate) const fn from_hash(hash: NodeHash) -> Self {
        Self(*hash.as_bytes())
    }
}

hash_id!(
    /// Size and geometry witness (spec §3).
    ///
    /// `H(0x02 ‖ be64(data_size) ‖ be64(chunk_size) ‖ be16(segments_per_chunk))`.
    SizeWitness
);

hash_id!(
    /// Identity of a v2 upload: `node(data_tree_root, size_witness)` (spec §4).
    ///
    /// Verify a chunk or a segment only against an anchored commitment (spec §6).
    /// An anchored commitment is derived from a header whose signature was checked,
    /// or from a `tx_root` v2 leaf that was proven. A commitment derived from an
    /// unauthenticated `(data_tree_root, data_size)` pair under a chosen geometry
    /// binds nothing. The witness is not a secret. The attacker chooses the claimed
    /// size (smaller or larger) and the claimed index, and writes
    /// `size_witness(G, claimed n)` as the proof's last hash.
    ///
    /// The bytes that pass are always honest bytes of that tree. They are never
    /// bytes from outside it. The rules below are for that same geometry.
    ///
    /// A chunk proof for `(c, C)` passes as `(c', C')` against the re-derived
    /// commitment when, and only when, the left/right step sequence of `(c', C')`
    /// equals that of `(c, C)` at every level and `chunk_len(n', c')` equals
    /// `chunk_len(n, c)`.
    ///
    /// A segment proof passes as `(c', j)` against the re-derived commitment when
    /// the chunk-level left/right steps of `(c', C')` equal those of `(c, C)`,
    /// `segment_len(n', c', j)` equals `segment_len(n, c, j)`, and every in-chunk
    /// sibling at or past the claimed data-segment count is already `empty(ℓ)`.
    /// That count is `s' = ceil(chunk_len(n', c') / segment_size)`. At level `ℓ`
    /// the sibling subtree starts at `start = ((j >> ℓ) ^ 1) << ℓ`. That sibling
    /// is at or past the claimed data when `start ≥ s'`. `chunk_len(n', c')` need
    /// not equal `chunk_len(n, c)`.
    ///
    /// `node(data_tree_root, size_witness(G, claimed n))` equals the re-derived
    /// commitment, so the proof passes against it and fails against the anchored
    /// commitment. A proof that still carries the true witness fails earlier,
    /// before the climb: the last hash is not `size_witness(G, claimed n)`.
    ///
    /// Across geometries the commitment differs. `size_witness` binds `chunk_size`
    /// and `segments_per_chunk`, so `node(data_tree_root, size_witness(G', n'))`
    /// is not the commitment under `G`. Equal steps and an equal `chunk_len` are
    /// not the accept rule under `G'`.
    ///
    /// Test geometry, same geometry. Chunk proofs: `n = 96`, chunk 2, passes as
    /// chunk 1 of `n' = 64`; `n = 128`, chunk 0, passes as chunk 0 of `C' = 3`
    /// after the last hash is `size_witness(G, 96)`. Segment proof: `n = 56`,
    /// chunk 1, segment 0 (`chunk_len = 24`) passes as that segment for every
    /// `n'` in `49..=64` (`chunk_len` 17 to 32). `n'` in `33..=48` does not pass.
    DataCommitment
);

hash_id!(
    /// Chunk identity key. Not a tree node (spec §3, §7).
    ChunkKey
);
