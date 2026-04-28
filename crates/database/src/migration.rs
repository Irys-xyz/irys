use crate::db::IrysDatabaseExt as _;
use irys_types::DatabaseVersion;

use crate::reth_db::{
    DatabaseEnv, DatabaseError,
    transaction::{DbTx, DbTxMut},
};

use std::fmt::Debug;
use tracing::debug;

// Old DataTransactionHeaderV1 structure used in migration modules
// This is kept for historical migration purposes only
mod old_structures {
    use irys_types::{Arbitrary, H256, IrysAddress, IrysSignature, transaction::BoundedFee};
    use reth_codecs::Compact;
    use serde::{Deserialize, Serialize};

    /// Old DataTransactionHeaderV1 WITH promoted_height field
    /// Used for v1_to_v2 migration - mirrors the structure before promoted_height was removed
    #[derive(
        Clone, Default, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Arbitrary, Compact,
    )]
    pub(super) struct DataTransactionHeaderV1WithPromotedHeight {
        pub id: H256,
        pub anchor: H256,
        pub signer: IrysAddress,
        pub data_root: H256,
        #[serde(with = "irys_types::string_u64")]
        pub data_size: u64,
        #[serde(with = "irys_types::string_u64")]
        pub header_size: u64,
        pub term_fee: BoundedFee,
        pub ledger_id: u32,
        #[serde(with = "irys_types::string_u64")]
        pub chain_id: u64,
        pub signature: IrysSignature,
        #[serde(default, with = "irys_types::optional_string_u64")]
        pub bundle_format: Option<u64>,
        #[serde(default)]
        pub perm_fee: Option<BoundedFee>,
        #[serde(default, with = "irys_types::optional_string_u64")]
        pub promoted_height: Option<u64>,
    }

    /// Old DataTransactionHeader enum (with discriminant)
    #[derive(Clone, Debug, PartialEq, Eq, Arbitrary)]
    pub(super) enum DataTransactionHeaderOld {
        V1(DataTransactionHeaderV1WithPromotedHeight),
    }

    impl Compact for DataTransactionHeaderOld {
        fn to_compact<B>(&self, buf: &mut B) -> usize
        where
            B: bytes::BufMut + AsMut<[u8]>,
        {
            match self {
                Self::V1(h) => irys_types::compact_with_discriminant(1, h, buf),
            }
        }

        fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
            let (disc, rest) = irys_types::split_discriminant(buf);
            match disc {
                1 => {
                    let (h, rest2) =
                        DataTransactionHeaderV1WithPromotedHeight::from_compact(rest, rest.len());
                    (Self::V1(h), rest2)
                }
                other => panic!("Unsupported DataTransactionHeaderOld version: {}", other),
            }
        }
    }

    /// Wrapper for old format matching CompactTxHeader structure
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) struct CompactTxHeaderOld(pub DataTransactionHeaderOld);

    impl Compact for CompactTxHeaderOld {
        fn to_compact<B>(&self, buf: &mut B) -> usize
        where
            B: bytes::BufMut + AsMut<[u8]>,
        {
            self.0.to_compact(buf)
        }

        fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
            let (h, rest) = DataTransactionHeaderOld::from_compact(buf, len);
            (Self(h), rest)
        }
    }

    // Manual Serialize/Deserialize via Compact encoding (since Compact doesn't automatically derive serde)
    impl serde::Serialize for CompactTxHeaderOld {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let mut buf = Vec::new();
            self.to_compact(&mut buf);
            serializer.serialize_bytes(&buf)
        }
    }

    impl<'de> serde::Deserialize<'de> for CompactTxHeaderOld {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            let bytes = <Vec<u8>>::deserialize(deserializer)?;
            let (value, _) = Self::from_compact(&bytes, bytes.len());
            Ok(value)
        }
    }

    impl reth_db_api::table::Compress for CompactTxHeaderOld {
        type Compressed = Vec<u8>;

        fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
            let _ = Compact::to_compact(&self, buf);
        }
    }

    impl reth_db_api::table::Decompress for CompactTxHeaderOld {
        fn decompress(value: &[u8]) -> Result<Self, super::DatabaseError> {
            let (obj, _) = Compact::from_compact(value, value.len());
            Ok(obj)
        }
    }

    /// Old table definition for IrysDataTxHeaders (v1 format with promoted_height)
    #[derive(Clone, Copy, Debug, Default)]
    pub(super) struct IrysDataTxHeadersOld;

    impl reth_db::table::Table for IrysDataTxHeadersOld {
        // Same physical table as `crate::tables::IrysDataTxHeaders` — pull the
        // name from there so the two can never drift apart.
        const NAME: &'static str =
            <crate::tables::IrysDataTxHeaders as reth_db::table::Table>::NAME;
        const DUPSORT: bool = false;
        type Key = H256;
        type Value = CompactTxHeaderOld;
    }
}

// V2-on-disk DataTransactionHeader structures used by the v2_to_v3 migration.
// At V2 the inline header still carries `bundle_format: Option<u64>`; the
// rename to `metadata_format: u8` is what V2 → V3 performs.
mod v2_structures {
    use irys_types::{Arbitrary, H256, IrysAddress, IrysSignature, transaction::BoundedFee};
    use reth_codecs::Compact;
    use serde::{Deserialize, Serialize};

