use crate::db_cache::{GlobalChunkOffset, PartitionHashes};
use crate::metadata::MetadataKey;
use crate::submodule::tables::{DataRootInfos, PendingBodyMigration, TxLeafBinding};
use crate::{
    db_cache::{
        CachedChunk, CachedChunkIndexEntry, CachedDataRoot, CachedIngressLeaf, CachedIngressLeafKey,
    },
    submodule::tables::{ChunkPathHashes, TxPathInterval},
};
use alloy_primitives::B512;
use irys_types::ingress::{CachedIngressProof, IngressProofV1};
use irys_types::{
    Base64, BlockHeight, DataLedger, IrysAddress, IrysPeerId, LedgerIndexItem, PeerListItemInner,
};
use irys_types::{ChunkPathHash, DataRoot, H256, PartitionChunkOffset, decode_address};
use irys_types::{
    CommitmentTransaction, CommitmentTransactionMetadata, CommitmentTransactionV1,
    CommitmentTransactionV2, DataTransactionHeader, DataTransactionHeaderV1,
    DataTransactionMetadata, IrysBlockHeader, IrysBlockHeaderV1, Versioned as _,
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

/// Cheap structural check run by `Decompress::decompress` before `Compact::from_compact`.
///
/// `from_compact` panics on malformed input, and a panic stops the node. Implementations
/// reject the inputs that make `from_compact` panic at the top level of the encoding
/// (missing bytes, unknown version discriminant). They do not parse nested fields, so a
/// row that passes can still be malformed deeper in; a row that fails is always malformed.
/// The default accepts every buffer.
pub trait CompactRowCheck {
    /// Returns the reason the buffer cannot be a valid encoding of `Self`.
    fn check_compact_row(_buf: &[u8]) -> Result<(), String> {
        Ok(())
    }
}

/// Fails when `buf` is shorter than `min_len`.
pub fn require_min_compact_row_len(buf: &[u8], min_len: usize) -> Result<(), String> {
    if buf.len() < min_len {
        return Err(format!(
            "row too short: expected at least {min_len} bytes, got {}",
            buf.len()
        ));
    }
    Ok(())
}

/// Checks the leading version byte against `supported`, then the minimum length for it.
/// `min_len_for_version` gives the length of the version byte plus the fixed header of
/// that version's inner encoding.
fn check_versioned_compact_row(
    buf: &[u8],
    supported: &[u8],
    min_len_for_version: impl Fn(u8) -> usize,
) -> Result<(), String> {
    let Some(&version) = buf.first() else {
        return Err("row is empty: missing version byte".to_string());
    };
    if !supported.contains(&version) {
        return Err(format!("unsupported version {version}"));
    }
    require_min_compact_row_len(buf, min_len_for_version(version))
}

/// Implements `CompactRowCheck` for derived-`Compact` types: the encoding starts with a
/// bit-flag header of `bitflag_encoded_bytes()` bytes that `from_compact` reads unconditionally.
macro_rules! impl_compact_row_check_for_flag_header {
    ($($name:ty),+) => {
        $(
            impl CompactRowCheck for $name {
                fn check_compact_row(buf: &[u8]) -> Result<(), String> {
                    require_min_compact_row_len(buf, <$name>::bitflag_encoded_bytes())
                }
            }
        )+
    };
}

/// Implements `CompactRowCheck` with an explicit minimum row length, for types whose flag
/// header is empty (`bitflag_encoded_bytes() == 0`) but whose `from_compact` still reads fixed
/// fields unconditionally. The length must never exceed a well-formed encoding of any value.
macro_rules! impl_compact_row_check_for_min_len {
    ($($name:ty => $min_len:expr),+ $(,)?) => {
        $(
            impl CompactRowCheck for $name {
                fn check_compact_row(buf: &[u8]) -> Result<(), String> {
                    require_min_compact_row_len(buf, $min_len)
                }
            }
        )+
    };
}

/// Implements `Compress`/`Decompress` through `Compact`. `decompress` first runs
/// `CompactRowCheck`, so each type must implement that trait.
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
									if let Err(reason) =
											<$name as $crate::tables::CompactRowCheck>::check_compact_row(value)
									{
											tracing::error!(
													table_value_type = stringify!($name),
													buffer_len = value.len(),
													%reason,
													"malformed table row"
											);
											return Err(DatabaseError::Decode);
									}
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
    CachedIngressLeaf,
    ChunkPathHashes,
    TxPathInterval,
    PartitionHashes,
    DataRootInfos,
    TxLeafBinding,
    PendingBodyMigration,
    GlobalChunkOffset,
    CompactBase64,
    CompactCachedIngressProof,
    CompactLedgerIndexItem,
    CompactCommitmentTxMetadata,
    CompactDataTxMetadata
);

