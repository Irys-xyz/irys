//! Utilities for calculating ledger expiry metadata.

use serde::{Deserialize, Serialize};

/// Metadata about the submit ledger's position and expiry within its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitLedgerMetadata {
    pub epochs_remaining: u64,
    pub blocks_per_cycle: u64,
    pub position_in_cycle: u64,
    pub epoch_in_cycle: u64,
}

impl SubmitLedgerMetadata {
    pub fn new(
        block_height: u64,
        num_blocks_in_epoch: u64,
        submit_ledger_epoch_length: u64,
    ) -> Self {
        let blocks_per_cycle = submit_ledger_epoch_length * num_blocks_in_epoch;
        let position_in_cycle = block_height % blocks_per_cycle;
        let epoch_in_cycle = position_in_cycle / num_blocks_in_epoch;
        let epochs_remaining = submit_ledger_epoch_length - epoch_in_cycle;

        Self {
            epochs_remaining,
            blocks_per_cycle,
            position_in_cycle,
            epoch_in_cycle,
        }
    }
}

/// Calculates the number of epochs remaining before the submit ledger expires.
/// Returns a value from 1 to submit_ledger_epoch_length (never 0).
pub fn calculate_submit_ledger_expiry(
    block_height: u64,
    num_blocks_in_epoch: u64,
    submit_ledger_epoch_length: u64,
) -> u64 {
    let metadata = SubmitLedgerMetadata::new(
        block_height,
        num_blocks_in_epoch,
        submit_ledger_epoch_length,
    );
    metadata.epochs_remaining
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expiry_countdown() {
        // Test that expiry counts down correctly through epochs
        assert_eq!(calculate_submit_ledger_expiry(0, 100, 5), 5);
        assert_eq!(calculate_submit_ledger_expiry(100, 100, 5), 4);
        assert_eq!(calculate_submit_ledger_expiry(200, 100, 5), 3);
        assert_eq!(calculate_submit_ledger_expiry(300, 100, 5), 2);
        assert_eq!(calculate_submit_ledger_expiry(400, 100, 5), 1);
        assert_eq!(calculate_submit_ledger_expiry(500, 100, 5), 5); // Reset
    }

    #[test]
    fn test_cycle_boundary() {
        // Test behavior at cycle boundaries
        assert_eq!(calculate_submit_ledger_expiry(499, 100, 5), 1);
        assert_eq!(calculate_submit_ledger_expiry(500, 100, 5), 5);
        assert_eq!(calculate_submit_ledger_expiry(999, 100, 5), 1);
        assert_eq!(calculate_submit_ledger_expiry(1000, 100, 5), 5);
    }

    #[test]
    fn test_single_epoch_cycle() {
        // When only 1 epoch per cycle, should always return 1
        assert_eq!(calculate_submit_ledger_expiry(0, 50, 1), 1);
        assert_eq!(calculate_submit_ledger_expiry(49, 50, 1), 1);
        assert_eq!(calculate_submit_ledger_expiry(50, 50, 1), 1);
    }

    #[test]
    fn test_metadata() {
        let metadata = SubmitLedgerMetadata::new(250, 100, 5);
        assert_eq!(metadata.epochs_remaining, 3);
        assert_eq!(metadata.blocks_per_cycle, 500);
        assert_eq!(metadata.position_in_cycle, 250);
        assert_eq!(metadata.epoch_in_cycle, 2);
    }
}