    #[derive(
        Clone, Default, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Arbitrary, Compact,
    )]
    pub(super) struct DataTransactionHeaderV1WithBundleFormat {
        pub id: H256,
        pub anchor: H256,
        pub signer: IrysAddress,
        pub data_root: H256,
        #[serde(with = "irys_types::string_u64")]
        pub data_size: u64,
        #[serde(with = "irys_types::string_u64")]
        pub header_size: u64,
        pub term_fee: BoundedFee,
        pub ledger_id: u32,
        #[serde(with = "irys_types::string_u64")]
        pub chain_id: u64,
        pub signature: IrysSignature,
        #[serde(default, with = "irys_types::optional_string_u64")]
        pub bundle_format: Option<u64>,
        #[serde(default)]
        pub perm_fee: Option<BoundedFee>,
    }

    /// V2-on-disk DataTransactionHeader enum wrapper (discriminant + inner).
    #[derive(Clone, Debug, PartialEq, Eq, Arbitrary)]
    pub(super) enum DataTransactionHeaderV2 {
        V1(DataTransactionHeaderV1WithBundleFormat),
    }

    impl Compact for DataTransactionHeaderV2 {
        fn to_compact<B>(&self, buf: &mut B) -> usize
        where
            B: bytes::BufMut + AsMut<[u8]>,
        {
            match self {
                Self::V1(h) => irys_types::compact_with_discriminant(1, h, buf),
            }
        }

        fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
            let (disc, rest) = irys_types::split_discriminant(buf);
            match disc {
                1 => {
                    let (h, rest2) =
                        DataTransactionHeaderV1WithBundleFormat::from_compact(rest, rest.len());
                    (Self::V1(h), rest2)
                }
                other => panic!("Unsupported DataTransactionHeaderV2 version: {}", other),
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) struct CompactTxHeaderV2(pub DataTransactionHeaderV2);

    impl Compact for CompactTxHeaderV2 {
        fn to_compact<B>(&self, buf: &mut B) -> usize
        where
            B: bytes::BufMut + AsMut<[u8]>,
        {
            self.0.to_compact(buf)
        }

        fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
            let (h, rest) = DataTransactionHeaderV2::from_compact(buf, len);
            (Self(h), rest)
        }
    }

    impl serde::Serialize for CompactTxHeaderV2 {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let mut buf = Vec::new();
            self.to_compact(&mut buf);
            serializer.serialize_bytes(&buf)
        }
    }

    impl<'de> serde::Deserialize<'de> for CompactTxHeaderV2 {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            let bytes = <Vec<u8>>::deserialize(deserializer)?;
            let (value, _) = Self::from_compact(&bytes, bytes.len());
            Ok(value)
        }
    }

    impl reth_db_api::table::Compress for CompactTxHeaderV2 {
        type Compressed = Vec<u8>;

        fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
            let _ = Compact::to_compact(&self, buf);
        }
    }

    impl reth_db_api::table::Decompress for CompactTxHeaderV2 {
        fn decompress(value: &[u8]) -> Result<Self, super::DatabaseError> {
            let (obj, _) = Compact::from_compact(value, value.len());
            Ok(obj)
        }
    }

    /// Table view of IrysDataTxHeaders that decodes V2 record bytes (bundle_format).
    /// Same physical table NAME as the current `IrysDataTxHeaders`.
    #[derive(Clone, Copy, Debug, Default)]
    pub(super) struct IrysDataTxHeadersV2;

    impl reth_db::table::Table for IrysDataTxHeadersV2 {
        // Same physical table as `crate::tables::IrysDataTxHeaders` — pull the
        // name from there so the two can never drift apart.
        const NAME: &'static str =
            <crate::tables::IrysDataTxHeaders as reth_db::table::Table>::NAME;
        const DUPSORT: bool = false;
        type Key = H256;
        type Value = CompactTxHeaderV2;
    }
}

mod v1_to_v2 {
    use super::*;
    use crate::tables::{IrysBlockHeaders, IrysCommitmentTxMetadata, IrysDataTxMetadata};
    use irys_types::{DataLedger, DataTransactionMetadata, H256, IrysBlockHeader, SystemLedger};
    use reth_db::cursor::DbCursorRO as _;

    fn migrate_batch<TX>(
        tx: &TX,
        entries: &[(
            irys_types::H256,
            old_structures::DataTransactionHeaderV1WithPromotedHeight,
        )],
    ) -> Result<(), DatabaseError>
    where
        TX: DbTxMut + DbTx + Debug,
    {
        for (tx_id, old_header) in entries {
            // Rewrite into the V2 on-disk shape: the inline header drops
            // `promoted_height` (moved to IrysDataTxMetadata below) but still
            // carries `bundle_format: Option<u64>`. The rename to
            // `metadata_format: u8` is performed by the V2→V3 migration.
            let new_inner = v2_structures::DataTransactionHeaderV1WithBundleFormat {
                id: old_header.id,
                anchor: old_header.anchor,
                signer: old_header.signer,
                data_root: old_header.data_root,
                data_size: old_header.data_size,
                header_size: old_header.header_size,
                term_fee: old_header.term_fee,
                ledger_id: old_header.ledger_id,
                chain_id: old_header.chain_id,
                signature: old_header.signature,
                bundle_format: old_header.bundle_format,
                perm_fee: old_header.perm_fee,
            };

            let new_header = v2_structures::DataTransactionHeaderV2::V1(new_inner);

            // Same physical IrysDataTxHeaders table, V2 value shape.
            tx.put::<v2_structures::IrysDataTxHeadersV2>(
                *tx_id,
                v2_structures::CompactTxHeaderV2(new_header),
            )?;

            // If promoted_height existed, create a metadata entry in IrysDataTxMetadata
            // Only set promoted_height; included_height remains None as v1 didn't track original Submit inclusion height
            if let Some(promoted_height) = old_header.promoted_height {
                let metadata = DataTransactionMetadata {
                    included_height: None,
                    promoted_height: Some(promoted_height),
                };
                tx.put::<IrysDataTxMetadata>(*tx_id, metadata.into())?;
            }
        }
        Ok(())
    }

