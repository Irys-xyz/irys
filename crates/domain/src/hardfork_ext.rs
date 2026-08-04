use crate::EpochSnapshot;
use irys_types::hardfork_config::{Cascade, ForEpoch, IrysHardforkConfig};

/// Extension trait for hardfork checks that require EpochSnapshot context.
pub trait HardforkConfigExt {
    /// Check if UpdateRewardAddress commitments are allowed for the given epoch.
    fn is_update_reward_address_allowed_for_epoch(&self, epoch_snapshot: &EpochSnapshot) -> bool;

    /// Cascade's activation state for the given epoch (epoch-aligned): active for
    /// all blocks in an epoch if the epoch block's timestamp >= activation_timestamp.
    ///
    /// Distinct from `cascade_for_block`: throughout the epoch in which Cascade
    /// activates, this still reads inactive while blocks' own timestamps read
    /// active. Anything that must agree with epoch processing needs the block
    /// state, not this one.
    fn cascade_for_epoch(&self, epoch_snapshot: &EpochSnapshot) -> ForEpoch<Cascade>;
}

impl HardforkConfigExt for IrysHardforkConfig {
    fn is_update_reward_address_allowed_for_epoch(&self, epoch_snapshot: &EpochSnapshot) -> bool {
        self.is_borealis_active_at(epoch_snapshot.epoch_block.timestamp_secs())
    }

    fn cascade_for_epoch(&self, epoch_snapshot: &EpochSnapshot) -> ForEpoch<Cascade> {
        ForEpoch::new(self.is_cascade_active_at(epoch_snapshot.epoch_block.timestamp_secs()))
    }
}
