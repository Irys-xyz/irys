use crate::db::{IrysDatabaseExt as _, RethDbWrapper};

use crate::reth_db::{
    table::TableImporter,
    transaction::{DbTx, DbTxMut},
    Database as _, DatabaseEnv, DatabaseError,
};

use std::fmt::Debug;
use tracing::debug;

/// Bump this every time you need to migrate data
const CURRENT_DB_VERSION: u32 = 1;

mod v0_to_v1 {
    use super::*;
    use crate::tables::{
        CachedChunks, CachedChunksIndex, CachedDataRoots, IngressProofs, IrysBlockHeaders,
        IrysDataTxHeaders,
    };
    use reth_db::cursor::DbCursorRO as _;
    use reth_db::table::Table;

    pub(crate) fn migrate<TXOld, TXNew>(tx_old: &TXOld, tx_new: &TXNew) -> Result<(), DatabaseError>
    where
        TXOld: DbTxMut + DbTx + Debug,
        TXNew: DbTxMut + DbTx + Debug + TableImporter,
    {
        debug!("Migrating from v0 to v1");
        move_all_records::<IrysBlockHeaders, TXOld, TXNew>(tx_old, tx_new)?;
        move_all_records::<IrysDataTxHeaders, TXOld, TXNew>(tx_old, tx_new)?;
        move_all_records::<CachedDataRoots, TXOld, TXNew>(tx_old, tx_new)?;
        move_all_records::<CachedChunksIndex, TXOld, TXNew>(tx_old, tx_new)?;
        move_all_records::<CachedChunks, TXOld, TXNew>(tx_old, tx_new)?;
        move_all_records::<IngressProofs, TXOld, TXNew>(tx_old, tx_new)?;

        crate::set_database_schema_version(tx_new, 1)?;
        Ok(())
    }

    fn move_all_records<T: Table, TXOld, TXNew>(
        tx_old: &TXOld,
        tx_new: &TXNew,
    ) -> Result<(), DatabaseError>
    where
        TXOld: DbTxMut + DbTx + Debug,
        TXNew: DbTxMut + DbTx + Debug + TableImporter,
    {
        debug!("Migrating table: {}", T::NAME);
        let mut binding = tx_old.cursor_read::<T>()?;
        let entries = binding.walk(None)?;

        // Insert entries into new DB
        for table_row in entries {
            let (key, value) = table_row?;
            tx_new.put::<T>(key, value)?;
        }

        tx_old.clear::<T>()
    }
}

