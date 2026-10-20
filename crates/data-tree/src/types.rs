//! One newtype per hash so a caller cannot pass one where another is required (spec §2).

use core::fmt;

fn write_lower_hex(f: &mut fmt::Formatter<'_>, bytes: &[u8; 32]) -> fmt::Result {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0_u8; 64];
    for (i, byte) in bytes.iter().enumerate() {
        out[i * 2] = HEX[usize::from(byte >> 4)];
        out[i * 2 + 1] = HEX[usize::from(byte & 0x0f)];
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
        pub struct $name(pub [u8; 32]);

        impl $name {
            /// Raw digest bytes.
            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }
        }

        impl From<[u8; 32]> for $name {
            fn from(bytes: [u8; 32]) -> Self {
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
    /// There is no `From<NodeHash>`: a node hash is not a data-tree root until
    /// the chunk tree says so.
    DataTreeRoot
);

impl DataTreeRoot {
    pub(crate) const fn from_hash(hash: NodeHash) -> Self {
        Self(hash.0)
    }
}

hash_id!(
    /// `H(0x02 ‖ be64(data_size))` (spec §3).
    SizeWitness
);

hash_id!(
    /// Identity of a v2 upload: `node(data_tree_root, size_witness)` (spec §4).
    ///
    /// Verify a chunk or a segment only against an anchored commitment (spec §6).
    /// An anchored commitment is derived from a header whose signature was checked,
    /// or from a `tx_root` leaf that was proven. A commitment derived from an
    /// unauthenticated `(data_tree_root, data_size)` pair binds nothing: the last
    /// step of the proof only repeats that derivation, so the proof passes for any
    /// size whose path has the same shape.
    DataCommitment
);

hash_id!(
    /// `H(0x03 ‖ data_commitment ‖ be32(c))`. Not a tree node (spec §3, §7).
    ChunkKey
);
