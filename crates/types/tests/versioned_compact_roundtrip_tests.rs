use irys_types::{
    CommitmentTransaction, Compact as _, ConsensusConfig, DataTransactionHeader, H256,
    IrysBlockHeader, VersionDiscriminant as _,
};
use rstest::rstest;

#[rstest]
#[case::mock(IrysBlockHeader::new_mock_header())]
#[case::default(IrysBlockHeader::default())]
fn block_header_compact_roundtrip(#[case] versioned: IrysBlockHeader) {
    assert_eq!(versioned.version(), 1);

    let mut buf = Vec::new();
    versioned.to_compact(&mut buf);

    assert!(!buf.is_empty());
    assert_eq!(buf[0], 1, "first byte should be the discriminant");

    let (decoded, rest) = IrysBlockHeader::from_compact(&buf, buf.len());

    assert!(rest.is_empty(), "entire buffer should be consumed");
    assert_eq!(decoded, versioned);
    assert_eq!(decoded.version(), 1);
}

#[rstest]
#[case::constructed({
    let config = ConsensusConfig::testing();
    let mut h = DataTransactionHeader::new(&config);
    h.id = H256::random();
    h
})]
#[case::default(DataTransactionHeader::default())]
fn data_tx_header_compact_roundtrip(#[case] versioned: DataTransactionHeader) {
    assert_eq!(versioned.version(), 1);

    let mut buf = Vec::new();
    versioned.to_compact(&mut buf);

    assert!(!buf.is_empty());
    assert_eq!(buf[0], 1, "first byte should be the discriminant");

    let (decoded, rest) = DataTransactionHeader::from_compact(&buf, buf.len());

    assert!(rest.is_empty(), "entire buffer should be consumed");
    assert!(
        decoded.eq_tx(&versioned),
        "decoded tx fields must match original"
    );
    assert_eq!(decoded.version(), 1);
}

#[rstest]
#[case::constructed({
    let config = ConsensusConfig::testing();
    let mut tx = CommitmentTransaction::new_stake(&config, H256::random());
    tx.set_id(H256::random());
    tx
})]
#[case::default(CommitmentTransaction::default())]
fn commitment_tx_compact_roundtrip(#[case] versioned: CommitmentTransaction) {
    assert_eq!(versioned.version(), 2);

    let mut buf = Vec::new();
    versioned.to_compact(&mut buf);

    assert!(!buf.is_empty());
    assert_eq!(buf[0], 2, "first byte should be the discriminant");

    let (decoded, rest) = CommitmentTransaction::from_compact(&buf, buf.len());

    assert!(rest.is_empty(), "entire buffer should be consumed");
    assert_eq!(decoded, versioned);
    assert_eq!(decoded.version(), 2);
}

macro_rules! unsupported_version_compact_panic_test {
    ($name:ident, $ty:ty) => {
        #[test]
        #[should_panic(expected = "UnsupportedVersion")]
        fn $name() {
            let mut buf = vec![99_u8];
            buf.extend_from_slice(&[0_u8; 100]);
            let _ = <$ty>::from_compact(&buf, buf.len());
        }
    };
}

unsupported_version_compact_panic_test!(
    block_header_from_compact_panics_on_unsupported_version,
    IrysBlockHeader
);
unsupported_version_compact_panic_test!(
    data_tx_header_from_compact_panics_on_unsupported_version,
    DataTransactionHeader
);
unsupported_version_compact_panic_test!(
    commitment_tx_from_compact_panics_on_unsupported_version,
    CommitmentTransaction
);
