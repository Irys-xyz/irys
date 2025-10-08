use irys_types::{
    CommitmentTransaction, CommitmentTransactionV1, DataTransactionHeader, DataTransactionHeaderV1,
    IrysBlockHeader, IrysBlockHeaderV1,
};

#[test]
fn data_tx_v1_construction() {
    let header = DataTransactionHeaderV1 {
        version: 1,
        ..Default::default()
    };
    let _versioned = DataTransactionHeader::V1(header);
}

#[test]
fn data_tx_v1_default() {
    let _versioned = DataTransactionHeader::default();
    // Default should create V1 variant
}

#[test]
fn block_header_v1_construction() {
    let header = IrysBlockHeaderV1 {
        version: 1,
        ..Default::default()
    };
    let _versioned = IrysBlockHeader::V1(header);
}

#[test]
fn block_header_v1_default() {
    let _versioned = IrysBlockHeader::default();
    // Default should create V1 variant
}

#[test]
fn commitment_tx_v1_construction() {
    let tx = CommitmentTransactionV1 {
        version: 1,
        ..Default::default()
    };
    let _versioned = CommitmentTransaction::V1(tx);
}

#[test]
fn commitment_tx_v1_default() {
    let _versioned = CommitmentTransaction::default();
    // Default should create V1 variant
}
