use irys_types::{
    ingress::IngressProof, partition::PartitionHash, ChunkPathHash, DataRoot, IrysBlockHeader,
    IrysTransactionHeader, H256,
};
use reth_codecs::Compact;
use reth_db::{table::DupSort, tables, DatabaseError};
use reth_db::{HasName, HasTableType, TableType, TableViewer};
use reth_db_api::table::{Compress, Decompress};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::submodule::tables::RelativeStartOffsets;
use crate::{
    db_cache::{CachedChunk, CachedChunkIndexEntry, CachedDataRoot},
    submodule::tables::{ChunkOffsets, ChunkPathHashes},
};

/// Adds wrapper structs for some primitive types so they can use `StructFlags` from Compact, when
/// used as pure table values.
#[macro_export]
macro_rules! add_wrapper_struct {
	($(($name:tt, $wrapper:tt)),+) => {
			$(
					/// Wrapper struct so it can use StructFlags from Compact, when used as pure table values.
					#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, Compact)]
					#[derive(arbitrary::Arbitrary)] //#[add_arbitrary_tests(compact)]
					pub struct $wrapper(pub $name);

					impl From<$name> for $wrapper {
							fn from(value: $name) -> Self {
									$wrapper(value)
							}
					}

					impl From<$wrapper> for $name {
							fn from(value: $wrapper) -> Self {
									value.0
							}
					}

					impl std::ops::Deref for $wrapper {
							type Target = $name;

							fn deref(&self) -> &Self::Target {
									&self.0
							}
					}

			)+
	};
}

#[macro_export]
macro_rules! impl_compression_for_compact {
	($($name:tt),+) => {
			$(
					impl Compress for $name {
							type Compressed = Vec<u8>;

							fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(self, buf: &mut B) {
									let _ = Compact::to_compact(&self, buf);
							}
					}

					impl Decompress for $name {
							fn decompress(value: &[u8]) -> Result<$name, DatabaseError> {
									let (obj, _) = Compact::from_compact(value, value.len());
									Ok(obj)
							}
					}
			)+
	};
}

add_wrapper_struct!((IrysBlockHeader, CompactIrysBlockHeader));
add_wrapper_struct!((IrysTransactionHeader, CompactTxHeader));

impl_compression_for_compact!(
    CompactIrysBlockHeader,
    CompactTxHeader,
    CachedDataRoot,
    CachedChunkIndexEntry,
    CachedChunk,
    ChunkOffsets,
    ChunkPathHashes,
    PartitionHashes,
    RelativeStartOffsets
);

tables! {
    IrysTables;
    /// Stores the header hashes belonging to the canonical chain.
    table IrysBlockHeaders<Key = H256, Value = CompactIrysBlockHeader>;

    /// Stores the tx header headers that have been confirmed
    table IrysTxHeaders<Key = H256, Value = CompactTxHeader>;

    /// Indexes the DataRoots currently in the cache
    table CachedDataRoots<Key = DataRoot, Value = CachedDataRoot>;

    /// Index mapping a data root to a set of ordered-by-index index entries, which contain the chunk path hash ('chunk id')
    table CachedChunksIndex<Key = DataRoot, Value = CachedChunkIndexEntry, SubKey = u32>;

    /// Table mapping a chunk path hash to a cached chunk (with data)
    table CachedChunks<Key =ChunkPathHash , Value = CachedChunk>;

    /// Indexes Ingress proofs by their data_root
    table IngressProofs<Key = DataRoot, Value = IngressProof>;

    /// Maps a data root to the partition hashes that store it. Primarily used for chunk ingress.
    /// Common case is a 1:1, but 1:N is possible
    table PartitionHashesByDataRoot<Key = DataRoot, Value = PartitionHashes>;


}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, Compact)]
/// partition hashes
/// TODO: use a custom Compact as the default for Vec<T> sucks (make a custom one using const generics so we can optimize for fixed-size types?)
pub struct PartitionHashes(pub Vec<PartitionHash>);
