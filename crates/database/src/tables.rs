use irys_types::{IrysBlockHeader, IrysTransactionHeader, H256};
use reth_codecs::Compact;
use reth_db::{
    table::{DupSort, Table},
    tables, DatabaseError,
};
use reth_db::{HasName, HasTableType, TableType, TableViewer};
use reth_db_api::table::{Compress, Decompress};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::db_cache::{CachedChunk, CachedDataRoot};

/// Adds wrapper structs for some primitive types so they can use `StructFlags` from Compact, when
/// used as pure table values.
#[macro_export]
macro_rules! add_wrapper_struct {
	($(($name:tt, $wrapper:tt)),+) => {
			$(
					/// Wrapper struct so it can use StructFlags from Compact, when used as pure table values.
					#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, Compact)]
					#[derive(arbitrary::Arbitrary)]
					//#[add_arbitrary_tests(compact)]
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
    CachedChunk
);

tables! {
    /// Stores the header hashes belonging to the canonical chain.
    table IrysBlockHeaders<Key = H256, Value = CompactIrysBlockHeader>;

    table IrysTxHeaders<Key = H256, Value = CompactTxHeader>;

    table CachedDataRoots<Key = H256, Value = CachedDataRoot>;

    table CachedChunks<Key = H256, Value = CachedChunk>;
}
