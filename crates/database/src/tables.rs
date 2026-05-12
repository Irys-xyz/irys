use crate::db_cache::{GlobalChunkOffset, PartitionHashes};
use crate::metadata::MetadataKey;
use crate::submodule::tables::DataRootInfos;
use crate::{
    db_cache::{CachedChunk, CachedChunkIndexEntry, CachedDataRoot},
    submodule::tables::ChunkPathHashes,
};
use irys_types::ingress::CachedIngressProof;
use irys_types::{
    Base64, BlockHeight, DataLedger, IrysAddress, IrysPeerId, LedgerIndexItem, PeerListItemInner,
};
use irys_types::{ChunkPathHash, DataRoot, H256};
use irys_types::{
    CommitmentTransaction, CommitmentTransactionMetadata, DataTransactionHeader,
    DataTransactionMetadata, IrysBlockHeader,
};
use reth_codecs::Compact;
use reth_db::{DatabaseError, TableSet, table::DupSort, tables};
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
            #[derive(Debug, Clone, Default, Serialize, Deserialize, Compact)]
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
add_wrapper_struct!((PeerListItemInner, CompactPeerListItem));
add_wrapper_struct!((Base64, CompactBase64));
add_wrapper_struct!((LedgerIndexItem, CompactLedgerIndexItem));
add_wrapper_struct!((CommitmentTransactionMetadata, CompactCommitmentTxMetadata));
add_wrapper_struct!((DataTransactionMetadata, CompactDataTxMetadata));
add_wrapper_struct!((CachedIngressProof, CompactCachedIngressProof));

impl_compression_for_compact!(
    CompactIrysBlockHeader,
    CompactTxHeader,
    CompactCommitment,
    CompactPeerListItem,
    CachedDataRoot,
    CachedChunkIndexEntry,
    CachedChunk,
    ChunkPathHashes,
    PartitionHashes,
    DataRootInfos,
    GlobalChunkOffset,
    CompactBase64,
    CompactCachedIngressProof,
    CompactLedgerIndexItem,
    CompactCommitmentTxMetadata,
    CompactDataTxMetadata
);

use paste::paste;
use reth_db::table::TableInfo;
use reth_primitives_traits::ValueWithSubKey;

impl ValueWithSubKey for CompactLedgerIndexItem {
    type SubKey = DataLedger;

    fn get_subkey(&self) -> Self::SubKey {
        self.0.ledger
    }
}

impl ValueWithSubKey for CachedChunkIndexEntry {
    type SubKey = u32;

    fn get_subkey(&self) -> Self::SubKey {
        self.index.0
    }
}

impl ValueWithSubKey for CompactCachedIngressProof {
    type SubKey = IrysAddress;

    fn get_subkey(&self) -> Self::SubKey {
        self.0.address
    }
}

mod consensus_tables_inner {
    use super::*;

    tables! {
    ConsensusTables;

    /// Stores block headers keyed by their hash (canonical chain).
    table IrysBlockHeaders {
        type Key = H256;
        type Value = CompactIrysBlockHeader;
    }

    // Block index table: BlockHeight -> DataLedger -> LedgerIndexItem
    // Stores ledger metadata for each block, keyed by height with ledger type as subkey.
    // This indexing scheme is optimized for pruning ledgers by height which is
    // a function of expiring term ledgers.
    table IrysBlockIndexItems {
        type Key = BlockHeight;
        type Value = CompactLedgerIndexItem;
        type SubKey = DataLedger;
    }

    // Indexes migrated block hashes by height
    table MigratedBlockHashes {
        type Key = BlockHeight;
        type Value = H256;
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

    /// Stores metadata for commitment transactions
    /// Tracks inclusion height
    table IrysCommitmentTxMetadata {
        type Key = H256;
        type Value = CompactCommitmentTxMetadata;
    }

    /// Stores metadata for data transactions
    /// Tracks inclusion height and promotion height
    table IrysDataTxMetadata {
        type Key = H256;
        type Value = CompactDataTxMetadata;
    }

    /// Tracks the peer list of known peers as well as their reputation score.
    /// While the node maintains connections to a subset of these peers - the
    /// ones with high reputation - the PeerListItems contain all the peers
    /// that the node is aware of and is periodically updated via peer discovery
    table PeerListItems {
        type Key = IrysPeerId;
        type Value = CompactPeerListItem;
    }

    /// Table to store various metadata, such as the current db schema version
    /// (consensus environment).
    table Metadata {
        type Key = MetadataKey;
        type Value = Vec<u8>;
    }
    }
}

mod cache_tables_inner {
    use super::*;

