use crate::EpochSnapshot;
use irys_types::hardfork_config::IrysHardforkConfig;

/// Extension trait for hardfork checks that require EpochSnapshot context.
pub trait HardforkConfigExt {
    /// Check if UpdateRewardAddress commitments are allowed for the given epoch.
    fn is_update_reward_address_allowed_for_epoch(&self, epoch_snapshot: &EpochSnapshot) -> bool;

    /// Check if the Cascade hardfork is active for the given epoch.
    /// Activation is epoch-aligned: enabled for all blocks in an epoch if the epoch block's
    /// timestamp >= activation_timestamp.
    fn is_cascade_active_for_epoch(&self, epoch_snapshot: &EpochSnapshot) -> bool;
}

impl HardforkConfigExt for IrysHardforkConfig {
    fn is_update_reward_address_allowed_for_epoch(&self, epoch_snapshot: &EpochSnapshot) -> bool {
        let epoch_block_timestamp = epoch_snapshot.epoch_block.timestamp_secs();
        self.borealis
            .as_ref()
            .is_some_and(|f| epoch_block_timestamp >= f.activation_timestamp)
    }

    fn is_cascade_active_for_epoch(&self, epoch_snapshot: &EpochSnapshot) -> bool {
        let epoch_block_timestamp = epoch_snapshot.epoch_block.timestamp_secs();
        self.cascade
            .as_ref()
            .is_some_and(|f| epoch_block_timestamp >= f.activation_timestamp)
    }
}