    pub(crate) fn migrate<TX>(tx: &TX) -> Result<(), DatabaseError>
    where
        TX: DbTxMut + DbTx + Debug,
    {
        debug!(
            "Migrating from v1 to v2: Moving promoted_height from DataTransactionHeaderV1 to IrysDataTxMetadata and filling metadata from block headers"
        );

        // Step 1: Migrate existing transaction headers (promoted_height from old format)
        let mut old_cursor = tx.cursor_read::<old_structures::IrysDataTxHeadersOld>()?;
        let mut entries_to_migrate = Vec::new();
        const BATCH_SIZE: usize = 10000;
        let mut total_migrated = 0;

        // Process records in fixed-size chunks to keep memory bounded
        for result in old_cursor.walk(None)? {
            let (tx_id, old_compact_header) = result?;

            // Extract the old header with promoted_height
            let old_structures::DataTransactionHeaderOld::V1(old_header) = old_compact_header.0;

            entries_to_migrate.push((tx_id, old_header));

            // Process and clear batch when it reaches BATCH_SIZE
            if entries_to_migrate.len() >= BATCH_SIZE {
                migrate_batch(tx, &entries_to_migrate)?;
                total_migrated += entries_to_migrate.len();
                entries_to_migrate.clear();
            }
        }

        // Process any remaining entries in the final batch
        if !entries_to_migrate.is_empty() {
            migrate_batch(tx, &entries_to_migrate)?;
            total_migrated += entries_to_migrate.len();
        }

        debug!(
            "Migrated {} transaction headers from old format",
            total_migrated
        );

        // Step 2: Iterate over all block headers and fill metadata based on ledgers
        // This mirrors the logic in handle_block_confirmed_message
        // Stream metadata writes directly to DB to avoid OOM on large chains
        let mut block_cursor = tx.cursor_read::<IrysBlockHeaders>()?;
        let mut blocks_processed = 0;
        let mut data_tx_metadata_updates = 0;
        let mut commitment_tx_metadata_updates = 0;

        // Helper function to update data tx metadata in DB with min height semantics
        let update_data_tx_metadata = |tx: &TX,
                                       tx_id: &H256,
                                       block_height: u64,
                                       set_included: bool,
                                       set_promoted: bool|
         -> Result<(), DatabaseError> {
            // Read existing metadata from DB
            let existing = tx.get::<IrysDataTxMetadata>(*tx_id)?;
            let mut metadata = existing.map(|w| w.0).unwrap_or_default();

            // Update with min height semantics
            if set_included {
                metadata.included_height = Some(
                    metadata
                        .included_height
                        .map_or(block_height, |existing| existing.min(block_height)),
                );
            }
            if set_promoted {
                metadata.promoted_height = Some(
                    metadata
                        .promoted_height
                        .map_or(block_height, |existing| existing.min(block_height)),
                );
            }

            // Write back to DB if there's actual metadata
            if metadata.included_height.is_some() || metadata.promoted_height.is_some() {
                tx.put::<IrysDataTxMetadata>(*tx_id, metadata.into())?;
            }
            Ok(())
        };

        // Helper function to update commitment tx metadata in DB with min height semantics
        let update_commitment_tx_metadata =
            |tx: &TX, tx_id: &H256, block_height: u64| -> Result<(), DatabaseError> {
                // Read existing metadata from DB
                let existing = tx.get::<IrysCommitmentTxMetadata>(*tx_id)?;
                let mut metadata = existing.map(|w| w.0).unwrap_or_default();

                // Update with min height semantics
                metadata.included_height = Some(
                    metadata
                        .included_height
                        .map_or(block_height, |existing| existing.min(block_height)),
                );

                // Write back to DB if there's actual metadata
                if metadata.included_height.is_some() {
                    tx.put::<IrysCommitmentTxMetadata>(*tx_id, metadata.into())?;
                }
                Ok(())
            };

        for result in block_cursor.walk(None)? {
            let (_block_hash, compact_header) = result?;
            let block_header: IrysBlockHeader = compact_header.into();
            let block_height = block_header.height();

            blocks_processed += 1;

            // Process Submit ledger transactions - set included_height
            // Use iter().find() to safely handle blocks that may not have this ledger
            if let Some(submit_ledger) = block_header
                .data_ledgers
                .iter()
                .find(|l| l.ledger_id == DataLedger::Submit as u32)
            {
                for tx_id in &submit_ledger.tx_ids.0 {
                    update_data_tx_metadata(tx, tx_id, block_height, true, false)?;
                    data_tx_metadata_updates += 1;
                }
            }

            // Process Publish ledger transactions - set included_height and promoted_height
            // Use iter().find() to safely handle blocks that may not have this ledger
            if let Some(publish_ledger) = block_header
                .data_ledgers
                .iter()
                .find(|l| l.ledger_id == DataLedger::Publish as u32)
            {
                for tx_id in &publish_ledger.tx_ids.0 {
                    update_data_tx_metadata(tx, tx_id, block_height, true, true)?;
                    data_tx_metadata_updates += 1;
                }
            }

            // Process Commitment ledger transactions - set included_height
            for ledger in SystemLedger::ALL {
                if let Some(tx_ledger) = block_header
                    .system_ledgers
                    .iter()
                    .find(|l| l.ledger_id == ledger as u32)
                {
                    for tx_id in &tx_ledger.tx_ids.0 {
                        update_commitment_tx_metadata(tx, tx_id, block_height)?;
                        commitment_tx_metadata_updates += 1;
                    }
                }
            }
        }

        debug!(
            "Processed {} blocks, created {} data tx metadata entries, {} commitment tx metadata entries",
            blocks_processed, data_tx_metadata_updates, commitment_tx_metadata_updates
        );

        crate::set_database_schema_version(tx, DatabaseVersion::V2)?;
        debug!(
            "Migration from v1 to v2 completed: migrated {} transaction headers, processed {} blocks",
            total_migrated, blocks_processed
        );
        Ok(())
    }
}

