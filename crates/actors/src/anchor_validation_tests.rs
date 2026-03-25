use std::sync::{Arc, RwLock};

use irys_database::{
    IrysDatabaseArgs as _, db::IrysDatabaseExt as _, insert_block_header, open_or_create_db,
    tables::IrysTables,
};
use irys_domain::{BlockTree, BlockTreeReadGuard};
use irys_testing_utils::IrysBlockHeaderTestExt as _;
use irys_types::ingress::IngressProofV1;
use irys_types::{
    ConsensusConfig, DataTransactionHeader, DatabaseProvider, H256, IngressProof, IrysBlockHeader,
};
use reth_db::mdbx::DatabaseArguments;
use rstest::rstest;

use super::{
    get_anchor_height, validate_anchor_for_inclusion, validate_ingress_proof_anchor_for_inclusion,
};
use crate::mempool_service::TxIngressError;

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

#[rstest]
#[case::at_min_boundary(10, 20, true)]
#[case::at_max_boundary(5, 10, true)]
#[case::below_min_off_by_one(11, 20, false)]
#[case::above_max_off_by_one(5, 9, false)]
#[case::in_range(5, 20, true)]
fn validate_anchor_for_inclusion_boundary(
    #[case] min_anchor_height: u64,
    #[case] max_anchor_height: u64,
    #[case] expected: bool,
) {
    let genesis = signed_genesis();
    let block_tree = test_block_tree(&genesis);
    let (_tmp, db) = test_db();

    let block_hash = H256::random();
    write_header_to_db(&db, &mock_header(10, block_hash));
    write_canonical_entry(&db, 10, block_hash);

    let mut tx = DataTransactionHeader::default();
    tx.anchor = block_hash;

    let result =
        validate_anchor_for_inclusion(&block_tree, &db, min_anchor_height, max_anchor_height, &tx)
            .unwrap();
    assert_eq!(result, expected);
}

#[rstest]
#[case::height_zero_exact_range(0, 0, true)]
#[case::height_zero_below_min(1, 10, false)]
fn validate_anchor_for_inclusion_height_zero(
    #[case] min_anchor_height: u64,
    #[case] max_anchor_height: u64,
    #[case] expected: bool,
) {
    let genesis = signed_genesis();
    let block_tree = test_block_tree(&genesis);
    let (_tmp, db) = test_db();

    let mut tx = DataTransactionHeader::default();
    tx.anchor = genesis.block_hash;

    let result =
        validate_anchor_for_inclusion(&block_tree, &db, min_anchor_height, max_anchor_height, &tx)
            .unwrap();
    assert_eq!(result, expected);
}

#[test]
fn validate_anchor_for_inclusion_unresolvable_anchor_returns_error() {
    let genesis = signed_genesis();
    let block_tree = test_block_tree(&genesis);
    let (_tmp, db) = test_db();

    let unknown_anchor = H256::random();
    let mut tx = DataTransactionHeader::default();
    tx.anchor = unknown_anchor;

    let err = validate_anchor_for_inclusion(&block_tree, &db, 5, 20, &tx).unwrap_err();
    let ingress_err = err
        .downcast_ref::<TxIngressError>()
        .expect("expected TxIngressError");
    assert!(matches!(ingress_err, TxIngressError::InvalidAnchor(a) if *a == unknown_anchor),);
}

#[rstest]
#[case::at_min_boundary(10, true)]
#[case::below_min(11, false)]
#[case::above_min(5, true)]
fn validate_ingress_proof_anchor_boundary(#[case] min_anchor_height: u64, #[case] expected: bool) {
    let genesis = signed_genesis();
    let block_tree = test_block_tree(&genesis);
    let (_tmp, db) = test_db();

    let block_hash = H256::random();
    write_header_to_db(&db, &mock_header(10, block_hash));
    write_canonical_entry(&db, 10, block_hash);

    let ingress_proof = IngressProof::V1(IngressProofV1 {
        anchor: block_hash,
        ..Default::default()
    });

    let result = validate_ingress_proof_anchor_for_inclusion(
        &block_tree,
        &db,
        min_anchor_height,
        &ingress_proof,
    )
    .unwrap();
    assert_eq!(result, expected);
}

#[rstest]
#[case::height_zero_at_min(0, true)]
#[case::height_zero_below_min(1, false)]
fn validate_ingress_proof_anchor_height_zero(
    #[case] min_anchor_height: u64,
    #[case] expected: bool,
) {
    let genesis = signed_genesis();
    let block_tree = test_block_tree(&genesis);
    let (_tmp, db) = test_db();

    let ingress_proof = IngressProof::V1(IngressProofV1 {
        anchor: genesis.block_hash,
        ..Default::default()
    });

    let result = validate_ingress_proof_anchor_for_inclusion(
        &block_tree,
        &db,
        min_anchor_height,
        &ingress_proof,
    )
    .unwrap();
    assert_eq!(result, expected);
}

#[test]
fn validate_ingress_proof_unresolvable_anchor_returns_false() {
    let genesis = signed_genesis();
    let block_tree = test_block_tree(&genesis);
    let (_tmp, db) = test_db();

    let ingress_proof = IngressProof::V1(IngressProofV1 {
        anchor: H256::random(),
        ..Default::default()
    });

    let result =
        validate_ingress_proof_anchor_for_inclusion(&block_tree, &db, 5, &ingress_proof).unwrap();
    assert!(!result);
}
