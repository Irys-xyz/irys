use crate::{EmaSnapshot, EpochSnapshot};
use irys_types::{IrysBlockHeaderV1, VersionedIrysBlockHeader};
use std::sync::Arc;

pub fn dummy_epoch_snapshot() -> Arc<EpochSnapshot> {
    Arc::new(EpochSnapshot::default())
}

pub fn dummy_ema_snapshot() -> Arc<EmaSnapshot> {
    let config = irys_types::ConsensusConfig::testing();
    let genesis_header = VersionedIrysBlockHeader::V1(IrysBlockHeaderV1 {
        oracle_irys_price: config.genesis.genesis_price,
        ema_irys_price: config.genesis.genesis_price,
        ..Default::default()
    });
    EmaSnapshot::genesis(&genesis_header)
}
