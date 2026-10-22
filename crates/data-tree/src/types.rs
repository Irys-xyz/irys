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
    /// Size witness over `data_size` only (spec §3).
    SizeWitness
);

hash_id!(
    /// Identity of a v2 upload: `node(data_tree_root, size_witness)` (spec §4).
    ///
    /// Verify a chunk or a segment only against an anchored commitment (spec §6).
    /// An anchored commitment is derived from a header whose signature was checked,
    /// or from a `tx_root` leaf that was proven. A commitment derived from an
    /// unauthenticated `(data_tree_root, data_size)` pair binds nothing. The
    /// attacker chooses the claimed size (smaller or larger) and the claimed
    /// index. An honest proof for `(c, C)` passes as `(c', C')` when, and only
    /// when, the left/right step sequence matches at every level and `chunk_len`
    /// matches. The bytes that pass are an honest chunk of that tree.
    DataCommitment
);

hash_id!(
    /// Chunk identity key. Not a tree node (spec §3, §7).
    ChunkKey
);
