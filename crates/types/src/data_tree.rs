//! Node-side data tree identifiers and the one seam to the `irys-data-tree`
//! crate.
//!
//! Geometry binding: a data transaction header signs `data_tree_root` and
//! `data_size`, not the geometry. The geometry of a transaction is the
//! consensus geometry in force at its inclusion height; `chain_id` (signed)
//! and that height fix it. A wrong geometry fails closed: the size witness
//! encodes the geometry, so the commitment differs and chunk verification
//! rejects. A future fork that changes `chunk_size` or `segments_per_chunk`
//! must refuse data transactions whose `anchor` block is below its activation
//! height, so a transaction signed for the old geometry never lands under the
//! new one.
//!
//! Each newtype wraps an [`H256`] and encodes byte-for-byte like it, so a
//! stored or signed value does not depend on which wrapper holds it. There is
//! no `From<H256>` and no `Deref`: a raw hash must be wrapped on purpose.

use crate::{H256, TxChunkOffset};
use alloy_primitives::bytes::BufMut;
use alloy_rlp::{Decodable, Encodable};
use arbitrary::Arbitrary;
/// Re-exported so node crates can name the geometry through `irys-types`.
/// A crate that matches on the tree crate's error variants depends on it
/// directly.
pub use irys_data_tree::Geometry;
use irys_data_tree::VerifyError;
use reth_codecs::Compact;
#[cfg(feature = "db")]
use reth_db_api::{
    DatabaseError,
    table::{Compress, Decode, Decompress, Encode},
};
use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! h256_newtype {
    ($(#[$meta:meta])* $name:ident, $crate_type:ty) => {
        $(#[$meta])*
        #[derive(
            Clone, Copy, Default, Debug, PartialEq, Eq, Hash, PartialOrd, Ord,
            Serialize, Deserialize, Arbitrary,
        )]
        #[serde(transparent)]
        pub struct $name(pub H256);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl From<$crate_type> for $name {
            fn from(value: $crate_type) -> Self {
                Self(H256::from(*value.as_bytes()))
            }
        }

        impl From<$name> for $crate_type {
            fn from(value: $name) -> Self {
                Self::from(value.0.0)
            }
        }

        impl Compact for $name {
            fn to_compact<B>(&self, buf: &mut B) -> usize
            where
                B: BufMut + AsMut<[u8]>,
            {
                self.0.to_compact(buf)
            }

            fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
                let (hash, remaining) = H256::from_compact(buf, len);
                (Self(hash), remaining)
            }
        }

        #[cfg(feature = "db")]
        impl Compress for $name {
            type Compressed = Vec<u8>;

            fn compress_to_buf<B: BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
                Compress::compress_to_buf(&self.0, buf);
            }
        }

        #[cfg(feature = "db")]
        impl Decompress for $name {
            fn decompress(value: &[u8]) -> Result<Self, DatabaseError> {
                <H256 as Decompress>::decompress(value).map(Self)
            }
        }

        #[cfg(feature = "db")]
        impl Encode for $name {
            type Encoded = <H256 as Encode>::Encoded;

            fn encode(self) -> Self::Encoded {
                Encode::encode(self.0)
            }
        }

        #[cfg(feature = "db")]
        impl Decode for $name {
            fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
                <H256 as Decode>::decode(value).map(Self)
            }
        }

        impl Encodable for $name {
            fn length(&self) -> usize {
                Encodable::length(&self.0)
            }

            fn encode(&self, out: &mut dyn BufMut) {
                Encodable::encode(&self.0, out);
            }
        }

        impl Decodable for $name {
            fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
                <H256 as Decodable>::decode(buf).map(Self)
            }
        }
    };
}

h256_newtype!(
    /// Merkle root of a data transaction's chunk tree. Signed in the header.
    DataTreeRoot,
    irys_data_tree::DataTreeRoot
);
h256_newtype!(
    /// Binds a [`DataTreeRoot`] to the `data_size` and geometry it was built for.
    /// Keys the data caches, and chunk storage.
    DataCommitment,
    irys_data_tree::DataCommitment
);
h256_newtype!(
    /// Identity of one chunk of one commitment.
    ChunkKey,
    irys_data_tree::ChunkKey
);

/// The commitment for `data_tree_root` and `data_size` under `geometry`.
pub fn data_commitment(
    geometry: &Geometry,
    data_tree_root: DataTreeRoot,
    data_size: u64,
) -> DataCommitment {
    irys_data_tree::data_commitment(geometry, &data_tree_root.into(), data_size).into()
}