impl CompactRowCheck for CompactIrysBlockHeader {
    fn check_compact_row(buf: &[u8]) -> Result<(), String> {
        check_versioned_compact_row(buf, &[IrysBlockHeaderV1::VERSION], |_| {
            1 + IrysBlockHeaderV1::bitflag_encoded_bytes()
        })
    }
}

impl CompactRowCheck for CompactTxHeader {
    fn check_compact_row(buf: &[u8]) -> Result<(), String> {
        check_versioned_compact_row(buf, &[DataTransactionHeaderV1::VERSION], |_| {
            1 + DataTransactionHeaderV1::bitflag_encoded_bytes()
        })
    }
}

impl CompactRowCheck for CompactCommitment {
    fn check_compact_row(buf: &[u8]) -> Result<(), String> {
        check_versioned_compact_row(
            buf,
            &[
                CommitmentTransactionV1::VERSION,
                CommitmentTransactionV2::VERSION,
            ],
            |version| {
                1 + if version == CommitmentTransactionV1::VERSION {
                    CommitmentTransactionV1::bitflag_encoded_bytes()
                } else {
                    CommitmentTransactionV2::bitflag_encoded_bytes()
                }
            },
        )
    }
}

/// Byte offset of `IngressProof`'s variant byte: after the 20-byte `IrysAddress`
/// (`CachedIngressProof` has no flag header).
const CACHED_INGRESS_PROOF_VARIANT_OFFSET: usize = 20;
/// The derived `Compact` for `IngressProof` stores the variant's declaration index
/// (not its `repr(u8)` discriminant) in its first byte. `V1` is declared first.
const INGRESS_PROOF_V1_COMPACT_VARIANT: u8 = 0;

impl CompactRowCheck for CompactCachedIngressProof {
    fn check_compact_row(buf: &[u8]) -> Result<(), String> {
        let proof_start = Self::bitflag_encoded_bytes() + CACHED_INGRESS_PROOF_VARIANT_OFFSET;
        let Some(&variant) = buf.get(proof_start) else {
            return Err(format!(
                "row too short: no ingress proof variant byte at offset {proof_start}, got {} bytes",
                buf.len()
            ));
        };
        if variant != INGRESS_PROOF_V1_COMPACT_VARIANT {
            return Err(format!("unsupported ingress proof variant {variant}"));
        }
        require_min_compact_row_len(
            buf,
            proof_start + 1 + IngressProofV1::bitflag_encoded_bytes(),
        )
    }
}

impl_compact_row_check_for_flag_header!(
    CachedDataRoot,
    CachedChunk,
    ChunkPathHashes,
    PendingBodyMigration
);

/// Smallest `Vec` encoding: the varint element count (an empty vec).
const MIN_VEC_COUNT_LEN: usize = 1;

// `Base64` is a raw byte string, so every buffer (including an empty one) is a valid row.
impl CompactRowCheck for CompactBase64 {}

// The wrapper derive emits no flag header of its own, so each minimum is the wrapped type's
// flag header plus the fields `from_compact` reads unconditionally.
impl_compact_row_check_for_min_len!(
    // flag header, `tx_root`, and the `DataLedger` variant (varint, at least 1 byte)
    CompactLedgerIndexItem => LedgerIndexItem::bitflag_encoded_bytes() + size_of::<H256>() + 1,
    CompactCommitmentTxMetadata => CommitmentTransactionMetadata::bitflag_encoded_bytes(),
    CompactDataTxMetadata => DataTransactionMetadata::bitflag_encoded_bytes(),
    // `end` (its own flag header) and `tx_path_hash`
    TxPathInterval => PartitionChunkOffset::bitflag_encoded_bytes() + size_of::<H256>(),
    TxLeafBinding => 2 * size_of::<H256>(),
    PartitionHashes => MIN_VEC_COUNT_LEN,
    DataRootInfos => MIN_VEC_COUNT_LEN,
);

/// `PeerListItemInner::from_compact` tolerates a row cut right after the 4-byte
/// score/response-time prefix (older layout) but reads the Reth peer id (a `B512`) after the
/// gossip, API and Reth peering addresses unconditionally, so a longer row must still hold one.
impl CompactRowCheck for CompactPeerListItem {
    fn check_compact_row(buf: &[u8]) -> Result<(), String> {
        const PREFIX_LEN: usize = 4;
        require_min_compact_row_len(buf, PREFIX_LEN)?;
        if buf.len() == PREFIX_LEN {
            return Ok(());
        }
        let mut rest = &buf[PREFIX_LEN..];
        for _ in 0..3 {
            let (_, consumed) = decode_address(rest);
            rest = &rest[consumed..];
        }
        require_min_compact_row_len(rest, B512::len_bytes())
            .map_err(|reason| format!("peer id after addresses: {reason}"))
    }
}

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