/// This function migrates data from an old DB instance to a new DB instance.
pub fn check_db_version_and_run_migrations_if_needed(
    old_db: &RethDbWrapper,
    new_db: &DatabaseEnv,
) -> eyre::Result<()> {
    debug!("Checking if database migration is needed.");
    let version = new_db.view(crate::database_schema_version)??;
    debug!("Database version: {:?}", version);
    debug!("Current database version: {:?}", CURRENT_DB_VERSION);
    if let Some(v) = version {
        // A version exists. If it’s less than CURRENT_DB_VERSION, apply sequential migrations.
        if v < CURRENT_DB_VERSION {}
    } else {
        debug!("No DB schema version information found in the new database. Applying initial migration from v0 to v1.");
        old_db.update_eyre(|tx_old| {
            new_db.update_eyre(|tx_new| {
                v0_to_v1::migrate(tx_old, tx_new)?;
                Ok(())
            })
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::db::RethDbWrapper;
    use crate::db_cache::{
        CachedChunk, CachedChunkIndexEntry, CachedChunkIndexMetadata, CachedDataRoot,
    };
    use crate::migration::check_db_version_and_run_migrations_if_needed;
    use crate::open_or_create_db;
    use crate::tables::IrysTables;
    use crate::tables::{
        CachedChunks, CachedChunksIndex, CachedDataRoots, IngressProofs, IrysBlockHeaders,
        IrysDataTxHeaders,
    };
    use irys_testing_utils::utils::temporary_directory;
    use irys_types::ingress::CachedIngressProof;
    use irys_types::{
        Address, Base64, ChunkPathHash, DataRoot, DataTransactionHeader, IrysBlockHeader,
        TxChunkOffset, H256,
    };
    use reth_db_api::transaction::{DbTx as _, DbTxMut as _};
    use reth_db_api::Database as _;

    // test ensures v0→v1 migration moves representative records to the new DB, clears old DB tables, and sets the schema version.
    #[test]
    fn should_migrate_from_v0_to_v1() -> Result<(), Box<dyn std::error::Error>> {
        // Create separate old and new DBs with no schema version set
        let old_db_path = temporary_directory(None, false);
        let old_db = RethDbWrapper::new(open_or_create_db(old_db_path, IrysTables::ALL, None)?);

        let new_db_path = temporary_directory(None, false);
        let new_db = open_or_create_db(new_db_path, IrysTables::ALL, None)?;

        let old_version = old_db.view(|tx| crate::database_schema_version(tx).unwrap())?;
        let new_version = new_db.view(|tx| crate::database_schema_version(tx).unwrap())?;
        assert!(old_version.is_none());
        assert!(new_version.is_none());

        // Insert one entry per representative table (including dupsort tables) into the old DB
        let block_hash: H256 = H256::random();
        let tx_id: H256 = H256::random();
        let data_root: DataRoot = H256::random();
        let chunk_path_hash: ChunkPathHash = H256::random();
        let address: Address = Address::random();

        {
            let write_tx = old_db.tx_mut()?;

            // IrysBlockHeaders (non-dupsort)
            let header = IrysBlockHeader {
                block_hash,
                height: 1,
                ..Default::default()
            };
            write_tx.put::<IrysBlockHeaders>(block_hash, header.into())?;

            // IrysDataTxHeaders (non-dupsort)
            let tx_header = DataTransactionHeader::default();
            write_tx.put::<IrysDataTxHeaders>(tx_id, tx_header.into())?;

            // CachedDataRoots (non-dupsort)
            let cdr = CachedDataRoot {
                data_size: 1,
                txid_set: vec![tx_id],
                block_set: vec![block_hash],
                expiry_height: None,
            };
            write_tx.put::<CachedDataRoots>(data_root, cdr)?;

            // CachedChunksIndex (dupsort with subkey = u32 encoded inside value)
            let index_entry = CachedChunkIndexEntry {
                index: TxChunkOffset::from(0),
                meta: CachedChunkIndexMetadata { chunk_path_hash },
            };
            write_tx.put::<CachedChunksIndex>(data_root, index_entry)?;

            // CachedChunks (non-dupsort key = ChunkPathHash)
            let chunk = CachedChunk {
                chunk: None,
                data_path: Base64(vec![]),
            };
            write_tx.put::<CachedChunks>(chunk_path_hash, chunk)?;

            // IngressProofs (dupsort with subkey = Address encoded inside value)
            let cached_proof = CachedIngressProof {
                address,
                ..Default::default()
            };
            write_tx.put::<IngressProofs>(data_root, cached_proof.into())?;

            write_tx.commit()?;
        }

        // Verify counts in old DB (1 each) and new DB (0 each) before migration
        let old_counts_pre = old_db.view(
            |tx| -> eyre::Result<(usize, usize, usize, usize, usize, usize)> {
                Ok((
                    tx.entries::<IrysBlockHeaders>()?,
                    tx.entries::<IrysDataTxHeaders>()?,
                    tx.entries::<CachedDataRoots>()?,
                    tx.entries::<CachedChunksIndex>()?,
                    tx.entries::<CachedChunks>()?,
                    tx.entries::<IngressProofs>()?,
                ))
            },
        )??;
        assert_eq!(old_counts_pre, (1, 1, 1, 1, 1, 1));

        let new_counts_pre = new_db.view(
            |tx| -> eyre::Result<(usize, usize, usize, usize, usize, usize)> {
                Ok((
                    tx.entries::<IrysBlockHeaders>()?,
                    tx.entries::<IrysDataTxHeaders>()?,
                    tx.entries::<CachedDataRoots>()?,
                    tx.entries::<CachedChunksIndex>()?,
                    tx.entries::<CachedChunks>()?,
                    tx.entries::<IngressProofs>()?,
                ))
            },
        )??;
        assert_eq!(new_counts_pre, (0, 0, 0, 0, 0, 0));

        // Run migration from v0 to v1
        check_db_version_and_run_migrations_if_needed(&old_db, &new_db)?;

        // Verify new DB now contains those entries (1 each)
        let new_counts_post = new_db.view(
            |tx| -> eyre::Result<(usize, usize, usize, usize, usize, usize)> {
                Ok((
                    tx.entries::<IrysBlockHeaders>()?,
                    tx.entries::<IrysDataTxHeaders>()?,
                    tx.entries::<CachedDataRoots>()?,
                    tx.entries::<CachedChunksIndex>()?,
                    tx.entries::<CachedChunks>()?,
                    tx.entries::<IngressProofs>()?,
                ))
            },
        )??;
        assert_eq!(new_counts_post, (1, 1, 1, 1, 1, 1));

        // Verify old DB was cleared by migration (0 each)
        let old_counts_post = old_db.view(
            |tx| -> eyre::Result<(usize, usize, usize, usize, usize, usize)> {
                Ok((
                    tx.entries::<IrysBlockHeaders>()?,
                    tx.entries::<IrysDataTxHeaders>()?,
                    tx.entries::<CachedDataRoots>()?,
                    tx.entries::<CachedChunksIndex>()?,
                    tx.entries::<CachedChunks>()?,
                    tx.entries::<IngressProofs>()?,
                ))
            },
        )??;
        assert_eq!(old_counts_post, (0, 0, 0, 0, 0, 0));

        // Schema version should be set to CURRENT_DB_VERSION (1)
        let new_version = new_db.view(|tx| crate::database_schema_version(tx).unwrap())?;
        assert_eq!(new_version.unwrap(), 1);

        Ok(())
    }
}