/// The key of chunk `chunk_index` under `data_commitment`.
pub fn chunk_key(data_commitment: DataCommitment, chunk_index: TxChunkOffset) -> ChunkKey {
    irys_data_tree::chunk_key(&data_commitment.into(), chunk_index.0).into()
}

/// Checks that `bytes` is chunk `chunk_index` of the data under
/// `data_tree_root` and `data_size`, and that `proof` opens it.
///
/// Every data-chunk check and every check of a stored or received data path
/// goes through this function. Callers map the error to their own error type.
///
/// The commitment derived here is anchored only when the caller takes
/// `data_tree_root` and `data_size` from a signature-checked header (spec §6).
/// A pass on fields the caller has not anchored (for example the root and
/// size a peer sent with the chunk) is only a filter: an anchored check must
/// follow before the chunk is trusted.
pub fn verify_data_chunk(
    geometry: &Geometry,
    data_tree_root: DataTreeRoot,
    data_size: u64,
    chunk_index: TxChunkOffset,
    bytes: &[u8],
    proof: &[u8],
) -> Result<(), VerifyError> {
    irys_data_tree::verify_chunk(
        geometry,
        &data_commitment(geometry, data_tree_root, data_size).into(),
        data_size,
        chunk_index.0,
        bytes,
        proof,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use proptest::prelude::*;

    /// Every codec the node applies to a hash, as the encoded bytes.
    /// One helper per codec keeps each assertion readable.
    fn json_bytes<T: Serialize>(value: &T) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }

    fn compact_bytes<T: Compact>(value: &T) -> Vec<u8> {
        let mut buf = BytesMut::new();
        value.to_compact(&mut buf);
        buf.to_vec()
    }

    #[cfg(feature = "db")]
    fn compress_bytes<T: Compress>(value: T) -> Vec<u8> {
        Compress::compress(value).as_ref().to_vec()
    }

    #[cfg(feature = "db")]
    fn table_key_bytes<T: Encode>(value: T) -> Vec<u8> {
        Encode::encode(value).as_ref().to_vec()
    }

    fn rlp_bytes<T: Encodable>(value: &T) -> Vec<u8> {
        let mut buf = Vec::new();
        value.encode(&mut buf);
        buf
    }

    macro_rules! assert_encodes_like_h256 {
        ($newtype:expr, $hash:expr) => {
            assert_eq!(json_bytes(&$newtype), json_bytes(&$hash));
            assert_eq!(compact_bytes(&$newtype), compact_bytes(&$hash));
            #[cfg(feature = "db")]
            assert_eq!(compress_bytes($newtype), compress_bytes($hash));
            #[cfg(feature = "db")]
            assert_eq!(table_key_bytes($newtype), table_key_bytes($hash));
            assert_eq!(rlp_bytes(&$newtype), rlp_bytes(&$hash));
        };
    }

    #[test]
    fn every_encoding_equals_the_h256_encoding() {
        let hash = H256::random();
        assert_encodes_like_h256!(DataTreeRoot(hash), hash);
        assert_encodes_like_h256!(DataCommitment(hash), hash);
        assert_encodes_like_h256!(ChunkKey(hash), hash);
    }

    #[test]
    fn data_commitment_equals_the_crate() {
        let geometry = Geometry::TESTING;
        let root = DataTreeRoot(H256::random());
        let expected = irys_data_tree::data_commitment(&geometry, &root.into(), 96);
        assert_eq!(
            irys_data_tree::DataCommitment::from(data_commitment(&geometry, root, 96)),
            expected
        );
    }

    #[test]
    fn chunk_key_equals_the_crate() {
        let commitment = DataCommitment(H256::random());
        let expected = irys_data_tree::chunk_key(&commitment.into(), 3);
        assert_eq!(
            irys_data_tree::ChunkKey::from(chunk_key(commitment, TxChunkOffset(3))),
            expected
        );
    }

    #[test]
    fn seam_accepts_every_honest_chunk_and_rejects_one_changed_byte() {
        let config = crate::ConsensusConfig::testing();
        let geometry = config.data_tree_geometry().unwrap();
        let signer = crate::irys::IrysSigner::random_signer(&config);
        // Two full chunks and a short last chunk.
        let data_size = geometry.chunk_size() * 2 + 5;
        let data: Vec<u8> = (0..data_size).map(|byte| byte as u8).collect();
        let tx = signer.create_transaction(data, H256::zero()).unwrap();
        let header = &tx.header;
        let chunks = tx.data_chunks().unwrap();
        assert_eq!(chunks.len(), 3);

        for chunk in &chunks {
            let proof = tx.chunk_proof(chunk.tx_offset).unwrap();
            assert_eq!(
                verify_data_chunk(
                    &geometry,
                    header.data_tree_root,
                    header.data_size,
                    chunk.tx_offset,
                    &chunk.bytes.0,
                    &proof.0,
                ),
                Ok(()),
                "honest chunk {}",
                chunk.tx_offset
            );

            let mut changed = chunk.bytes.0.clone();
            changed[0] ^= 1;
            assert!(
                verify_data_chunk(
                    &geometry,
                    header.data_tree_root,
                    header.data_size,
                    chunk.tx_offset,
                    &changed,
                    &proof.0,
                )
                .is_err(),
                "changed byte in chunk {}",
                chunk.tx_offset
            );
        }
    }

    #[test]
    fn seam_returns_each_verify_error() {
        let config = crate::ConsensusConfig::testing();
        let geometry = config.data_tree_geometry().unwrap();
        let signer = crate::irys::IrysSigner::random_signer(&config);
        let data_size = geometry.chunk_size() * 2;
        // The chunks differ, so their proofs differ (identical chunks can
        // share a proof, spec §7).
        let data: Vec<u8> = (0..data_size).map(|byte| byte as u8).collect();
        let tx = signer.create_transaction(data, H256::zero()).unwrap();
        let root = tx.header.data_tree_root;
        let chunk = &tx.data_chunks().unwrap()[0];
        let proof = tx.chunk_proof(TxChunkOffset(0)).unwrap().0;
        let other_proof = tx.chunk_proof(TxChunkOffset(1)).unwrap().0;
        let verify = |chunk_index: u32, bytes: &[u8], proof: &[u8]| {
            verify_data_chunk(
                &geometry,
                root,
                data_size,
                TxChunkOffset(chunk_index),
                bytes,
                proof,
            )
        };

        assert!(matches!(
            verify(2, &chunk.bytes.0, &proof),
            Err(irys_data_tree::VerifyError::Size(
                irys_data_tree::SizeError::ChunkIndexOutOfRange { .. }
            ))
        ));
        assert_eq!(
            verify(0, &chunk.bytes.0[1..], &proof),
            Err(irys_data_tree::VerifyError::InvalidChunkLength {
                expected: geometry.chunk_size(),
                actual: geometry.chunk_size() - 1,
            })
        );
        assert!(matches!(
            verify(0, &chunk.bytes.0, &proof[1..]),
            Err(irys_data_tree::VerifyError::InvalidProofLength { .. })
        ));
        // Another size changes the size witness, so the honest proof fails.
        assert_eq!(
            verify_data_chunk(
                &geometry,
                root,
                data_size - 1,
                TxChunkOffset(0),
                &chunk.bytes.0,
                &proof,
            ),
            Err(irys_data_tree::VerifyError::SizeWitnessMismatch)
        );
        // A proof for another chunk does not open this one.
        assert_eq!(
            verify(0, &chunk.bytes.0, &other_proof),
            Err(irys_data_tree::VerifyError::CommitmentMismatch)
        );
        // Another root does not open this proof.
        assert_eq!(
            verify_data_chunk(
                &geometry,
                DataTreeRoot(H256::random()),
                data_size,
                TxChunkOffset(0),
                &chunk.bytes.0,
                &proof,
            ),
            Err(irys_data_tree::VerifyError::CommitmentMismatch)
        );
    }

    macro_rules! assert_round_trips_every_codec {
        ($newtype:ident, $bytes:expr) => {{
            let value = $newtype(H256($bytes));
            let from_json: $newtype = serde_json::from_slice(&json_bytes(&value)).unwrap();
            assert_eq!(from_json, value);

            let compact = compact_bytes(&value);
            let (from_compact, remaining) = $newtype::from_compact(&compact, compact.len());
            assert_eq!(from_compact, value);
            assert!(remaining.is_empty());

            let rlp = rlp_bytes(&value);
            assert_eq!(
                <$newtype as Decodable>::decode(&mut rlp.as_slice()).unwrap(),
                value
            );

            #[cfg(feature = "db")]
            {
                let compressed = compress_bytes(value);
                assert_eq!(
                    <$newtype as Decompress>::decompress(&compressed).unwrap(),
                    value
                );
                let key = table_key_bytes(value);
                assert_eq!(<$newtype as Decode>::decode(&key).unwrap(), value);
            }
        }};
    }

    proptest! {
        #[test]
        fn each_newtype_round_trips_every_codec(bytes in any::<[u8; 32]>()) {
            assert_round_trips_every_codec!(DataTreeRoot, bytes);
            assert_round_trips_every_codec!(DataCommitment, bytes);
            assert_round_trips_every_codec!(ChunkKey, bytes);
        }
    }
}