mod v2_to_v3 {
    use super::*;
    use irys_types::{
        DataTransactionHeader, DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata,
        DataTransactionMetadata,
    };
    use reth_db::cursor::DbCursorRO as _;

    fn migrate_batch<TX>(
        tx: &TX,
        entries: &[(
            irys_types::H256,
            v2_structures::DataTransactionHeaderV1WithBundleFormat,
        )],
    ) -> eyre::Result<()>
    where
        TX: DbTxMut + DbTx + Debug,
    {
        for (tx_id, v2_header) in entries {
            // bundle_format was Option<u64> but values were always small (0 or 1
            // in practice). The new u8 field uses 0 for "None / no/custom format",
            // matching the documented "None == Some(0) for RLP" equivalence.
            // Use checked conversion so an out-of-range legacy value fails the
            // migration instead of silently truncating into metadata_format.
            let bundle_format_u64 = v2_header.bundle_format.unwrap_or(0);
            let metadata_format = u8::try_from(bundle_format_u64).map_err(|_| {
                eyre::eyre!(
                    "bundle_format value {bundle_format_u64} for tx {tx_id:?} \
                     does not fit in u8 metadata_format"
                )
            })?;

            let new_inner = DataTransactionHeaderV1 {
                id: v2_header.id,
                anchor: v2_header.anchor,
                signer: v2_header.signer,
                data_root: v2_header.data_root,
                data_size: v2_header.data_size,
                header_size: v2_header.header_size,
                term_fee: v2_header.term_fee,
                ledger_id: v2_header.ledger_id,
                chain_id: v2_header.chain_id,
                signature: v2_header.signature,
                metadata_format,
                perm_fee: v2_header.perm_fee,
            };

            // Go through the public helper rather than `tx.put::<IrysDataTxHeaders>`
            let new_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
                tx: new_inner,
                metadata: DataTransactionMetadata::new(),
            });

            crate::insert_tx_header(tx, &new_header)?;
        }
        Ok(())
    }

    pub(crate) fn migrate<TX>(tx: &TX) -> eyre::Result<()>
    where
        TX: DbTxMut + DbTx + Debug,
    {
        debug!(
            "Migrating from v2 to v3: renaming bundle_format (Option<u64>) -> metadata_format (u8) on DataTransactionHeaderV1"
        );

        let mut v2_cursor = tx.cursor_read::<v2_structures::IrysDataTxHeadersV2>()?;
        let mut entries_to_migrate = Vec::new();
        const BATCH_SIZE: usize = 10000;
        let mut total_migrated = 0;

        for result in v2_cursor.walk(None)? {
            let (tx_id, v2_compact_header) = result?;
            let v2_structures::DataTransactionHeaderV2::V1(v2_inner) = v2_compact_header.0;
            entries_to_migrate.push((tx_id, v2_inner));

            if entries_to_migrate.len() >= BATCH_SIZE {
                migrate_batch(tx, &entries_to_migrate)?;
                total_migrated += entries_to_migrate.len();
                entries_to_migrate.clear();
            }
        }

        if !entries_to_migrate.is_empty() {
            migrate_batch(tx, &entries_to_migrate)?;
            total_migrated += entries_to_migrate.len();
        }

        crate::set_database_schema_version(tx, DatabaseVersion::V3)?;
        debug!(
            "Migration from v2 to v3 completed: migrated {} transaction headers",
            total_migrated
        );
        Ok(())
    }
}

