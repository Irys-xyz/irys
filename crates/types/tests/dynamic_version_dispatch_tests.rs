use irys_types::{
    CommitmentTransactionV1, DataTransactionHeaderV1, IrysBlockHeaderV1,
    VersionedCommitmentTransaction, VersionedDataTransactionHeader, VersionedIrysBlockHeader,
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