    tables! {
    CacheTables;

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
        type SubKey = IrysAddress;
    }

    /// Schema-version metadata for the cache environment
    /// (independent of the consensus `Metadata` table).
    table CacheMetadata {
        type Key = MetadataKey;
        type Value = Vec<u8>;
    }
    }
}

pub use consensus_tables_inner::{
    ConsensusTables, IrysBlockHeaders, IrysBlockIndexItems, IrysCommitmentTxMetadata,
    IrysCommitments, IrysDataTxHeaders, IrysDataTxMetadata, IrysPoAChunks, Metadata,
    MigratedBlockHashes, PeerListItems,
};

pub use cache_tables_inner::{
    CacheMetadata, CacheTables, CachedChunks, CachedChunksIndex, CachedDataRoots, IngressProofs,
};

// Marker-trait impls — wire each table to its environment.
use crate::scoped_tx::{Cache, CacheTable, Consensus, ConsensusTable, IrysScope};

impl IrysScope for Consensus {
    type Tables = ConsensusTables;
    const ALL_TABLES: &'static [Self::Tables] = ConsensusTables::ALL;
}

impl IrysScope for Cache {
    type Tables = CacheTables;
    const ALL_TABLES: &'static [Self::Tables] = CacheTables::ALL;
}

macro_rules! impl_consensus_tables {
    ($($name:ident),* $(,)?) => {
        $(impl ConsensusTable for $name {})*
    };
}

macro_rules! impl_cache_tables {
    ($($name:ident),* $(,)?) => {
        $(impl CacheTable for $name {})*
    };
}

impl_consensus_tables!(
    IrysBlockHeaders,
    IrysBlockIndexItems,
    MigratedBlockHashes,
    IrysPoAChunks,
    IrysDataTxHeaders,
    IrysCommitments,
    IrysCommitmentTxMetadata,
    IrysDataTxMetadata,
    PeerListItems,
    Metadata,
);

impl_cache_tables!(
    CachedDataRoots,
    CachedChunksIndex,
    CachedChunks,
    IngressProofs,
    CacheMetadata,
);

#[cfg(test)]
mod tables_names {
    use super::*;
    use reth_db::table::Table as _;

    #[test]
    fn table_names_match_struct_idents() {
        assert_eq!(IrysBlockHeaders::NAME, "IrysBlockHeaders");
        assert_eq!(Metadata::NAME, "Metadata");
        assert_eq!(CachedChunks::NAME, "CachedChunks");
        assert_eq!(CacheMetadata::NAME, "CacheMetadata");
    }
}

#[cfg(test)]
mod marker_trait_sanity {
    use super::*;
    use crate::scoped_tx::{CacheTable, ConsensusTable};
    use static_assertions::{assert_impl_all, assert_not_impl_any};

    // Consensus tables
    assert_impl_all!(IrysBlockHeaders: ConsensusTable);
    assert_not_impl_any!(IrysBlockHeaders: CacheTable);
    assert_impl_all!(Metadata: ConsensusTable);
    assert_not_impl_any!(Metadata: CacheTable);

    // Cache tables
    assert_impl_all!(CachedChunks: CacheTable);
    assert_not_impl_any!(CachedChunks: ConsensusTable);
    assert_impl_all!(IngressProofs: CacheTable);
    assert_not_impl_any!(IngressProofs: ConsensusTable);
    assert_impl_all!(CacheMetadata: CacheTable);
    assert_not_impl_any!(CacheMetadata: ConsensusTable);
}
