use crate::db::RethDbWrapper;
use crate::reth_db::{
    table::TableImporter,
    transaction::{DbTx, DbTxMut},
    Database as _, DatabaseEnv, DatabaseError,
};
use core::fmt::Debug;
use tracing::debug;

/// Bump this every time you need to migrate data
const CURRENT_DB_VERSION: u32 = 1;

/// Example migration step to version 2
const fn migration_to_v2(_db: &DatabaseEnv) -> Result<(), DatabaseError> {
    // template for future migrations
    // update the database schema version here
    Ok(())
}

mod v0_to_v1 {
    use super::*;
    use crate::tables::{
        CachedChunks, CachedChunksIndex, CachedDataRoots, IngressProofLRU, IngressProofs,
        IrysBlockHeaders, IrysTxHeaders,
    };
    use reth_db::table::Table;
    use reth_db_api::cursor::DbCursorRO as _;

    pub(crate) fn migrate<TXOld, TXNew>(tx_old: &TXOld, tx_new: &TXNew) -> Result<(), DatabaseError>
    where
        TXOld: DbTxMut + DbTx + Debug,
        TXNew: DbTxMut + DbTx + Debug + TableImporter,
    {
        debug!("Migrating from v0 to v1");
        move_all_records::<IrysBlockHeaders, TXOld, TXNew>(tx_old, tx_new)?;
        move_all_records::<IrysTxHeaders, TXOld, TXNew>(tx_old, tx_new)?;
        move_all_records::<CachedDataRoots, TXOld, TXNew>(tx_old, tx_new)?;
        move_all_records::<CachedChunksIndex, TXOld, TXNew>(tx_old, tx_new)?;
        move_all_records::<CachedChunks, TXOld, TXNew>(tx_old, tx_new)?;
        move_all_records::<IngressProofs, TXOld, TXNew>(tx_old, tx_new)?;
        move_all_records::<IngressProofLRU, TXOld, TXNew>(tx_old, tx_new)?;

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
        if v < CURRENT_DB_VERSION {
            for next_version in (v + 1)..=CURRENT_DB_VERSION {
                match next_version {
                    // This is a template for future migrations.
                    2 => migration_to_v2(new_db)?,
                    _ => (),
                }
            }
        }
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
    use crate::migration::check_db_version_and_run_migrations_if_needed;
    use crate::open_or_create_db;
    use crate::tables::{IngressProofLRU, IrysTables};
    use irys_testing_utils::utils::temporary_directory;
    use irys_types::H256;
    use reth_db_api::transaction::{DbTx, DbTxMut};
    use reth_db_api::Database;

    #[test]
    fn should_migrate_from_v0_to_v1() -> Result<(), Box<dyn std::error::Error>> {
        let old_db_path = temporary_directory(None, false);
        let old_db = open_or_create_db(old_db_path, IrysTables::ALL, None)?;

        let new_db_path = temporary_directory(None, false);
        let new_db = open_or_create_db(new_db_path, IrysTables::ALL, None)?;

        let old_version = old_db.view(|tx| crate::database_schema_version(tx).unwrap())?;

        let new_version = new_db.view(|tx| crate::database_schema_version(tx).unwrap())?;

        let write_tx = old_db.tx_mut()?;
        let key = H256::random();
        let value = 123;
        write_tx.put::<IngressProofLRU>(key, value)?;
        write_tx.commit()?;

        assert!(old_version.is_none());
        assert!(new_version.is_none());

        let old_tx = old_db.tx()?;
        let old_db_value = old_tx.get::<IngressProofLRU>(key)?;
        assert_eq!(old_db_value.unwrap(), 123);
        old_tx.commit()?;

        let new_tx = new_db.tx()?;
        let new_db_value = new_tx.get::<IngressProofLRU>(key)?;
        assert!(new_db_value.is_none());
        new_tx.commit()?;

        check_db_version_and_run_migrations_if_needed(&old_db, &new_db)?;

        let new_version = new_db.view(|tx| crate::database_schema_version(tx).unwrap())?;

        let new_tx = new_db.tx()?;
        let new_db_value = new_tx.get::<IngressProofLRU>(key)?;
        assert_eq!(new_db_value.unwrap(), 123);
        new_tx.commit()?;

        assert_eq!(new_version.unwrap(), 1);

        Ok(())
    }
}
