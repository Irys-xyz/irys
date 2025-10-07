use irys_types::{
    versioning::VersioningError, CommitmentTransaction, DataTransactionHeader, IrysBlockHeader,
};

#[test]
fn data_tx_try_into_versioned_supported() {
    let mut header = DataTransactionHeader::default();
    header.version = 1;
    header.try_into_versioned().expect("v1 must be supported");
}

#[test]
fn data_tx_try_into_versioned_unsupported() {
    let mut header = DataTransactionHeader::default();
    header.version = 42;
    let err = header.try_into_versioned().unwrap_err();
    matches!(err, VersioningError::UnsupportedVersion(42));
}

#[test]
fn block_header_try_into_versioned_supported() {
    let mut header = IrysBlockHeader::default();
    header.version = 1;
    header.try_into_versioned().expect("v1 must be supported");
}

#[test]
fn block_header_try_into_versioned_unsupported() {
    let mut header = IrysBlockHeader::default();
    header.version = 7;
    let err = header.try_into_versioned().unwrap_err();
    matches!(err, VersioningError::UnsupportedVersion(7));
}

#[test]
fn commitment_tx_try_into_versioned_supported() {
    let mut tx = CommitmentTransaction::default();
    tx.version = 1;
    tx.try_into_versioned().expect("v1 must be supported");
}

#[test]
fn commitment_tx_try_into_versioned_unsupported() {
    let mut tx = CommitmentTransaction::default();
    tx.version = 3;
    let err = tx.try_into_versioned().unwrap_err();
    matches!(err, VersioningError::UnsupportedVersion(3));
}
