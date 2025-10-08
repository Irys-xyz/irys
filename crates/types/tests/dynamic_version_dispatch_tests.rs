use irys_types::{
    versioning::VersioningError, VersionedCommitmentTransaction, VersionedDataTransactionHeader, VersionedIrysBlockHeader,
    DataTransactionHeaderV1, IrysBlockHeaderV1, CommitmentTransactionV1,
};

#[test]
fn data_tx_v1_construction() {
    let header = DataTransactionHeaderV1 {
        version: 1,
        ..Default::default()
    };
    let _versioned = VersionedDataTransactionHeader::V1(header);
}

#[test]
fn data_tx_v1_default() {
    let _versioned = VersionedDataTransactionHeader::default();
    // Default should create V1 variant
}

#[test]
fn block_header_v1_construction() {
    let header = IrysBlockHeaderV1 {
        version: 1,
        ..Default::default()
    };
    let _versioned = VersionedIrysBlockHeader::V1(header);
}

#[test]
fn block_header_v1_default() {
    let _versioned = VersionedIrysBlockHeader::default();
    // Default should create V1 variant
}

#[test]
fn commitment_tx_v1_construction() {
    let tx = CommitmentTransactionV1 {
        version: 1,
        ..Default::default()
    };
    let _versioned = VersionedCommitmentTransaction::V1(tx);
}

#[test]
fn commitment_tx_v1_default() {
    let _versioned = VersionedCommitmentTransaction::default();
    // Default should create V1 variant
}
