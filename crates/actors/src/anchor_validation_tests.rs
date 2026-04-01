use std::sync::{Arc, RwLock};

use irys_database::{
    IrysDatabaseArgs as _, db::IrysDatabaseExt as _, insert_block_header, open_or_create_db,
    tables::IrysTables,
};
use irys_domain::{BlockTree, BlockTreeReadGuard};
use irys_testing_utils::IrysBlockHeaderTestExt as _;
use irys_types::{ConsensusConfig, DatabaseProvider, H256, IrysBlockHeader};
use reth_db::mdbx::DatabaseArguments;

use super::get_anchor_height;

fn signed_genesis() -> IrysBlockHeader {
    let mut header = IrysBlockHeader::new_mock_header();
    header.height = 0;
    header.poa.chunk = Some(Default::default());
    header.test_sign();
    header
}

/// Creates a mock header with an arbitrary hash — valid for DB writes but not block tree.
fn mock_header(height: u64, block_hash: H256) -> IrysBlockHeader {
    let mut header = IrysBlockHeader::new_mock_header();
    header.height = height;
    header.block_hash = block_hash;
    header.poa.chunk = Some(Default::default());
    header
}

fn test_db() -> (irys_testing_utils::tempfile::TempDir, DatabaseProvider) {
    let tmp = irys_testing_utils::utils::TempDirBuilder::new().build();
    let db = open_or_create_db(
        tmp.path(),
        IrysTables::ALL,
        DatabaseArguments::irys_testing().unwrap(),
    )
    .unwrap();
    let provider = DatabaseProvider(Arc::new(db));
    (tmp, provider)
}

fn test_block_tree(genesis: &IrysBlockHeader) -> BlockTreeReadGuard {
    let cache = BlockTree::new(genesis, ConsensusConfig::testing());
    BlockTreeReadGuard::new(Arc::new(RwLock::new(cache)))
}

fn write_header_to_db(db: &DatabaseProvider, header: &IrysBlockHeader) {
    db.update_eyre(|tx| insert_block_header(tx, header))
        .unwrap();
}

fn write_canonical_entry(db: &DatabaseProvider, height: u64, block_hash: H256) {
    use irys_database::tables::MigratedBlockHashes;
    use reth_db::transaction::DbTxMut as _;
    db.update_eyre(|tx| {
        tx.put::<MigratedBlockHashes>(height, block_hash)?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn canonical_block_in_tree_returns_height() {
    let genesis = signed_genesis();
    let block_tree = test_block_tree(&genesis);
    let (_tmp, db) = test_db();

    let result = get_anchor_height(&block_tree, &db, genesis.block_hash(), true).unwrap();
    assert_eq!(result, Some(0));
}

#[test]
fn non_canonical_block_in_tree_returns_height_when_canonical_false() {
    let genesis = signed_genesis();
    let block_tree = test_block_tree(&genesis);
    let (_tmp, db) = test_db();

    let result = get_anchor_height(&block_tree, &db, genesis.block_hash(), false).unwrap();
    assert_eq!(result, Some(0));
}

#[test]
fn unknown_block_returns_none() {
    let genesis = signed_genesis();
    let block_tree = test_block_tree(&genesis);
    let (_tmp, db) = test_db();

    let result = get_anchor_height(&block_tree, &db, H256::random(), true).unwrap();
    assert_eq!(result, None);
}

#[test]
fn canonical_block_in_db_with_migrated_entry_returns_height() {
    let genesis = signed_genesis();
    let block_tree = test_block_tree(&genesis);
    let (_tmp, db) = test_db();

    let old_block_hash = H256::random();
    let old_block = mock_header(5, old_block_hash);
    write_header_to_db(&db, &old_block);
    write_canonical_entry(&db, 5, old_block_hash);

    let result = get_anchor_height(&block_tree, &db, old_block_hash, true).unwrap();
    assert_eq!(result, Some(5));
}

#[test]
fn orphan_block_in_db_without_migrated_entry_returns_none_when_canonical() {
    let genesis = signed_genesis();
    let block_tree = test_block_tree(&genesis);
    let (_tmp, db) = test_db();

    let orphan_hash = H256::random();
    let orphan_block = mock_header(5, orphan_hash);
    write_header_to_db(&db, &orphan_block);

    let result = get_anchor_height(&block_tree, &db, orphan_hash, true).unwrap();
    assert_eq!(
        result, None,
        "orphan block should not be accepted as canonical"
    );
}

#[test]
fn orphan_block_at_height_with_different_canonical_returns_none() {
    let genesis = signed_genesis();
    let block_tree = test_block_tree(&genesis);
    let (_tmp, db) = test_db();

    let orphan_hash = H256::random();
    let canonical_hash = H256::random();

    write_header_to_db(&db, &mock_header(5, orphan_hash));
    write_header_to_db(&db, &mock_header(5, canonical_hash));

    write_canonical_entry(&db, 5, canonical_hash);

    let result = get_anchor_height(&block_tree, &db, canonical_hash, true).unwrap();
    assert_eq!(result, Some(5));

    let result = get_anchor_height(&block_tree, &db, orphan_hash, true).unwrap();
    assert_eq!(result, None, "orphan should be rejected after reorg");
}

#[test]
fn orphan_block_in_db_returns_height_when_canonical_false() {
    let genesis = signed_genesis();
    let block_tree = test_block_tree(&genesis);
    let (_tmp, db) = test_db();

    let orphan_hash = H256::random();
    write_header_to_db(&db, &mock_header(5, orphan_hash));

    let result = get_anchor_height(&block_tree, &db, orphan_hash, false).unwrap();
    assert_eq!(result, Some(5));
}