/// Checks the database schema version on startup and either:
/// - Migrates forward from V0/V1/V2 through to CURRENT
/// - Returns an error if the DB version is newer than the binary (rollback not supported)
/// - No-ops if versions match
///
/// A database without a version stamp is treated as V0 (the versioning system
/// didn't exist yet). The V0→V1 transition is purely "add the stamp" — no data
/// format change — so we unconditionally stamp V1 and then run V1→V2 and V2→V3.
/// On a truly fresh (empty) database the per-step migrations are no-ops.
pub fn ensure_db_version_compatible(db: &DatabaseEnv) -> eyre::Result<()> {
    use reth_db::Database as _;

    let raw_version = db.view(crate::database_schema_version)??;

    // No version stamp → V0 database (or brand-new). Stamp V1 so the
    // migration chain below handles it uniformly.
    let raw = match raw_version {
        Some(v) => v,
        None => {
            debug!("No database version stamp found — treating as V0, stamping V1.");
            db.update_eyre(|tx| {
                crate::set_database_schema_version(tx, DatabaseVersion::V1)?;
                Ok(())
            })?;
            DatabaseVersion::V1 as u32
        }
    };

    // Try to convert the stored u32 into a known DatabaseVersion variant.
    // If the conversion fails the DB was written by a newer binary.
    let Some(version) = DatabaseVersion::from_u32(raw) else {
        eyre::bail!(
            "Database schema version {} is newer than this binary supports \
             (version {}). Running an older binary against a newer database is not \
             supported. Use the newer binary or restore the database from a backup.",
            raw,
            DatabaseVersion::CURRENT
        );
    };

    // Migrate forward one version at a time until we reach CURRENT.
    // The match enumerates every variant so the compiler forces an update
    // when a new DatabaseVersion is added.
    let mut version = version;
    loop {
        match version {
            DatabaseVersion::V0 => {
                // Shouldn't happen (we stamp V1 above for unstamped DBs), but
                // handle defensively in case a V0 value was written explicitly.
                debug!("Explicit V0 stamp found — upgrading to V1.");
                db.update_eyre(|tx| {
                    crate::set_database_schema_version(tx, DatabaseVersion::V1)?;
                    Ok(())
                })?;
                version = DatabaseVersion::V1;
            }
            DatabaseVersion::V1 => {
                debug!("Applying migration from V1 to V2");
                db.update_eyre(|tx| {
                    v1_to_v2::migrate(tx)?;
                    Ok(())
                })?;
                version = DatabaseVersion::V2;
            }
            DatabaseVersion::V2 => {
                debug!("Applying migration from V2 to V3");
                db.update_eyre(|tx| {
                    v2_to_v3::migrate(tx)?;
                    Ok(())
                })?;
                version = DatabaseVersion::V3;
            }
            DatabaseVersion::CURRENT => {
                debug!(
                    "Database schema is up-to-date (V{})",
                    DatabaseVersion::CURRENT
                );
                break;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tables::{IrysBlockHeaders, IrysDataTxHeaders, IrysTables};
    use crate::{IrysDatabaseArgs as _, open_or_create_db};
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{H256, IrysBlockHeader};
    use reth_db::mdbx::DatabaseArguments;
    use reth_db_api::transaction::{DbTx as _, DbTxMut as _};
    use reth_db_api::{Database as _, DatabaseError};

    use irys_types::DatabaseVersion;

    // Test ensures v1→v2 migration correctly moves promoted_height to IrysDataTxMetadata
    #[test]
    fn should_migrate_from_v1_to_v2() -> eyre::Result<()> {
        use crate::tables::IrysDataTxMetadata;

        let db_path = TempDirBuilder::new().build();
        let db = open_or_create_db(
            db_path.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;

        // Set schema version to 1
        let _ = db.update(|tx| -> Result<(), DatabaseError> {
            crate::set_database_schema_version(tx, DatabaseVersion::V1)?;
            Ok(())
        })?;

        let version = db.view(|tx| crate::database_schema_version(tx).unwrap())?;
        assert_eq!(version, Some(DatabaseVersion::V1 as u32));

        // Create test data in OLD format (with promoted_height field)
        let tx_id_1: H256 = H256::random();
        let tx_id_2: H256 = H256::random();
        let tx_id_3: H256 = H256::random();

        {
            let write_tx = db.tx_mut()?;

            // Write using old table type
            let old_header_1 = super::old_structures::DataTransactionHeaderOld::V1(
                super::old_structures::DataTransactionHeaderV1WithPromotedHeight {
                    id: tx_id_1,
                    promoted_height: Some(100),
                    ..Default::default()
                },
            );
            write_tx.put::<super::old_structures::IrysDataTxHeadersOld>(
                tx_id_1,
                super::old_structures::CompactTxHeaderOld(old_header_1),
            )?;

            let old_header_2 = super::old_structures::DataTransactionHeaderOld::V1(
                super::old_structures::DataTransactionHeaderV1WithPromotedHeight {
                    id: tx_id_2,
                    promoted_height: Some(200),
                    ..Default::default()
                },
            );
            write_tx.put::<super::old_structures::IrysDataTxHeadersOld>(
                tx_id_2,
                super::old_structures::CompactTxHeaderOld(old_header_2),
            )?;

            let old_header_3 = super::old_structures::DataTransactionHeaderOld::V1(
                super::old_structures::DataTransactionHeaderV1WithPromotedHeight {
                    id: tx_id_3,
                    promoted_height: None, // No promoted_height for this one
                    ..Default::default()
                },
            );
            write_tx.put::<super::old_structures::IrysDataTxHeadersOld>(
                tx_id_3,
                super::old_structures::CompactTxHeaderOld(old_header_3),
            )?;

            write_tx.commit()?;
        }

        // Run migration
        let _ = db.update(|tx| -> Result<(), DatabaseError> {
            super::v1_to_v2::migrate(tx)?;
            Ok(())
        })?;

        // Verify that a schema version is updated to 2
        let new_version = db.view(|tx| crate::database_schema_version(tx).unwrap())?;
        assert_eq!(new_version.unwrap(), DatabaseVersion::V2 as u32);

        // Headers should now be in the V2 on-disk shape (with bundle_format,
        // no inline promoted_height). Read via IrysDataTxHeadersV2 to verify.
        let header_1 = db
            .view(|tx| tx.get::<super::v2_structures::IrysDataTxHeadersV2>(tx_id_1))??
            .expect("tx_id_1 should be present after V1→V2");
        let super::v2_structures::DataTransactionHeaderV2::V1(h1_inner) = header_1.0;
        assert_eq!(h1_inner.id, tx_id_1);

        let header_2 = db
            .view(|tx| tx.get::<super::v2_structures::IrysDataTxHeadersV2>(tx_id_2))??
            .expect("tx_id_2 should be present after V1→V2");
        let super::v2_structures::DataTransactionHeaderV2::V1(h2_inner) = header_2.0;
        assert_eq!(h2_inner.id, tx_id_2);

        let header_3 = db
            .view(|tx| tx.get::<super::v2_structures::IrysDataTxHeadersV2>(tx_id_3))??
            .expect("tx_id_3 should be present after V1→V2");
        let super::v2_structures::DataTransactionHeaderV2::V1(h3_inner) = header_3.0;
        assert_eq!(h3_inner.id, tx_id_3);

        // Verify metadata table has the correct entries
        let metadata_1_check = db.view(|tx| tx.get::<IrysDataTxMetadata>(tx_id_1))??;
        assert!(metadata_1_check.is_some());
        let metadata_1_inner = metadata_1_check.unwrap().0;
        assert_eq!(metadata_1_inner.promoted_height, Some(100));
        // Verify included_height is None after migration
        assert_eq!(metadata_1_inner.included_height, None);

        let metadata_2_check = db.view(|tx| tx.get::<IrysDataTxMetadata>(tx_id_2))??;
        assert!(metadata_2_check.is_some());
        let metadata_2_inner = metadata_2_check.unwrap().0;
        assert_eq!(metadata_2_inner.promoted_height, Some(200));
        // Verify included_height is None after migration
        assert_eq!(metadata_2_inner.included_height, None);

        // tx_id_3 should NOT have a metadata entry (promoted_height was None)
        let metadata_3_check = db.view(|tx| tx.get::<IrysDataTxMetadata>(tx_id_3))??;
        assert!(metadata_3_check.is_none());

        Ok(())
    }

    // Test ensures v1→v2 migration Step 2 correctly fills included_height from block headers
    #[test]
    fn should_migrate_v1_to_v2_with_block_header_metadata_filling() -> eyre::Result<()> {
        use crate::tables::{IrysCommitmentTxMetadata, IrysDataTxMetadata};
        use irys_types::{
            DataLedger, DataTransactionLedger, H256List, IrysBlockHeaderV1, SystemLedger,
            SystemTransactionLedger,
        };

        let db_path = TempDirBuilder::new().build();
        let db = open_or_create_db(
            db_path.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;

        // Set schema version to 1
        let _ = db.update(|tx| -> Result<(), DatabaseError> {
            crate::set_database_schema_version(tx, DatabaseVersion::V1)?;
            Ok(())
        })?;

        let version = db.view(|tx| crate::database_schema_version(tx).unwrap())?;
        assert_eq!(version, Some(DatabaseVersion::V1 as u32));

        // Create test data: transaction IDs
        let submit_tx_id: H256 = H256::random();
        let publish_tx_id: H256 = H256::random();
        let commitment_tx_id: H256 = H256::random();

        // Create old-format headers with promoted_height for data transactions
        {
            let write_tx = db.tx_mut()?;

            // Insert old format headers (with promoted_height field)
            let old_header_submit = super::old_structures::DataTransactionHeaderOld::V1(
                super::old_structures::DataTransactionHeaderV1WithPromotedHeight {
                    id: submit_tx_id,
                    promoted_height: None, // Submit tx has no promoted_height initially
                    ..Default::default()
                },
            );
            write_tx.put::<super::old_structures::IrysDataTxHeadersOld>(
                submit_tx_id,
                super::old_structures::CompactTxHeaderOld(old_header_submit),
            )?;

            let old_header_publish = super::old_structures::DataTransactionHeaderOld::V1(
                super::old_structures::DataTransactionHeaderV1WithPromotedHeight {
                    id: publish_tx_id,
                    promoted_height: Some(150), // Publish tx has promoted_height set
                    ..Default::default()
                },
            );
            write_tx.put::<super::old_structures::IrysDataTxHeadersOld>(
                publish_tx_id,
                super::old_structures::CompactTxHeaderOld(old_header_publish),
            )?;

            write_tx.commit()?;
        }

        // Create block headers that include these transaction IDs in their ledgers
        {
            let write_tx = db.tx_mut()?;

            // Block at height 100 with Submit ledger containing submit_tx_id
            let block_hash_100 = H256::random();
            let header_100 = IrysBlockHeader::V1(IrysBlockHeaderV1 {
                block_hash: block_hash_100,
                height: 100,
                data_ledgers: vec![DataTransactionLedger {
                    ledger_id: DataLedger::Submit as u32,
                    tx_ids: H256List(vec![submit_tx_id]),
                    ..Default::default()
                }],
                ..Default::default()
            });
            write_tx.put::<IrysBlockHeaders>(block_hash_100, header_100.into())?;

            // Block at height 200 with Publish ledger containing publish_tx_id
            let block_hash_200 = H256::random();
            let header_200 = IrysBlockHeader::V1(IrysBlockHeaderV1 {
                block_hash: block_hash_200,
                height: 200,
                data_ledgers: vec![DataTransactionLedger {
                    ledger_id: DataLedger::Publish as u32,
                    tx_ids: H256List(vec![publish_tx_id]),
                    ..Default::default()
                }],
                ..Default::default()
            });
            write_tx.put::<IrysBlockHeaders>(block_hash_200, header_200.into())?;

            // Block at height 300 with Commitment ledger containing commitment_tx_id
            let block_hash_300 = H256::random();
            let header_300 = IrysBlockHeader::V1(IrysBlockHeaderV1 {
                block_hash: block_hash_300,
                height: 300,
                system_ledgers: vec![SystemTransactionLedger {
                    ledger_id: SystemLedger::Commitment as u32,
                    tx_ids: H256List(vec![commitment_tx_id]),
                }],
                ..Default::default()
            });
            write_tx.put::<IrysBlockHeaders>(block_hash_300, header_300.into())?;

            write_tx.commit()?;
        }

        // Run migration from v1 to v2
        let _ = db.update(|tx| -> Result<(), DatabaseError> {
            super::v1_to_v2::migrate(tx)?;
            Ok(())
        })?;

        // Verify schema version is updated to 2
        let new_version = db.view(|tx| crate::database_schema_version(tx).unwrap())?;
        assert_eq!(new_version.unwrap(), DatabaseVersion::V2 as u32);

        // Verify IrysDataTxMetadata was created/updated correctly

        // submit_tx_id: should have included_height=100 from Submit ledger
        let submit_metadata = db
            .view(|tx| tx.get::<IrysDataTxMetadata>(submit_tx_id))??
            .expect("submit_tx should have metadata");
        assert_eq!(
            submit_metadata.0.included_height,
            Some(100),
            "submit_tx should have included_height=100 from Submit ledger"
        );
        assert_eq!(
            submit_metadata.0.promoted_height, None,
            "submit_tx should not have promoted_height"
        );

        // publish_tx_id: should have included_height=200 from Publish ledger AND promoted_height=150 from old header
        let publish_metadata = db
            .view(|tx| tx.get::<IrysDataTxMetadata>(publish_tx_id))??
            .expect("publish_tx should have metadata");
        assert_eq!(
            publish_metadata.0.included_height,
            Some(200),
            "publish_tx should have included_height=200 from Publish ledger"
        );
        // Note: Migration preserves old promoted_height and also sets it from Publish ledger
        // Min semantics apply: min(150 from old format, 200 from Publish) = 150
        assert_eq!(
            publish_metadata.0.promoted_height,
            Some(150),
            "publish_tx should have promoted_height=150 (min of old value and Publish block)"
        );

        // commitment_tx_id: should have included_height=300 from Commitment ledger
        let commitment_metadata = db
            .view(|tx| tx.get::<IrysCommitmentTxMetadata>(commitment_tx_id))??
            .expect("commitment_tx should have metadata");
        assert_eq!(
            commitment_metadata.0.included_height,
            Some(300),
            "commitment_tx should have included_height=300 from Commitment ledger"
        );

        Ok(())
    }

    // Test ensures v2→v3 migration renames bundle_format -> metadata_format
    // on existing V2 records, leaving IrysDataTxMetadata untouched.
    #[test]
    fn should_migrate_from_v2_to_v3() -> eyre::Result<()> {
        use crate::tables::IrysDataTxMetadata;
        use irys_types::DataTransactionMetadata;

        let db_path = TempDirBuilder::new().build();
        let db = open_or_create_db(
            db_path.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;

        // Stamp V2 to simulate a database that has already run V1→V2.
        let _ = db.update(|tx| -> Result<(), DatabaseError> {
            crate::set_database_schema_version(tx, DatabaseVersion::V2)?;
            Ok(())
        })?;

        let tx_id_none: H256 = H256::random();
        let tx_id_zero: H256 = H256::random();
        let tx_id_one: H256 = H256::random();

        // Pre-populate IrysDataTxMetadata rows so we can prove V2→V3 leaves
        // them byte-for-byte intact (the migration must only rewrite the
        // header table, never the metadata table).
        let metadata_none = DataTransactionMetadata {
            included_height: Some(10),
            promoted_height: None,
        };
        let metadata_zero = DataTransactionMetadata {
            included_height: Some(20),
            promoted_height: Some(25),
        };
        // tx_id_one intentionally has no metadata row, to verify V2→V3 doesn't
        // create one for headers that don't have one.

        // Write three V2-shape records with different bundle_format values.
        {
            let write_tx = db.tx_mut()?;
            for (tx_id, bundle_format) in [
                (tx_id_none, None),
                (tx_id_zero, Some(0_u64)),
                (tx_id_one, Some(1_u64)),
            ] {
                let header = super::v2_structures::DataTransactionHeaderV2::V1(
                    super::v2_structures::DataTransactionHeaderV1WithBundleFormat {
                        id: tx_id,
                        bundle_format,
                        ..Default::default()
                    },
                );
                write_tx.put::<super::v2_structures::IrysDataTxHeadersV2>(
                    tx_id,
                    super::v2_structures::CompactTxHeaderV2(header),
                )?;
            }
            write_tx.put::<IrysDataTxMetadata>(tx_id_none, metadata_none.into())?;
            write_tx.put::<IrysDataTxMetadata>(tx_id_zero, metadata_zero.into())?;
            write_tx.commit()?;
        }

        // Run V2→V3.
        let _ = db.update(|tx| -> eyre::Result<()> {
            super::v2_to_v3::migrate(tx)?;
            Ok(())
        })?;

        // Schema is now V3.
        let new_version = db.view(|tx| crate::database_schema_version(tx).unwrap())?;
        assert_eq!(new_version.unwrap(), DatabaseVersion::V3 as u32);

        // Records should now decode via the current IrysDataTxHeaders, with
        // bundle_format converted to metadata_format.
        let h_none = db
            .view(|tx| tx.get::<IrysDataTxHeaders>(tx_id_none))??
            .expect("tx_id_none should exist");
        assert_eq!(h_none.0.metadata_format, 0); // None => 0
        let h_zero = db
            .view(|tx| tx.get::<IrysDataTxHeaders>(tx_id_zero))??
            .expect("tx_id_zero should exist");
        assert_eq!(h_zero.0.metadata_format, 0);
        let h_one = db
            .view(|tx| tx.get::<IrysDataTxHeaders>(tx_id_one))??
            .expect("tx_id_one should exist");
        assert_eq!(h_one.0.metadata_format, 1);

        // IrysDataTxMetadata rows must be untouched by V2→V3.
        let metadata_none_after = db
            .view(|tx| tx.get::<IrysDataTxMetadata>(tx_id_none))??
            .expect("tx_id_none metadata should still exist");
        assert_eq!(metadata_none_after.0, metadata_none);
        let metadata_zero_after = db
            .view(|tx| tx.get::<IrysDataTxMetadata>(tx_id_zero))??
            .expect("tx_id_zero metadata should still exist");
        assert_eq!(metadata_zero_after.0, metadata_zero);
        let metadata_one_after = db.view(|tx| tx.get::<IrysDataTxMetadata>(tx_id_one))??;
        assert!(
            metadata_one_after.is_none(),
            "V2→V3 must not invent a metadata row where none existed"
        );

        Ok(())
    }

    // V2 records whose bundle_format does not fit in u8 must fail the migration
    // rather than be silently truncated.
    #[test]
    fn v2_to_v3_rejects_bundle_format_overflow() -> eyre::Result<()> {
        let db_path = TempDirBuilder::new().build();
        let db = open_or_create_db(
            db_path.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;

        let _ = db.update(|tx| -> Result<(), DatabaseError> {
            crate::set_database_schema_version(tx, DatabaseVersion::V2)?;
            Ok(())
        })?;

        let tx_id: H256 = H256::random();
        {
            let write_tx = db.tx_mut()?;
            let header = super::v2_structures::DataTransactionHeaderV2::V1(
                super::v2_structures::DataTransactionHeaderV1WithBundleFormat {
                    id: tx_id,
                    bundle_format: Some(u64::from(u8::MAX) + 1),
                    ..Default::default()
                },
            );
            write_tx.put::<super::v2_structures::IrysDataTxHeadersV2>(
                tx_id,
                super::v2_structures::CompactTxHeaderV2(header),
            )?;
            write_tx.commit()?;
        }

        let result = db.update(|tx| -> eyre::Result<()> {
            super::v2_to_v3::migrate(tx)?;
            Ok(())
        })?;
        assert!(
            result.is_err(),
            "v2_to_v3 must reject bundle_format that overflows u8"
        );

        Ok(())
    }

    #[test]
    fn test_fresh_db_gets_current_version_stamped() {
        use crate::migration::ensure_db_version_compatible;
        let dir = TempDirBuilder::new().build();
        let db = open_or_create_db(
            dir.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();
        ensure_db_version_compatible(&db).unwrap();
        let version = db
            .view(|tx| crate::database_schema_version(tx).unwrap())
            .unwrap();
        assert_eq!(version, Some(DatabaseVersion::CURRENT as u32));
    }

    #[test]
    fn test_matching_version_passes() {
        use crate::db::IrysDatabaseExt as _;
        use crate::migration::ensure_db_version_compatible;

        let dir = TempDirBuilder::new().build();
        let db = open_or_create_db(
            dir.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();
        db.update_eyre(|tx| {
            crate::set_database_schema_version(tx, DatabaseVersion::CURRENT)?;
            Ok(())
        })
        .unwrap();
        ensure_db_version_compatible(&db).unwrap();
    }

    #[test]
    fn test_newer_db_version_is_rejected() {
        use crate::db::IrysDatabaseExt as _;
        use crate::metadata::MetadataKey;
        use crate::migration::ensure_db_version_compatible;
        use crate::tables::Metadata;

        let dir = TempDirBuilder::new().build();
        let db = open_or_create_db(
            dir.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();
        // Write a version higher than CURRENT directly to simulate a newer binary's DB
        let future_version = DatabaseVersion::CURRENT as u32 + 1;
        db.update_eyre(|tx| {
            tx.put::<Metadata>(
                MetadataKey::DBSchemaVersion,
                future_version.to_le_bytes().to_vec(),
            )?;
            Ok(())
        })
        .unwrap();
        let result = ensure_db_version_compatible(&db);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("newer than this binary supports"));
    }

    #[test]
    fn test_unstamped_db_migrates_to_current() {
        use crate::migration::ensure_db_version_compatible;

        let dir = TempDirBuilder::new().build();
        let db = open_or_create_db(
            dir.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();

        // No schema version set — simulates a V0 / fresh database.
        // Should stamp V1 then migrate V1 → V2.
        ensure_db_version_compatible(&db).unwrap();

        let version = db
            .view(|tx| crate::database_schema_version(tx).unwrap())
            .unwrap();
        assert_eq!(version, Some(DatabaseVersion::CURRENT as u32));
    }

    #[test]
    fn test_unstamped_db_with_data_migrates_to_current() {
        use crate::db::IrysDatabaseExt as _;
        use crate::migration::ensure_db_version_compatible;
        use crate::tables::IrysDataTxMetadata;

        let dir = TempDirBuilder::new().build();
        let db = open_or_create_db(
            dir.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();

        let legacy_tx_id = H256::random();

        // Write data WITHOUT setting a schema version — simulates mainnet-0.1.x (V0).
        db.update_eyre(|tx| {
            // Insert a block header (existing coverage).
            let block_hash = H256::random();
            let header = IrysBlockHeader::default();
            tx.put::<IrysBlockHeaders>(block_hash, header.into())?;

            // Insert a legacy-format tx header with promoted_height to exercise
            // the v1→v2 rewrite path.
            let old_header = super::old_structures::DataTransactionHeaderOld::V1(
                super::old_structures::DataTransactionHeaderV1WithPromotedHeight {
                    id: legacy_tx_id,
                    promoted_height: Some(42),
                    ..Default::default()
                },
            );
            tx.put::<super::old_structures::IrysDataTxHeadersOld>(
                legacy_tx_id,
                super::old_structures::CompactTxHeaderOld(old_header),
            )?;
            Ok(())
        })
        .unwrap();

        // Should stamp V1 then migrate V1 → V2 (tx header rewrite + metadata back-fill)
        ensure_db_version_compatible(&db).unwrap();

        // Verify schema version is CURRENT.
        let version = db
            .view(|tx| crate::database_schema_version(tx).unwrap())
            .unwrap();
        assert_eq!(version, Some(DatabaseVersion::CURRENT as u32));

        // Verify the legacy record was rewritten into the new IrysDataTxHeaders shape.
        let migrated_header = db
            .view(|tx| tx.get::<IrysDataTxHeaders>(legacy_tx_id))
            .unwrap()
            .unwrap()
            .expect("legacy tx header should exist in new table after migration");
        assert_eq!(migrated_header.0.id(), legacy_tx_id);

        // Verify promoted_height was split out into IrysDataTxMetadata.
        let metadata = db
            .view(|tx| tx.get::<IrysDataTxMetadata>(legacy_tx_id))
            .unwrap()
            .unwrap()
            .expect("metadata entry should exist for legacy record with promoted_height");
        assert_eq!(metadata.0.promoted_height, Some(42));
        assert_eq!(metadata.0.included_height, None);
    }

    #[test]
    fn test_older_db_version_migrates_forward() {
        use crate::db::IrysDatabaseExt as _;
        use crate::migration::ensure_db_version_compatible;

        let dir = TempDirBuilder::new().build();
        let db = open_or_create_db(
            dir.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();
        db.update_eyre(|tx| {
            crate::set_database_schema_version(tx, DatabaseVersion::V1)?;
            Ok(())
        })
        .unwrap();
        ensure_db_version_compatible(&db).unwrap();
        let version = db
            .view(|tx| crate::database_schema_version(tx).unwrap())
            .unwrap();
        assert_eq!(version, Some(DatabaseVersion::CURRENT as u32));
    }
}
