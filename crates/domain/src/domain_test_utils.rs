use crate::{EmaSnapshot, EpochSnapshot};
use irys_types::IrysBlockHeader;
use std::sync::Arc;

pub fn dummy_epoch_snapshot() -> Arc<EpochSnapshot> {
    Arc::new(EpochSnapshot::default())
}

pub fn dummy_ema_snapshot() -> Arc<EmaSnapshot> {
    let config = irys_types::ConsensusConfig::testnet();
    let genesis_header = IrysBlockHeader {
        oracle_irys_price: config.genesis_price,
        ema_irys_price: config.genesis_price,
        ..Default::default()
    };
    EmaSnapshot::genesis(&genesis_header)
}
