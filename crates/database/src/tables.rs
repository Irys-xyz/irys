use crate::db_cache::{GlobalChunkOffset, PartitionHashes};
use crate::metadata::MetadataKey;
use crate::submodule::tables::RelativeStartOffsets;
use crate::{
    db_cache::{CachedChunk, CachedChunkIndexEntry, CachedDataRoot},
    submodule::tables::{ChunkOffsets, ChunkPathHashes},
};
use irys_types::ingress::CachedIngressProof;
use irys_types::{Address, Base64, CommitmentTransaction, PeerListItem};
use irys_types::{ChunkPathHash, DataRoot, DataTransactionHeader, IrysBlockHeader, H256};
use reth_codecs::Compact;
use reth_db::{table::DupSort, tables, DatabaseError, TableSet};
use reth_db::{TableType, TableViewer};
use reth_db_api::table::{Compress, Decompress};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Adds wrapper structs for some primitive types so they can derive `Compact` and be used
/// directly as table values.
#[macro_export]
macro_rules! add_wrapper_struct {
	($(($name:tt, $wrapper:tt)),+) => {
        $(
            /// Wrapper struct enabling `Compact` derivation so it can be used directly as a table value.
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

							fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
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
add_wrapper_struct!((DataTransactionHeader, CompactTxHeader));
add_wrapper_struct!((CommitmentTransaction, CompactCommitment));
add_wrapper_struct!((PeerListItem, CompactPeerListItem));
add_wrapper_struct!((Base64, CompactBase64));

add_wrapper_struct!((CachedIngressProof, CompactCachedIngressProof));

impl_compression_for_compact!(
    CompactIrysBlockHeader,
    CompactTxHeader,
    CompactCommitment,
    CompactPeerListItem,
    CachedDataRoot,
    CachedChunkIndexEntry,
    CachedChunk,
    ChunkOffsets,
    ChunkPathHashes,
    PartitionHashes,
    RelativeStartOffsets,
    GlobalChunkOffset,
    CompactBase64,
    CompactCachedIngressProof
);

use paste::paste;
use reth_db::table::TableInfo;

tables! {
IrysTables;

/// Stores block headers keyed by their hash (canonical chain).
table IrysBlockHeaders {
    type Key = H256;
    type Value = CompactIrysBlockHeader;
}

/// Stores PoA chunks
table IrysPoAChunks {
    type Key = H256;
    type Value = CompactBase64;
}

/// Stores confirmed transaction headers
table IrysDataTxHeaders {
    type Key = H256;
    type Value = CompactTxHeader;
}

/// Stores commitment transactions
table IrysCommitments {
    type Key = H256;
    type Value = CompactCommitment;
}

/// Indexes the DataRoots currently in the cache
table CachedDataRoots {
    type Key = DataRoot;
    type Value = CachedDataRoot;
}

/// Index mapping a DataRoot to a set of ordered-by-index index entries, which contain the ChunkPathHash ('chunk id')
table CachedChunksIndex {
    type Key = DataRoot;
    type Value = CachedChunkIndexEntry;
    type SubKey = u32;
}

/// Maps a ChunkPathHash to the cached chunk metadata and optionally its data
table CachedChunks {
    type Key = ChunkPathHash;
    type Value = CachedChunk;
}

/// Indexes ingress proofs by DataRoot and Address
table IngressProofs {
    type Key = DataRoot;
    type Value = CompactCachedIngressProof;
    type SubKey = Address;
}

/// Maps a global (perm) chunk offset to the last block height it was used by a transaction
/// this acts as an LRU cache for PD chunks, to reduce the bandwidth requirements for frequently used chunks
table ProgrammableDataLRU {
    type Key = GlobalChunkOffset;
    type Value = u64;
}

/// Maps a global offset to a cached chunk
table ProgrammableDataCache {
    type Key = GlobalChunkOffset;
    type Value = CachedChunk;
}

/// Tracks the peer list of known peers as well as their reputation score.
/// While the node maintains connections to a subset of these peers - the
/// ones with high reputation - the PeerListItems contain all the peers
/// that the node is aware of and is periodically updated via peer discovery
table PeerListItems {
    type Key = Address;
    type Value = CompactPeerListItem;
}

/// Table to store various metadata, such as the current db schema version
table Metadata {
    type Key = MetadataKey;
    type Value = Vec<u8>;
}
}
