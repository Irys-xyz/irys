use irys_types::{
    versioning::VersioningError, CommitmentTransaction, DataTransactionHeader, IrysBlockHeader,
};

#[test]
fn data_tx_try_into_versioned_supported() {
    let header = DataTransactionHeader {
        version: 1,
        ..Default::default()
    };
    header.try_into_versioned().expect("v1 must be supported");
}

#[test]
fn data_tx_try_into_versioned_unsupported() {
    let header = DataTransactionHeader {
        version: 42,
        ..Default::default()
    };
    let err = header.try_into_versioned().unwrap_err();
    matches!(err, VersioningError::UnsupportedVersion(42));
}

#[test]
fn block_header_try_into_versioned_supported() {
    let header = IrysBlockHeader {
        version: 1,
        ..Default::default()
    };
    header.try_into_versioned().expect("v1 must be supported");
}

#[test]
fn block_header_try_into_versioned_unsupported() {
    let header = IrysBlockHeader {
        version: 7,
        ..Default::default()
    };
    let err = header.try_into_versioned().unwrap_err();
    matches!(err, VersioningError::UnsupportedVersion(7));
}

#[test]
fn commitment_tx_try_into_versioned_supported() {
    let tx = CommitmentTransaction {
        version: 1,
        ..Default::default()
    };
    tx.try_into_versioned().expect("v1 must be supported");
}

#[test]
fn commitment_tx_try_into_versioned_unsupported() {
    let tx = CommitmentTransaction {
        version: 3,
        ..Default::default()
    };
    let err = tx.try_into_versioned().unwrap_err();
    matches!(err, VersioningError::UnsupportedVersion(3));
}