impl ValueWithSubKey for CachedIngressLeaf {
    type SubKey = u32;

    fn get_subkey(&self) -> Self::SubKey {
        self.tx_offset.0
    }
}

impl ValueWithSubKey for CompactCachedIngressProof {
    type SubKey = IrysAddress;

    fn get_subkey(&self) -> Self::SubKey {
        self.0.address
    }
}

tables! {
IrysTables;

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

/// Stores one compact, miner-specific ingress leaf per transaction-relative
/// chunk offset, recorded when the chunk is validated. Cached-body reclamation
/// is gated on storage-module durability, not on the presence of a leaf.
table CachedIngressLeaves {
    type Key = CachedIngressLeafKey;
    type Value = CachedIngressLeaf;
    type SubKey = u32;
}

/// Indexes ingress proofs by DataRoot and Address
table IngressProofs {
    type Key = DataRoot;
    type Value = CompactCachedIngressProof;
    type SubKey = IrysAddress;
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
table Metadata {
    type Key = MetadataKey;
    type Value = Vec<u8>;
}

/// Append-only log of internal block-stream events, keyed by a monotonic `seq`.
/// The value is the canonical wire JSON of the event payload (see
/// `irys_types::block_stream`). Node-local: stripped from snapshots.
table IrysBlockStreamEvents {
    type Key = u64;
    type Value = Vec<u8>;
}
}

#[cfg(test)]
mod compact_row_check_tests {
    use super::*;
    use crate::db_cache::GlobalChunkOffset;
    use rstest::rstest;

    /// One table value type, reduced to what the checks need.
    struct RowCase {
        /// Valid encoding of `T::default()`.
        valid: Vec<u8>,
        decompress_fails: fn(&[u8]) -> bool,
        /// Shortest buffer the check accepts (independent of the check's own constants).
        min_len: usize,
        /// Offset of the version/variant byte, for versioned types.
        version_offset: Option<usize>,
    }

    fn row_case<T>(value: T, min_len: usize, version_offset: Option<usize>) -> RowCase
    where
        T: Compress<Compressed = Vec<u8>> + Decompress,
    {
        RowCase {
            valid: value.compress(),
            decompress_fails: |buf| T::decompress(buf).is_err(),
            min_len,
            version_offset,
        }
    }

    #[rstest]
    #[case::block_header(row_case(CompactIrysBlockHeader::default(), 7, Some(0)))]
    #[case::data_tx_header(row_case(CompactTxHeader::default(), 4, Some(0)))]
    #[case::commitment_v2(row_case(CompactCommitment::default(), 3, Some(0)))]
    #[case::cached_ingress_proof(row_case(CompactCachedIngressProof::default(), 22, Some(20)))]
    #[case::global_chunk_offset(row_case(GlobalChunkOffset::default(), 29, None))]
    #[case::cached_ingress_leaf(row_case(CachedIngressLeaf { tx_offset: 7.into(), ingress_data_hash: H256::repeat_byte(1) }, 36, None))]
    #[case::cached_chunk_index_entry(row_case(CachedChunkIndexEntry::default(), 5, None))]
    #[case::cached_data_root(row_case(CachedDataRoot::default(), 2, None))]
    #[case::cached_chunk(row_case(CachedChunk::default(), 1, None))]
    #[case::chunk_path_hashes(row_case(ChunkPathHashes::default(), 1, None))]
    #[case::pending_body_migration(row_case(PendingBodyMigration::default(), 2, None))]
    #[case::peer_list_item(row_case(CompactPeerListItem::default(), 4, None))]
    #[case::base64(row_case(CompactBase64::default(), 0, None))]
    #[case::ledger_index_item(row_case(CompactLedgerIndexItem::default(), 34, None))]
    #[case::commitment_metadata(row_case(CompactCommitmentTxMetadata::default(), 1, None))]
    #[case::data_metadata(row_case(CompactDataTxMetadata::default(), 1, None))]
    #[case::tx_path_interval(row_case(TxPathInterval::default(), 33, None))]
    #[case::tx_leaf_binding(row_case(TxLeafBinding::default(), 64, None))]
    #[case::data_root_infos(row_case(DataRootInfos::default(), 1, None))]
    #[case::partition_hashes(row_case(PartitionHashes::default(), 1, None))]
    #[case::h256(row_case(H256::repeat_byte(1), 32, None))]
    fn malformed_rows_are_decode_errors(#[case] case: RowCase) {
        assert!(
            case.valid.len() >= case.min_len,
            "valid encoding is shorter than the minimum"
        );
        assert!(
            !(case.decompress_fails)(&case.valid),
            "valid row must decode"
        );
        if case.min_len > 0 {
            assert!(
                (case.decompress_fails)(&[]),
                "empty buffer must be rejected"
            );
            assert!(
                (case.decompress_fails)(&case.valid[..case.min_len - 1]),
                "truncated buffer must be rejected"
            );
        }
        if let Some(offset) = case.version_offset {
            let mut unknown = case.valid.clone();
            unknown[offset] = 0xFF;
            assert!(
                (case.decompress_fails)(&unknown),
                "unknown version must be rejected"
            );
        }
    }

    /// Re-encoding a decoded valid row reproduces the stored bytes.
    fn assert_round_trip<T>(value: T)
    where
        T: Compress<Compressed = Vec<u8>> + Decompress,
    {
        let encoded = value.compress();
        let decoded = T::decompress(&encoded).expect("valid row decodes");
        assert_eq!(decoded.compress(), encoded);
    }

    #[test]
    fn valid_rows_decode_to_the_same_encoding() {
        assert_round_trip(CompactIrysBlockHeader::default());
        assert_round_trip(CompactTxHeader::default());
        assert_round_trip(CompactCommitment::default());
        assert_round_trip(CompactCachedIngressProof::default());
        assert_round_trip(GlobalChunkOffset::default());
        assert_round_trip(CachedIngressLeaf {
            tx_offset: 7.into(),
            ingress_data_hash: H256::repeat_byte(1),
        });
        assert_round_trip(CachedChunkIndexEntry::default());
        assert_round_trip(CachedDataRoot::default());
        assert_round_trip(CachedChunk::default());
        assert_round_trip(ChunkPathHashes::default());
        assert_round_trip(PendingBodyMigration::default());
        assert_round_trip(CompactPeerListItem::default());
        assert_round_trip(CompactBase64::default());
        assert_round_trip(CompactLedgerIndexItem::default());
        assert_round_trip(CompactCommitmentTxMetadata::default());
        assert_round_trip(CompactDataTxMetadata::default());
        assert_round_trip(TxPathInterval::default());
        assert_round_trip(TxLeafBinding::default());
        assert_round_trip(DataRootInfos::default());
        assert_round_trip(PartitionHashes::default());
        assert_round_trip(H256::repeat_byte(1));
    }

    /// Rows holding the minimum amount of data (all fields at their smallest encoding) decode.
    #[test]
    fn minimal_rows_decode() {
        let base64 = CompactBase64(Base64(vec![7, 8, 9]));
        assert_eq!(
            CompactBase64::decompress(&base64.compress()).unwrap().0.0,
            vec![7, 8, 9]
        );
        assert!(CompactBase64::decompress(&[]).unwrap().0.0.is_empty());
        let ledger_row = CompactLedgerIndexItem::default().compress();
        assert_eq!(
            ledger_row.len(),
            LedgerIndexItem::bitflag_encoded_bytes() + size_of::<H256>() + 1
        );
    }

    #[test]
    fn peer_list_item_rows_between_prefix_and_peer_id_are_rejected() {
        let valid = CompactPeerListItem::default().compress();
        // Prefix-only rows decode to defaults; any other cut before the peer id panics.
        assert!(CompactPeerListItem::decompress(&valid[..4]).is_ok());
        for len in 5..(valid.len() - 8 - 1 - 4) {
            assert!(
                CompactPeerListItem::decompress(&valid[..len]).is_err(),
                "cut at {len}"
            );
        }
    }

    #[test]
    fn commitment_v1_row_round_trips() {
        let value = CommitmentTransaction::V1(irys_types::CommitmentV1WithMetadata {
            tx: CommitmentTransactionV1::default(),
            metadata: CommitmentTransactionMetadata::new(),
        });
        let row = CompactCommitment(value).compress();
        assert_eq!(row[0], CommitmentTransactionV1::VERSION);
        let decoded = CompactCommitment::decompress(&row).expect("V1 row decodes");
        assert!(matches!(decoded.0, CommitmentTransaction::V1(_)));
        assert_eq!(decoded.compress(), row);
    }

    #[test]
    fn commitment_v1_truncated_row_is_rejected() {
        let v1 = CommitmentTransactionV1::VERSION;
        assert!(CompactCommitment::decompress(&[v1]).is_err());
        assert!(CompactCommitment::decompress(&[v1, 0]).is_err());
    }

    #[test]
    fn ingress_proof_variant_constant_matches_the_derived_encoding() {
        // Distinct non-zero address bytes: a shifted offset would read an address byte.
        let address = IrysAddress::from([
            1_u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        ]);
        let encoded = CompactCachedIngressProof(CachedIngressProof {
            address,
            ..Default::default()
        })
        .compress();
        assert_eq!(encoded[..CACHED_INGRESS_PROOF_VARIANT_OFFSET], address.0.0);
        assert_eq!(
            encoded[CACHED_INGRESS_PROOF_VARIANT_OFFSET],
            INGRESS_PROOF_V1_COMPACT_VARIANT
        );
    }
}
